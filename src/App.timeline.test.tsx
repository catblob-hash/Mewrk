import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, onTestFinished, vi } from "vitest";
import App from "./App";
import { configureI18n } from "./i18n";
import type {
  AppDocument,
  ContextItem,
  ModelRunRequest,
  ToolContext
} from "./types";
import { resetAppMocks, answeredQuestionPair, documentWithModel, model, runtimeMocks } from "./test/appMocks";
import { EMPTY_TOOL_LOCK } from "./lib/toolLock";

/** The host's proofs for pasted copies of tool cards: the shared mocks do not carry this command. */
const copiedToolProofs = vi.hoisted(() => ({ attestCopiedToolContexts: vi.fn() }));
/** The host's file rewind: the shared mocks do not carry this command either. */
const rewindMocks = vi.hoisted(() => ({ rewindFiles: vi.fn() }));

vi.mock("./lib/runtime", async (importOriginal) => {
  const { runtimeMocks } = await import("./test/appMockInstances");
  return { ...await importOriginal<typeof import("./lib/runtime")>(), ...runtimeMocks, ...copiedToolProofs, ...rewindMocks };
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

/** The main conversation's timeline, where the undo keys answer. */
const mainTimeline = () => window.document.querySelector<HTMLElement>(".conversation-pane__main .context-scroll")!;

describe("App model run flow — timeline", () => {
  beforeEach(resetAppMocks);

  it("exposes the complete tool catalog in temporary-workspace model requests", async () => {
    const document = documentWithModel();
    // Tool descriptions are loaded from trusted files at runtime; a selection is only a resource ID.
    // Requests therefore contain built-in text, not the selected file's content.
    for (const workspace of document.workspaces) {
      for (const conversation of workspace.conversations) {
        conversation.settings.toolDescriptionFileId = "tooldesc_user_main_0f0f0f0f";
      }
    }
    for (const preset of document.globalSettings.conversationPresets) {
      preset.settings.toolDescriptionFileId = "tooldesc_user_main_0f0f0f0f";
    }
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue({
      contexts: [{ id: "ctx_temporary", kind: "assistant", content: "临时工作区回复", createdAt: "2026-07-11T00:00:00Z" }],
      usage: { inputTokens: 1, outputTokens: 1, totalTokens: 2 },
      model: model.id,
      providerName: "OpenAI Responses",
      durationMs: 1,
      stopReason: "completed"
    });
    const user = userEvent.setup();

    render(<App />);
    await user.click(await screen.findByRole("button", { name: `项目：${document.workspaces[0].name}` }));
    await user.click(within(screen.getByRole("menu", { name: "选择项目" })).getByRole("menuitemradio", { name: "临时项目" }));
    await user.type(screen.getByLabelText("向 Agent 发送消息"), "检查临时目录");
    await user.click(screen.getByRole("button", { name: "发送" }));

    await waitFor(() => expect(runtimeMocks.runModel).toHaveBeenCalledTimes(1));
    const request = runtimeMocks.runModel.mock.calls[0][0] as ModelRunRequest;
    expect(request).toEqual(expect.objectContaining({
      workspacePath: "",
      enabledTools: document.tools.map((tool) => tool.name)
    }));
    expect(request.tools.map((tool) => tool.name)).toEqual(document.tools.map((tool) => tool.name));
    expect(request.tools.map((tool) => tool.name)).toEqual(expect.arrayContaining(["ls", "pwsh", "powershell", "bash"]));
    expect(request.tools.find((tool) => tool.name === "ls")?.parameters[0].defaultValue).toBe(".");
    // The renderer never produces tool descriptions. Assert an empty string rather than equality with the seed, which could make matching defects pass.
    expect(request.tools.find((tool) => tool.name === "ls")?.description).toBe("");
  });

  it("sends the conversation snapshot and appends the generated response", async () => {
    const document = documentWithModel();
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue({
      contexts: [{ id: "ctx_generated", kind: "assistant", content: "模型已经回复", createdAt: "2026-07-11T00:00:00Z" }],
      usage: { inputTokens: 8, outputTokens: 6, totalTokens: 14 },
      model: model.id,
      providerName: "OpenAI Responses",
      durationMs: 42,
      stopReason: "completed"
    });

    const user = userEvent.setup();
    render(<App />);
    const composer = await screen.findByLabelText("向 Agent 发送消息");
    await user.type(composer, "请检查项目");
    await user.click(screen.getByRole("button", { name: "发送" }));

    await waitFor(() => expect(runtimeMocks.runModel).toHaveBeenCalledTimes(1));
    expect(runtimeMocks.runModel).toHaveBeenCalledWith(expect.objectContaining({
      conversationId: document.workspaces[0].conversations[0].id,
      workspacePath: document.workspaces[0].path,
      model,
      reasoningEffort: "low",
      contexts: [expect.objectContaining({ kind: "user", content: "请检查项目" })]
    }), expect.any(Function), expect.any(String));
    expect(await screen.findByText("模型已经回复")).toBeInTheDocument();
    expect(composer).toHaveValue("");
  });

  it("deletes an unanswered question from before questions blocked, and restores it with one undo", async () => {
    const document = documentWithModel();
    const { ask } = answeredQuestionPair();
    document.workspaces[0].conversations[0].contexts = [ask];
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const user = userEvent.setup();

    render(<App />);
    // An old paused question has no card to answer: it sits in the timeline.
    expect(await screen.findByText("采用哪个方案？")).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "删除提问" }));

    await waitFor(() => expect(screen.queryByText("采用哪个方案？")).not.toBeInTheDocument());
    expect(runtimeMocks.runModel).not.toHaveBeenCalled();
    // No undo control below the timeline: one line says what happened and which key takes it back.
    expect(screen.queryByRole("button", { name: /撤销删除/ })).not.toBeInTheDocument();
    const notice = screen.getByText("已删除 1 条消息").closest(".timeline-notice") as HTMLElement;
    expect(within(notice).getByText("Ctrl+Z")).toBeInTheDocument();

    await waitFor(() => expect(mainTimeline()).toHaveFocus());
    await user.keyboard("{Control>}z{/Control}");
    expect(await screen.findByText("采用哪个方案？")).toBeInTheDocument();
    expect(screen.getByText("已撤回：删除提问消息")).toBeInTheDocument();
  });

  it("answers the undo keys only while focus is in the timeline, and redoes with Ctrl+X", async () => {
    const document = documentWithModel();
    const { ask } = answeredQuestionPair();
    document.workspaces[0].conversations[0].contexts = [ask];
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const user = userEvent.setup();

    render(<App />);
    expect(await screen.findByText("采用哪个方案？")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "删除提问" }));
    await waitFor(() => expect(screen.queryByText("采用哪个方案？")).not.toBeInTheDocument());

    // The composer's own Ctrl+Z belongs to its text.
    await user.click(screen.getByLabelText("向 Agent 发送消息"));
    await user.keyboard("{Control>}z{/Control}");
    expect(screen.queryByText("采用哪个方案？")).not.toBeInTheDocument();

    mainTimeline().focus();
    await user.keyboard("{Control>}z{/Control}");
    expect(await screen.findByText("采用哪个方案？")).toBeInTheDocument();
    await user.keyboard("{Control>}x{/Control}");
    await waitFor(() => expect(screen.queryByText("采用哪个方案？")).not.toBeInTheDocument());
    expect(screen.getByText("已重做：删除提问消息")).toBeInTheDocument();
    // Both directions are spent until something else happens.
    await user.keyboard("{Control>}x{/Control}");
    expect(screen.getByText("没有可重做的修改")).toBeInTheDocument();
  });

  it("keeps each conversation's undo history to itself", async () => {
    const document = documentWithModel();
    const [first] = document.workspaces[0].conversations;
    first.contexts = [{ id: "first-reply", kind: "assistant", content: "第一段对话的回复", createdAt: "2026-07-24T00:00:00Z" }];
    const second = {
      ...structuredClone(first),
      id: "conv_second",
      title: "第二段对话",
      contexts: [{ id: "second-reply", kind: "assistant" as const, content: "第二段对话的回复", createdAt: "2026-07-24T00:00:00Z" }]
    };
    document.workspaces[0].conversations.push(second);
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const user = userEvent.setup();

    render(<App />);
    const firstReply = (await screen.findByText("第一段对话的回复")).closest("article") as HTMLElement;
    await user.click(within(firstReply).getByRole("button", { name: "删除上下文" }));
    await waitFor(() => expect(screen.queryByText("第一段对话的回复")).not.toBeInTheDocument());

    // Emptied of its only reply, the first conversation is a new task whose landing repeats this title.
    await user.click(within(screen.getByRole("navigation", { name: "对话列表" })).getByText("第二段对话"));
    expect(await screen.findByText("第二段对话的回复")).toBeInTheDocument();
    mainTimeline().focus();
    await user.keyboard("{Control>}z{/Control}");
    expect(screen.getByText("没有可撤回的修改")).toBeInTheDocument();
    expect(screen.getByText("第二段对话的回复")).toBeInTheDocument();
  });

  it("deletes and restores an answered question and answer as one timeline message", async () => {
    const document = documentWithModel();
    const { ask, answer } = answeredQuestionPair();
    document.workspaces[0].conversations[0].contexts = [ask, answer];
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const user = userEvent.setup();

    render(<App />);
    const card = (await screen.findByText("采用哪个方案？")).closest(".question-history") as HTMLElement;
    expect(card).toBeInTheDocument();
    expect(within(card).getByText("方案 A")).toBeInTheDocument();
    expect(within(card).getAllByRole("button")).toHaveLength(2);
    await user.click(within(card).getByRole("button", { name: "删除整条提问消息" }));

    await waitFor(() => expect(screen.queryByText("采用哪个方案？")).not.toBeInTheDocument());
    expect(screen.queryByText("方案 A")).not.toBeInTheDocument();

    await waitFor(() => expect(mainTimeline()).toHaveFocus());
    await user.keyboard("{Control>}z{/Control}");
    const restored = (await screen.findByText("采用哪个方案？")).closest(".question-history") as HTMLElement;
    expect(within(restored).getByText("方案 A")).toBeInTheDocument();
    expect(screen.queryByRole("dialog", { name: "需要你的回答" })).not.toBeInTheDocument();
  });

  it("deletes a tool call row and restores it from the undo entry", async () => {
    const appDocument = documentWithModel();
    const conversation = appDocument.workspaces[0].conversations[0];
    const readCall = (id: string, path: string): ToolContext => ({
      id,
      kind: "tool",
      toolName: "read",
      round: 1,
      input: { path },
      result: { success: true, output: `${path} 的内容`, executedAt: "2026-07-24T00:00:00Z", durationMs: 1 },
      createdAt: "2026-07-24T00:00:00Z"
    });
    conversation.contexts = [readCall("read-kept", "src/kept.ts"), readCall("read-deleted", "src/deleted.ts")];
    runtimeMocks.loadDocument.mockResolvedValue(appDocument);
    const user = userEvent.setup();

    render(<App />);
    const deletedRow = () => window.document.querySelector<HTMLElement>('[data-context-id="read-deleted"]');
    const row = await waitFor(() => {
      expect(deletedRow()).not.toBeNull();
      return deletedRow()!;
    });

    runtimeMocks.saveDocument.mockClear();
    await user.click(within(row).getByRole("button", { name: "删除工具调用 已读取：deleted.ts" }));

    await waitFor(() => expect(deletedRow()).toBeNull());
    await waitFor(() => expect(runtimeMocks.saveDocument.mock.calls.some(([saved]) => (
      saved.workspaces[0].conversations[0].contexts.map((context: { id: string }) => context.id).join(",")
      === "read-kept"
    ))).toBe(true));

    await waitFor(() => expect(mainTimeline()).toHaveFocus());
    await user.keyboard("{Control>}z{/Control}");
    await waitFor(() => expect(deletedRow()).not.toBeNull());
    await waitFor(() => expect(runtimeMocks.saveDocument.mock.calls.some(([saved]) => (
      saved.workspaces[0].conversations[0].contexts.map((context: { id: string }) => context.id).join(",")
      === "read-kept,read-deleted"
    ))).toBe(true));
  });

  it("asks before deleting when the preference is on, and deletes nothing unless the answer is yes", async () => {
    const appDocument = documentWithModel();
    appDocument.globalSettings.appearance = { ...appDocument.globalSettings.appearance, confirmMessageDelete: true };
    const conversation = appDocument.workspaces[0].conversations[0];
    conversation.contexts = [{
      id: "read-doomed",
      kind: "tool",
      toolName: "read",
      round: 1,
      input: { path: "src/doomed.ts" },
      result: { success: true, output: "内容", executedAt: "2026-07-24T00:00:00Z", durationMs: 1 },
      createdAt: "2026-07-24T00:00:00Z"
    } satisfies ToolContext];
    runtimeMocks.loadDocument.mockResolvedValue(appDocument);
    const user = userEvent.setup();

    render(<App />);
    const doomedRow = () => window.document.querySelector<HTMLElement>('[data-context-id="read-doomed"]');
    const row = await waitFor(() => {
      expect(doomedRow()).not.toBeNull();
      return doomedRow()!;
    });
    const deleteButton = within(row).getByRole("button", { name: "删除工具调用 已读取：doomed.ts" });

    await user.click(deleteButton);
    const question = await screen.findByRole("dialog", { name: "确定删除这条上下文？" });
    await user.click(within(question).getByRole("button", { name: "取消" }));
    expect(screen.queryByRole("dialog", { name: "确定删除这条上下文？" })).toBeNull();
    expect(doomedRow()).not.toBeNull();

    await user.click(deleteButton);
    await user.click(within(await screen.findByRole("dialog", { name: "确定删除这条上下文？" }))
      .getByRole("button", { name: "删除" }));
    await waitFor(() => expect(doomedRow()).toBeNull());
  });

  it("edits an answered question and its mapped answer in one fixed-count editor", async () => {
    const document = documentWithModel();
    const { ask, answer } = answeredQuestionPair();
    document.workspaces[0].conversations[0].contexts = [ask, answer];
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const user = userEvent.setup();

    render(<App />);
    const card = (await screen.findByText("采用哪个方案？")).closest(".question-history") as HTMLElement;
    await user.click(within(card).getByRole("button", { name: "编辑提问与回答" }));
    // The editor replaces the card's own body rather than opening over it.
    const dialog = within(card).getByRole("form", { name: "编辑提问与回答" });
    expect(within(dialog).getAllByRole("heading", { level: 3 })).toHaveLength(1);
    expect(within(dialog).queryByRole("button", { name: /增加问题|删除问题/ })).not.toBeInTheDocument();

    const prompt = within(dialog).getByLabelText("第 1 题问题内容");
    const response = within(dialog).getByLabelText("第 1 题回答");
    await user.clear(prompt);
    await user.type(prompt, "最终采用哪个方案？");
    await user.clear(response);
    await user.type(response, "方案 B，并补齐回归测试");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));

    expect(await screen.findByText("最终采用哪个方案？")).toBeInTheDocument();
    const updatedAnswer = screen.getByText("方案 B，并补齐回归测试");
    expect(updatedAnswer.closest(".question-history__output")).toBeInTheDocument();
    expect(screen.queryByText("采用哪个方案？")).not.toBeInTheDocument();
    expect(runtimeMocks.executeTool).not.toHaveBeenCalled();
  });

  it("branches a user message into an independent conversation carrying its own copy of the history", async () => {
    const document = documentWithModel();
    const conversation = document.workspaces[0].conversations[0];
    conversation.settings.securityLevel = "full_access";
    const remembered = document.workspaces[0].lastConversationSettings;
    conversation.contexts = [
      { id: "branch-sys", kind: "system", content: "系统", createdAt: "2026-07-20T00:00:00Z" },
      { id: "branch-u1", kind: "user", content: "第一问", createdAt: "2026-07-20T00:00:01Z" },
      { id: "branch-a1", kind: "assistant", content: "旧的第一答", createdAt: "2026-07-20T00:00:02Z" },
      { id: "branch-u2", kind: "user", content: "第二问", createdAt: "2026-07-20T00:00:03Z" }
    ];
    runtimeMocks.loadDocument.mockResolvedValue(document);
    // The host returns a copy with its own ids; nothing is shared by identity.
    runtimeMocks.forkConversationContexts.mockResolvedValue([
      { id: "copied-sys", kind: "system", content: "系统", createdAt: "2026-07-20T00:00:00Z" },
      { id: "copied-u1", kind: "user", content: "第一问", createdAt: "2026-07-20T00:00:01Z" },
      { id: "copied-a1", kind: "assistant", content: "旧的第一答", createdAt: "2026-07-20T00:00:02Z" }
    ]);

    const user = userEvent.setup();
    render(<App />);
    const secondQuestion = (await screen.findByText("第二问")).closest("article")!;
    await user.click(within(secondQuestion).getByRole("button", { name: "从此消息分支" }));

    // The branched message itself lands in the composer so it can be edited
    // before it is sent. Nothing runs on its own.
    const composer = await screen.findByRole("textbox", { name: "向 Agent 发送消息" });
    await waitFor(() => expect(composer).toHaveValue("第二问"));
    expect(runtimeMocks.runModel).not.toHaveBeenCalled();

    // Everything strictly before the branch point is copied by the host.
    await waitFor(() => expect(runtimeMocks.forkConversationContexts).toHaveBeenCalledTimes(1));
    expect(runtimeMocks.forkConversationContexts.mock.calls[0][0]).toMatchObject({
      sourceConversationId: conversation.id,
      throughContextId: "branch-a1"
    });

    await waitFor(() => expect(runtimeMocks.saveDocument.mock.calls.some(([snapshot]) => (
      (snapshot as AppDocument).workspaces[0].conversations.some(
        (candidate) => candidate.contexts.some((context) => context.id === "copied-a1")
      )
    ))).toBe(true));
    const saved = runtimeMocks.saveDocument.mock.calls
      .map(([snapshot]) => snapshot as AppDocument)
      .at(-1)!;
    const conversations = saved.workspaces[0].conversations;
    expect(conversations).toHaveLength(2);

    // The branch owns a full, separately persisted copy...
    const branch = conversations.find((candidate) => candidate.id !== conversation.id)!;
    expect(branch.contexts.map((context) => context.id))
      .toEqual(["copied-sys", "copied-u1", "copied-a1"]);
    // ...retries the source's work under the source's settings, without making them what the
    // project's next new task starts from...
    expect(branch.settings.securityLevel).toBe("full_access");
    expect(saved.workspaces[0].lastConversationSettings).toEqual(remembered);
    // ...remembers where it came from, yet the sidebar lists it as an ordinary conversation...
    expect(branch.parentConversationId).toBe(conversation.id);
    const parentRow = window.document.querySelector(`[data-conversation-id="${conversation.id}"]`) as HTMLElement;
    const branchRow = window.document.querySelector(`[data-conversation-id="${branch.id}"]`) as HTMLElement;
    expect(branchRow.parentElement).toBe(parentRow.parentElement);
    expect(within(parentRow).queryByRole("button", { name: "收起子会话" })).toBeNull();

    // ...and the original is untouched.
    const original = conversations.find((candidate) => candidate.id === conversation.id)!;
    expect(original.contexts.map((context) => context.id))
      .toEqual(["branch-sys", "branch-u1", "branch-a1", "branch-u2"]);
  });

  it("branches a message back into the composer as it was written, in the source's workspaces", async () => {
    const document = documentWithModel();
    const conversation = document.workspaces[0].conversations[0];
    conversation.attachedWorkspaces = [{ path: "/work/shared-lib" }];
    conversation.contexts = [
      { id: "attach-u1", kind: "user", content: "第一问", createdAt: "2026-07-20T00:00:01Z" },
      { id: "attach-a1", kind: "assistant", content: "第一答", createdAt: "2026-07-20T00:00:02Z" },
      {
        id: "attach-u2",
        kind: "user",
        content: "看这张图 [Image #1]",
        images: [{ id: "img-1", name: "shot.png", mime: "image/png", width: 4, height: 4, bytes: 64, shortId: 1 }],
        files: [{ id: "file-1", name: "notes.txt", format: "text", bytes: 5, tokens: 2 }],
        createdAt: "2026-07-20T00:00:03Z"
      } as ContextItem
    ];
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.forkConversationContexts.mockResolvedValue([
      { id: "copied-u1", kind: "user", content: "第一问", createdAt: "2026-07-20T00:00:01Z" },
      { id: "copied-a1", kind: "assistant", content: "第一答", createdAt: "2026-07-20T00:00:02Z" }
    ]);

    const user = userEvent.setup();
    render(<App />);
    const message = (await screen.findByText("看这张图")).closest("article")!;
    await user.click(within(message).getByRole("button", { name: "从此消息分支" }));

    const composer = await screen.findByRole("textbox", { name: "向 Agent 发送消息" });
    await waitFor(() => expect(composer).toHaveValue("看这张图"));
    expect(await screen.findByRole("button", { name: "移除图片 shot.png" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "移除文件 notes.txt" })).toBeInTheDocument();

    await waitFor(() => expect(runtimeMocks.saveDocument.mock.calls.some(([snapshot]) => (
      (snapshot as AppDocument).workspaces[0].conversations.length === 2
    ))).toBe(true));
    const saved = runtimeMocks.saveDocument.mock.calls.map(([snapshot]) => snapshot as AppDocument).at(-1)!;
    const branch = saved.workspaces[0].conversations.find((candidate) => candidate.id !== conversation.id)!;
    expect(branch.attachedWorkspaces).toEqual([{ path: "/work/shared-lib" }]);
  });

  it("flushes the branch before the host copy, and the host accepts that flushed target", async () => {
    const document = documentWithModel();
    const conversation = document.workspaces[0].conversations[0];
    conversation.contexts = [
      { id: "order-u1", kind: "user", content: "第一问", createdAt: "2026-07-20T00:00:01Z" },
      { id: "order-a1", kind: "assistant", content: "第一答", createdAt: "2026-07-20T00:00:02Z" },
      { id: "order-u2", kind: "user", content: "第二问", createdAt: "2026-07-20T00:00:03Z" }
    ];
    runtimeMocks.loadDocument.mockResolvedValue(document);

    // Stand in for the host guard. The renderer flushes the new conversation to
    // disk *because* the host copies from its own committed document, so by the
    // time the host runs, the target already exists and is empty. Rejecting that
    // — as the host used to — made every fork past the first message fail.
    runtimeMocks.forkConversationContexts.mockImplementation(async ({ targetConversationId }) => {
      const flushed = runtimeMocks.saveDocument.mock.calls
        .map(([snapshot]) => snapshot as AppDocument)
        .at(-1);
      const target = flushed?.workspaces[0].conversations
        .find((candidate) => candidate.id === targetConversationId);
      if (!target) throw new Error(`分支目标对话 ${targetConversationId} 尚未落盘`);
      if (target.contexts.length > 0) {
        throw new Error(`分支目标对话 ${targetConversationId} 已有历史，无法作为分支目标`);
      }
      return [
        { id: "order-copied-u1", kind: "user", content: "第一问", createdAt: "2026-07-20T00:00:01Z" },
        { id: "order-copied-a1", kind: "assistant", content: "第一答", createdAt: "2026-07-20T00:00:02Z" }
      ];
    });

    const user = userEvent.setup();
    render(<App />);
    const secondQuestion = (await screen.findByText("第二问")).closest("article")!;
    await user.click(within(secondQuestion).getByRole("button", { name: "从此消息分支" }));

    await waitFor(() => expect(runtimeMocks.forkConversationContexts).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(runtimeMocks.saveDocument.mock.calls.some(([snapshot]) => (
      (snapshot as AppDocument).workspaces[0].conversations.some(
        (candidate) => candidate.contexts.some((context) => context.id === "order-copied-a1")
      )
    ))).toBe(true));
  });

  it("forks the timeline above the insertion line into a conversation named after its origin", async () => {
    const document = documentWithModel();
    const conversation = document.workspaces[0].conversations[0];
    conversation.title = "修复登录";
    conversation.settings.enabledTools = ["read"];
    const sentAt = new Date(Date.now() - 10 * 60_000).toISOString();
    const lastRequest = { providerId: document.globalSettings.apiProviders[0].id, modelId: "test-model", at: sentAt };
    conversation.settings.toolLock = {
      ...EMPTY_TOOL_LOCK,
      tools: ["read"],
      promptSkillIds: [],
      lastRequest,
      modelRequests: [lastRequest]
    };
    conversation.contexts = [
      { id: "fork-u1", kind: "user", content: "第一问", createdAt: "2026-07-20T00:00:01Z" },
      { id: "fork-a1", kind: "assistant", content: "第一答", createdAt: "2026-07-20T00:00:02Z" },
      { id: "fork-u2", kind: "user", content: "第二问", createdAt: "2026-07-20T00:00:03Z" }
    ];
    conversation.attachedWorkspaces = [{ path: "/work/shared-lib" }];
    const worktree = {
      path: "/repo/.mewrk/worktrees/conversations/login",
      branch: "mewrk/conv/login",
      baseOid: "abc1234",
      baseBranch: "main"
    };
    conversation.worktrees = [worktree];
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.forkConversationContexts.mockImplementation(async ({ sourceContexts, throughContextId }: {
      sourceContexts: ContextItem[];
      throughContextId: string;
    }) => {
      const cut = sourceContexts.findIndex((context) => context.id === throughContextId);
      const copy = runtimeMocks.forkConversationContexts.mock.calls.length;
      return sourceContexts.slice(0, cut + 1).map((context) => ({ ...context, id: `${context.id}-copy${copy}` }));
    });
    const saved = () => runtimeMocks.saveDocument.mock.calls
      .map(([snapshot]) => snapshot as AppDocument)
      .at(-1)?.workspaces[0].conversations ?? [];
    const savedTitled = (title: string) => saved().find((candidate) => candidate.title === title);

    const user = userEvent.setup();
    render(<App />);
    const answer = (await screen.findByText("第一答")).closest("article")!;
    const header = window.document.querySelector<HTMLElement>(".topbar")!;
    // The lower half of the answer puts the line under it.
    fireEvent.contextMenu(answer, { clientX: 40, clientY: 1 });
    await user.click(within(screen.getByRole("menu")).getByRole("menuitem", { name: "分叉会话" }));

    expect(await within(header).findByRole("button", { name: "对话标题：修复登录-fork-1，点击重命名" })).toBeInTheDocument();
    await waitFor(() => expect(runtimeMocks.forkConversationContexts).toHaveBeenCalledTimes(1));
    expect(runtimeMocks.forkConversationContexts.mock.calls[0][0]).toMatchObject({
      sourceConversationId: conversation.id,
      throughContextId: "fork-a1"
    });
    await waitFor(() => expect(savedTitled("修复登录-fork-1")?.contexts.map((context) => context.id))
      .toEqual(["fork-u1-copy1", "fork-a1-copy1"]));
    const first = savedTitled("修复登录-fork-1")!;
    expect(first.forkOf).toEqual({ conversationId: conversation.id, number: 1 });
    // It carries on the source's work, so it keeps the source's settings.
    expect(first.settings.enabledTools).toEqual(["read"]);
    // And the source's cache: the same lock, down to the moment it was sent,
    // so it runs out when the source's does rather than starting over.
    expect(first.settings.toolLock?.lastRequest).toEqual(lastRequest);
    expect(first.settings.toolLock?.modelRequests).toEqual([lastRequest]);
    expect(first.settings.toolLock?.tools).toEqual(["read"]);
    // It works where the source works: the same attached workspaces, and the
    // source's worktree, shared.
    expect(first.attachedWorkspaces).toEqual([{ path: "/work/shared-lib" }]);
    expect(first.worktrees).toEqual([worktree]);
    // Nothing is handed to the composer, and the source keeps its whole timeline.
    expect(screen.getByRole("textbox", { name: "向 Agent 发送消息" })).toHaveValue("");
    expect(saved().find((candidate) => candidate.id === conversation.id)?.contexts).toHaveLength(3);

    // A fork of the fork is numbered and named under the same origin.
    fireEvent.contextMenu(window.document.querySelector(".context-stream")!, { clientX: 40, clientY: 400 });
    await user.click(within(screen.getByRole("menu")).getByRole("menuitem", { name: "分叉会话" }));
    expect(await within(header).findByRole("button", { name: "对话标题：修复登录-fork-2，点击重命名" })).toBeInTheDocument();
    await waitFor(() => expect(runtimeMocks.forkConversationContexts).toHaveBeenCalledTimes(2));
    expect(runtimeMocks.forkConversationContexts.mock.calls[1][0]).toMatchObject({
      sourceConversationId: first.id,
      throughContextId: "fork-a1-copy1"
    });
    await waitFor(() => expect(savedTitled("修复登录-fork-2")?.forkOf)
      .toEqual({ conversationId: conversation.id, number: 2 }));

    // Renaming the origin renames its forks.
    const originRow = window.document.querySelector<HTMLElement>(`[data-conversation-id="${conversation.id}"]`)!;
    await user.click(within(originRow).getByText("修复登录"));
    await user.click(within(header).getByRole("button", { name: "对话标题：修复登录，点击重命名" }));
    await user.clear(within(header).getByRole("textbox", { name: "对话标题" }));
    await user.keyboard("修复注册{Enter}");
    await waitFor(() => expect(saved().map((candidate) => candidate.title))
      .toEqual(expect.arrayContaining(["修复注册", "修复注册-fork-1", "修复注册-fork-2"])));
  });

  it("branches the first message into an empty conversation without copying history", async () => {
    const document = documentWithModel();
    const conversation = document.workspaces[0].conversations[0];
    conversation.contexts = [
      { id: "only-u1", kind: "user", content: "唯一的问题", createdAt: "2026-07-20T00:00:00Z" }
    ];
    runtimeMocks.loadDocument.mockResolvedValue(document);

    const user = userEvent.setup();
    render(<App />);
    const onlyQuestion = (await screen.findByText("唯一的问题")).closest("article")!;
    await user.click(within(onlyQuestion).getByRole("button", { name: "从此消息分支" }));

    const composer = await screen.findByRole("textbox", { name: "向 Agent 发送消息" });
    await waitFor(() => expect(composer).toHaveValue("唯一的问题"));
    // There is nothing before the branch point, so the host is never asked.
    expect(runtimeMocks.forkConversationContexts).not.toHaveBeenCalled();
  });

  it("carries the source's lock into a branch that copies history, and none into one that does not", async () => {
    const document = documentWithModel();
    const conversation = document.workspaces[0].conversations[0];
    const lastRequest = {
      providerId: document.globalSettings.apiProviders[0].id,
      modelId: "test-model",
      at: new Date(Date.now() - 5 * 60_000).toISOString()
    };
    conversation.settings.toolLock = { ...EMPTY_TOOL_LOCK, promptSkillIds: [], lastRequest, modelRequests: [lastRequest] };
    conversation.contexts = [
      { id: "lock-u1", kind: "user", content: "第一问", createdAt: "2026-07-20T00:00:01Z" },
      { id: "lock-a1", kind: "assistant", content: "第一答", createdAt: "2026-07-20T00:00:02Z" },
      { id: "lock-u2", kind: "user", content: "第二问", createdAt: "2026-07-20T00:00:03Z" }
    ];
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.forkConversationContexts.mockImplementation(async ({ sourceContexts, throughContextId }: {
      sourceContexts: ContextItem[];
      throughContextId: string;
    }) => sourceContexts
      .slice(0, sourceContexts.findIndex((context) => context.id === throughContextId) + 1)
      .map((context) => ({ ...context, id: `${context.id}-copy` })));
    const branches = () => (runtimeMocks.saveDocument.mock.calls.at(-1)?.[0] as AppDocument | undefined)
      ?.workspaces[0].conversations.filter((candidate) => candidate.parentConversationId === conversation.id) ?? [];

    const user = userEvent.setup();
    render(<App />);
    const secondQuestion = (await screen.findByText("第二问")).closest("article")!;
    await user.click(within(secondQuestion).getByRole("button", { name: "从此消息分支" }));
    await waitFor(() => expect(branches()).toHaveLength(1));
    expect(branches()[0].settings.toolLock?.lastRequest).toEqual(lastRequest);

    const originRow = window.document.querySelector<HTMLElement>(`[data-conversation-id="${conversation.id}"]`)!;
    await user.click(within(originRow).getByText(conversation.title));
    const firstQuestion = (await screen.findByText("第一问")).closest("article")!;
    await user.click(within(firstQuestion).getByRole("button", { name: "从此消息分支" }));
    await waitFor(() => expect(branches()).toHaveLength(2));
    const empty = branches().find((candidate) => candidate.contexts.length === 0);
    expect(empty?.settings.toolLock).toBeUndefined();
  });
});

describe("copying, pasting and moving on the timeline", () => {
  beforeEach(() => {
    resetAppMocks();
    copiedToolProofs.attestCopiedToolContexts.mockReset();
  });

  const at = (second: number) => `2026-10-12T00:00:0${second}Z`;
  const textMessages = (): ContextItem[] => [
    { id: "copy-1", kind: "user", content: "第一条", createdAt: at(0) },
    { id: "copy-2", kind: "assistant", content: "第二条", createdAt: at(1) },
    { id: "copy-3", kind: "user", content: "第三条", createdAt: at(2) },
    { id: "copy-4", kind: "assistant", content: "第四条", createdAt: at(3) }
  ];
  const documentHolding = (contexts: ContextItem[]) => {
    const appDocument = documentWithModel();
    const conversation = appDocument.workspaces[0].conversations[0];
    conversation.contexts = contexts;
    runtimeMocks.loadDocument.mockResolvedValue(appDocument);
    return conversation;
  };

  const rect = (top: number, height: number) => ({
    x: 0,
    y: top,
    top,
    left: 0,
    right: 600,
    bottom: top + height,
    width: 600,
    height,
    toJSON: () => ({})
  } as DOMRect);
  const timelineCards = () => Array.from(mainTimeline().querySelectorAll<HTMLElement>("[data-context-id]"));
  const card = (id: string) => mainTimeline().querySelector<HTMLElement>(`[data-context-id="${id}"]`)!;
  const order = () => timelineCards().map((element) => element.getAttribute("data-context-id") as string);
  const pickedIds = () => timelineCards()
    .filter((element) => element.hasAttribute("data-timeline-selected"))
    .map((element) => element.getAttribute("data-context-id"));
  /** jsdom has no layout: the cards are laid 100px apart down a 600px timeline, the first at 20. */
  const layOut = () => {
    const scroller = mainTimeline();
    vi.spyOn(scroller, "getBoundingClientRect").mockReturnValue(rect(0, 600));
    Object.defineProperty(scroller, "clientWidth", { configurable: true, value: 590 });
    timelineCards().forEach((element, index) => {
      vi.spyOn(element, "getBoundingClientRect").mockReturnValue(rect(20 + index * 100, 60));
    });
    return scroller;
  };
  /** The vertical middle of the nth card as `layOut` drew it. */
  const middleOf = (index: number) => 50 + index * 100;
  /** A press let go where it was made, with Ctrl held: it picks the card out or lets it go. */
  const ctrlClick = async (id: string, y: number) => {
    fireEvent.pointerDown(card(id), { button: 0, pointerId: 7, clientX: 30, clientY: y, ctrlKey: true });
    fireEvent.pointerUp(window, { pointerId: 7, clientX: 30, clientY: y, ctrlKey: true });
    // The click each press ended in is swallowed until the turn is over.
    await new Promise((resolve) => setTimeout(resolve, 0));
  };
  const press = (key: string) => fireEvent.keyDown(mainTimeline(), { key, ctrlKey: true });
  const savedContexts = () => (runtimeMocks.saveDocument.mock.calls.at(-1)?.[0] as AppDocument | undefined)
    ?.workspaces[0].conversations[0].contexts ?? [];

  it("copies two picked messages and pastes them over a third, which one undo brings back", async () => {
    documentHolding(textMessages());
    // Installs the clipboard the copy's text is written to.
    userEvent.setup();
    render(<App />);
    await screen.findByText("第四条");
    layOut();
    const original = order();
    expect(original).toEqual(["copy-1", "copy-2", "copy-3", "copy-4"]);

    await ctrlClick("copy-3", middleOf(2));
    await ctrlClick("copy-1", middleOf(0));
    expect(pickedIds()).toEqual(["copy-1", "copy-3"]);
    press("c");
    expect(await screen.findByText("已复制 2 条消息")).toBeInTheDocument();
    // Their text goes to the system clipboard too, in timeline order.
    await waitFor(async () => expect(await navigator.clipboard.readText()).toBe("第一条\n\n第三条"));

    // Copying lets nothing go; a third message is picked on its own.
    expect(pickedIds()).toEqual(["copy-1", "copy-3"]);
    fireEvent.keyDown(mainTimeline(), { key: "Escape" });
    expect(pickedIds()).toEqual([]);
    await ctrlClick("copy-2", middleOf(1));
    expect(pickedIds()).toEqual(["copy-2"]);
    press("v");

    expect(await screen.findByText("已粘贴 2 条消息")).toBeInTheDocument();
    const pasted = order();
    expect(pasted).toHaveLength(5);
    // Where the picked message sat, in timeline order, under ids of their own.
    expect(pasted[0]).toBe("copy-1");
    expect(pasted.slice(3)).toEqual(["copy-3", "copy-4"]);
    const [first, second] = pasted.slice(1, 3);
    expect([first, second]).not.toContain("copy-1");
    expect([first, second]).not.toContain("copy-3");
    expect(first).not.toBe(second);
    expect(card(first)).toHaveTextContent("第一条");
    expect(card(second)).toHaveTextContent("第三条");
    expect(screen.queryByText("第二条")).not.toBeInTheDocument();
    expect(pickedIds()).toEqual([]);
    // No tool card was copied, so the host proved nothing.
    expect(copiedToolProofs.attestCopiedToolContexts).not.toHaveBeenCalled();
    await waitFor(() => expect(savedContexts().map((context) => context.id)).toEqual(pasted));

    press("z");
    await waitFor(() => expect(order()).toEqual(original));
    expect(screen.getByText("第二条")).toBeInTheDocument();
    expect(screen.getByText("已撤回：粘贴 2 条消息")).toBeInTheDocument();
    await waitFor(() => expect(savedContexts().map((context) => context.id)).toEqual(original));
  });

  it("offers 粘贴 on the insert menu only once something was copied, and puts the copies at its line", async () => {
    documentHolding(textMessages().slice(0, 3));
    render(<App />);
    await screen.findByText("第三条");
    layOut();
    const stream = () => mainTimeline().querySelector(".context-stream")!;
    /** Between the first and second messages. */
    const rightClickBetween = () => fireEvent.contextMenu(stream(), { clientX: 30, clientY: 100 });

    rightClickBetween();
    const emptyMenu = screen.getByRole("menu", { name: "添加上下文" });
    expect(within(emptyMenu).getByRole("menuitem", { name: /粘贴/ })).toBeDisabled();
    fireEvent.keyDown(emptyMenu, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();

    await ctrlClick("copy-3", middleOf(2));
    press("c");
    expect(await screen.findByText("已复制 1 条消息")).toBeInTheDocument();
    fireEvent.keyDown(mainTimeline(), { key: "Escape" });
    expect(pickedIds()).toEqual([]);

    rightClickBetween();
    const menu = screen.getByRole("menu", { name: "添加上下文" });
    const paste = within(menu).getByRole("menuitem", { name: /粘贴/ });
    expect(paste).toBeEnabled();
    fireEvent.click(paste);

    expect(await screen.findByText("已粘贴 1 条消息")).toBeInTheDocument();
    const pasted = order();
    expect(pasted).toHaveLength(4);
    expect([pasted[0], pasted[2], pasted[3]]).toEqual(["copy-1", "copy-2", "copy-3"]);
    expect(timelineCards().map((element) => element.textContent?.match(/第[一二三]条/)?.[0]))
      .toEqual(["第一条", "第三条", "第二条", "第三条"]);
    expect(screen.queryByRole("menu")).toBeNull();
  });

  describe("a copied tool card", () => {
    const source: ToolContext = {
      id: "tool-source",
      kind: "tool",
      toolName: "read",
      round: 1,
      providerCallId: "call_provider_1",
      attestation: "proof-for-source",
      input: { path: "src/kept.ts" },
      result: { success: true, output: "src/kept.ts 的内容", executedAt: at(1), durationMs: 1 },
      createdAt: at(1)
    };

    /** Copies the tool card and opens the insert menu at the end of the timeline, ready to paste. */
    const copyToolCard = async () => {
      const conversation = documentHolding([
        { id: "tool-ask", kind: "user", content: "读一下", createdAt: at(0) },
        structuredClone(source)
      ]);
      render(<App />);
      await waitFor(() => expect(card(source.id)).toBeTruthy());
      layOut();
      await ctrlClick(source.id, middleOf(1));
      press("c");
      expect(await screen.findByText("已复制 1 条消息")).toBeInTheDocument();
      fireEvent.keyDown(mainTimeline(), { key: "Escape" });
      expect(pickedIds()).toEqual([]);
      fireEvent.contextMenu(mainTimeline().querySelector(".context-stream")!, { clientX: 30, clientY: 400 });
      return {
        conversation,
        paste: within(screen.getByRole("menu", { name: "添加上下文" })).getByRole("menuitem", { name: /粘贴/ })
      };
    };

    it("is pasted with a proof the host issued for its new id, and without the source's provider id", async () => {
      copiedToolProofs.attestCopiedToolContexts.mockResolvedValue(["proof-for-copy"]);
      const { conversation, paste } = await copyToolCard();
      fireEvent.click(paste);

      expect(await screen.findByText("已粘贴 1 条消息")).toBeInTheDocument();
      expect(copiedToolProofs.attestCopiedToolContexts).toHaveBeenCalledTimes(1);
      const request = copiedToolProofs.attestCopiedToolContexts.mock.calls[0][0];
      expect(request.conversationId).toBe(conversation.id);
      expect(request.copies).toHaveLength(1);
      const [copy] = request.copies;
      expect(copy.sourceConversationId).toBe(conversation.id);
      expect(copy.source.id).toBe(source.id);
      expect(copy.contextId).toEqual(expect.any(String));
      expect(copy.contextId).not.toBe(source.id);

      expect(order()).toEqual(["tool-ask", source.id, copy.contextId]);
      await waitFor(() => expect(savedContexts().map((context) => context.id))
        .toEqual(["tool-ask", source.id, copy.contextId]));
      const [, kept, pasted] = savedContexts() as ToolContext[];
      expect(pasted).toMatchObject({
        id: copy.contextId,
        kind: "tool",
        toolName: source.toolName,
        input: source.input,
        result: source.result,
        attestation: "proof-for-copy"
      });
      expect(pasted.providerCallId).toBeUndefined();
      // The original keeps its own proof and the provider's id for its call.
      expect(kept).toMatchObject({ attestation: "proof-for-source", providerCallId: "call_provider_1" });
    });

    it("is not pasted at all when the host refuses to vouch for it", async () => {
      copiedToolProofs.attestCopiedToolContexts.mockRejectedValue(new Error("拒绝"));
      const { paste } = await copyToolCard();
      fireEvent.click(paste);

      expect(await screen.findByText(/没有粘贴/)).toHaveTextContent("拒绝");
      expect(copiedToolProofs.attestCopiedToolContexts).toHaveBeenCalledTimes(1);
      expect(order()).toEqual(["tool-ask", source.id]);
      expect(screen.queryByText("已粘贴 1 条消息")).not.toBeInTheDocument();
      expect(savedContexts().map((context) => context.id)).toEqual(["tool-ask", source.id]);
    });
  });

  it("moves two picked messages below the last in one drag, which one undo takes back", async () => {
    documentHolding(textMessages().slice(0, 3));
    // Drag frames are run by hand, so the test decides when the pieces settle.
    const frames: FrameRequestCallback[] = [];
    const requestFrame = vi.spyOn(window, "requestAnimationFrame").mockImplementation((callback) => {
      frames.push(callback);
      return frames.length;
    });
    onTestFinished(() => requestFrame.mockRestore());
    const flush = () => act(() => {
      for (let count = 0; frames.length > 0 && count < 100; count += 1) frames.shift()?.(0);
    });
    render(<App />);
    await screen.findByText("第三条");
    layOut();
    const original = order();
    expect(original).toEqual(["copy-1", "copy-2", "copy-3"]);

    await ctrlClick("copy-1", middleOf(0));
    await ctrlClick("copy-3", middleOf(2));
    flush();
    expect(pickedIds()).toEqual(["copy-1", "copy-3"]);

    // Taken by the first of them, carried below the one that stays, and let go there.
    fireEvent.pointerDown(card("copy-1"), { button: 0, pointerId: 9, clientX: 30, clientY: middleOf(0) });
    fireEvent.pointerMove(window, { pointerId: 9, clientX: 30, clientY: 260 });
    flush();
    fireEvent.pointerUp(window, { pointerId: 9, clientX: 30, clientY: 260 });
    flush();

    expect(await screen.findByText("已移动 2 条消息")).toBeInTheDocument();
    await waitFor(() => expect(order()).toEqual(["copy-2", "copy-1", "copy-3"]));
    await waitFor(() => expect(savedContexts().map((context) => context.id)).toEqual(["copy-2", "copy-1", "copy-3"]));

    press("z");
    await waitFor(() => expect(order()).toEqual(original));
    expect(screen.getByText("已撤回：移动 2 条消息")).toBeInTheDocument();
    await waitFor(() => expect(savedContexts().map((context) => context.id)).toEqual(original));
  });
});

describe("rewinding to before a user message", () => {
  beforeEach(() => {
    resetAppMocks();
    rewindMocks.rewindFiles.mockReset();
  });

  const at = (second: number) => `2026-10-13T00:00:0${second}Z`;
  const messages = (): ContextItem[] => [
    { id: "rw-u1", kind: "user", content: "先读一下配置", createdAt: at(0) },
    { id: "rw-a1", kind: "assistant", content: "配置已读完", createdAt: at(1) },
    { id: "rw-u2", kind: "user", content: "把端口改成 8080", createdAt: at(2) },
    { id: "rw-a2", kind: "assistant", content: "端口已改成 8080", createdAt: at(3) }
  ];
  const changedFiles = [
    { path: "/work/config.json", displayPath: "config.json", action: "restore" as const, skipped: null },
    { path: "/work/notes.md", displayPath: "notes.md", action: "delete" as const, skipped: null },
    { path: "/work/locked.txt", displayPath: "locked.txt", action: "restore" as const, skipped: "文件被外部修改过" }
  ];
  const documentHolding = (contexts: ContextItem[], confirmMessageDelete = false) => {
    const appDocument = documentWithModel();
    appDocument.globalSettings.appearance = { ...appDocument.globalSettings.appearance, confirmMessageDelete };
    const conversation = appDocument.workspaces[0].conversations[0];
    conversation.contexts = contexts;
    runtimeMocks.loadDocument.mockResolvedValue(appDocument);
    return conversation;
  };
  const savedIds = () => (
    (runtimeMocks.saveDocument.mock.calls.at(-1)?.[0] as AppDocument | undefined)?.workspaces[0].conversations[0].contexts ?? []
  ).map((context) => context.id);
  const composer = () => screen.getByRole("textbox", { name: "向 Agent 发送消息" });
  const rewindCalls = () => rewindMocks.rewindFiles.mock.calls.map(([request]) => request);
  const openRewind = async (user: ReturnType<typeof userEvent.setup>, text: string) => {
    const message = (await screen.findByText(text)).closest("article") as HTMLElement;
    await user.click(within(message).getByRole("button", { name: "回退到此消息之前" }));
    const dialog = await screen.findByRole("dialog", { name: "回退到这条消息之前" });
    await within(dialog).findByRole("list");
    return dialog;
  };
  const dialogGone = () => waitFor(() => expect(screen.queryByRole("dialog", { name: "回退到这条消息之前" })).toBeNull());
  /** Previews list every file; the real restore reports the same, the locked one still skipped. */
  const hostRewinds = () => rewindMocks.rewindFiles.mockImplementation(async () => ({ files: changedFiles }));

  it("previews the files, then restores them before cutting the conversation, which one undo brings back", async () => {
    const conversation = documentHolding(messages());
    hostRewinds();
    const user = userEvent.setup();
    render(<App />);

    const dialog = await openRewind(user, "把端口改成 8080");
    expect(rewindCalls()).toEqual([{ conversationId: conversation.id, contextId: "rw-u2", dryRun: true }]);
    expect(within(dialog).getByText("将恢复 1 个文件，删除 1 个之后新建的文件；1 个会被跳过")).toBeInTheDocument();
    expect(within(dialog).getByText("config.json")).toBeInTheDocument();
    expect(within(dialog).getByText("文件被外部修改过")).toBeInTheDocument();
    // Looking changes nothing.
    expect(screen.getByText("端口已改成 8080")).toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "恢复对话和文件" }));
    await dialogGone();

    expect(rewindCalls().map((request) => request.dryRun)).toEqual([true, false]);
    expect(rewindCalls()[1]).toEqual({ conversationId: conversation.id, contextId: "rw-u2", dryRun: false });
    // The message and everything after it left the timeline, and the message is back in the composer to edit.
    await waitFor(() => expect(screen.queryByText("端口已改成 8080")).not.toBeInTheDocument());
    expect(screen.getByText("配置已读完")).toBeInTheDocument();
    expect(composer()).toHaveValue("把端口改成 8080");
    await waitFor(() => expect(savedIds()).toEqual(["rw-u1", "rw-a1"]));
    expect(runtimeMocks.runModel).not.toHaveBeenCalled();
    const notice = screen.getByText(/已回退对话和文件/).closest(".timeline-notice") as HTMLElement;
    expect(notice).toHaveTextContent("恢复了 1 个文件，删除了 1 个之后新建的文件；跳过 1 个：文件被外部修改过");
    expect(within(notice).getByText("Ctrl+Z")).toBeInTheDocument();

    // The timeline's own undo takes the conversation back; the files are the host's.
    mainTimeline().focus();
    await user.keyboard("{Control>}z{/Control}");
    expect(await screen.findByText("端口已改成 8080")).toBeInTheDocument();
    expect(screen.getByText("已撤回：回退到这条消息之前")).toBeInTheDocument();
    await waitFor(() => expect(savedIds()).toEqual(["rw-u1", "rw-a1", "rw-u2", "rw-a2"]));
    expect(rewindCalls()).toHaveLength(2);
  });

  it("restores only the files: the timeline and the composer stay as they are", async () => {
    documentHolding(messages());
    hostRewinds();
    const user = userEvent.setup();
    render(<App />);

    const dialog = await openRewind(user, "把端口改成 8080");
    runtimeMocks.saveDocument.mockClear();
    await user.click(within(dialog).getByRole("button", { name: "仅恢复文件" }));
    await dialogGone();

    expect(rewindCalls().map((request) => request.dryRun)).toEqual([true, false]);
    expect(await screen.findByText(/已回退文件/)).toHaveTextContent("恢复了 1 个文件，删除了 1 个之后新建的文件；跳过 1 个：文件被外部修改过");
    expect(screen.getByText("端口已改成 8080")).toBeInTheDocument();
    expect(composer()).toHaveValue("");
    expect(runtimeMocks.saveDocument.mock.calls.some(([saved]) => (
      (saved as AppDocument).workspaces[0].conversations[0].contexts.length < 4
    ))).toBe(false);
  });

  it("restores only the conversation without asking the host to touch a file, and without the delete confirmation", async () => {
    documentHolding(messages(), true);
    hostRewinds();
    const user = userEvent.setup();
    render(<App />);
    await user.type(await screen.findByLabelText("向 Agent 发送消息"), "顺手记一下");

    const dialog = await openRewind(user, "把端口改成 8080");
    await user.click(within(dialog).getByRole("button", { name: "仅恢复对话" }));
    await dialogGone();

    // "Confirm message delete" is for deletes: this window was the question.
    expect(screen.queryByRole("dialog", { name: "确定删除这条上下文？" })).not.toBeInTheDocument();
    await waitFor(() => expect(screen.queryByText("端口已改成 8080")).not.toBeInTheDocument());
    expect(rewindCalls().map((request) => request.dryRun)).toEqual([true]);
    // A draft the user had begun is kept, after the message.
    expect(composer()).toHaveValue("把端口改成 8080\n\n顺手记一下");
    await waitFor(() => expect(savedIds()).toEqual(["rw-u1", "rw-a1"]));
    expect(screen.getByText("已回退到这条消息之前")).toBeInTheDocument();
  });

  it("rewinds to the first message, taking the whole timeline", async () => {
    documentHolding(messages());
    rewindMocks.rewindFiles.mockResolvedValue({ files: [] });
    const user = userEvent.setup();
    render(<App />);

    const message = (await screen.findByText("先读一下配置")).closest("article") as HTMLElement;
    await user.click(within(message).getByRole("button", { name: "回退到此消息之前" }));
    const dialog = await screen.findByRole("dialog", { name: "回退到这条消息之前" });
    expect(await within(dialog).findByText("这条消息之后没有可恢复的文件改动")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "仅恢复文件" })).toBeDisabled();
    await user.click(within(dialog).getByRole("button", { name: "仅恢复对话" }));
    await dialogGone();

    await waitFor(() => expect(savedIds()).toEqual([]));
    expect(screen.queryByText("配置已读完")).not.toBeInTheDocument();
    expect(composer()).toHaveValue("先读一下配置");
  });

  it("keeps the entry off while a model turn is running", async () => {
    documentHolding(messages());
    rewindMocks.rewindFiles.mockResolvedValue({ files: [] });
    runtimeMocks.runModel.mockImplementation(() => new Promise(() => {}));
    const user = userEvent.setup();
    render(<App />);

    await screen.findByText("配置已读完");
    await user.type(screen.getByLabelText("向 Agent 发送消息"), "继续");
    await user.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(runtimeMocks.runModel).toHaveBeenCalled());

    await waitFor(() => {
      const entries = screen.getAllByRole("button", { name: "回退到此消息之前" });
      expect(entries.length).toBeGreaterThan(1);
      for (const entry of entries) expect(entry).toBeDisabled();
    });
    expect(screen.getAllByRole("button", { name: "回退到此消息之前" })[0])
      .toHaveAttribute("title", "模型回合进行中，完成或停止后才能回退");
    fireEvent.click(screen.getAllByRole("button", { name: "回退到此消息之前" })[0]);
    expect(screen.queryByRole("dialog", { name: "回退到这条消息之前" })).not.toBeInTheDocument();
    expect(rewindMocks.rewindFiles).not.toHaveBeenCalled();
  });

  it("only restores the files when a branch was taken from a message in the range, and explains why", async () => {
    const conversation = documentHolding([
      ...messages(),
      { id: "rw-u3", kind: "user", content: "再加一个日志", createdAt: at(4) },
      { id: "rw-a3", kind: "assistant", content: "日志已加", createdAt: at(5) }
    ]);
    // Two answers were tried from the second message, so it anchors both.
    conversation.branches = [
      { id: "rw-branch-old", forkContextId: "rw-u2", active: false, contexts: [], createdAt: at(2), updatedAt: at(3) },
      { id: "rw-branch-live", forkContextId: "rw-u2", active: true, contexts: [], createdAt: at(3), updatedAt: at(4) }
    ];
    hostRewinds();
    const user = userEvent.setup();
    render(<App />);

    // From the first message the range holds the anchor.
    const dialog = await openRewind(user, "先读一下配置");
    expect(within(dialog).getByText(/有分支从这条消息或其之后的消息开始，不能回退对话/)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "仅恢复对话" })).toBeDisabled();
    expect(within(dialog).getByRole("button", { name: "恢复对话和文件" })).toBeDisabled();
    expect(within(dialog).getByRole("button", { name: "仅恢复文件" })).toBeEnabled();
    runtimeMocks.saveDocument.mockClear();
    await user.click(within(dialog).getByRole("button", { name: "仅恢复文件" }));
    await dialogGone();
    expect(await screen.findByText(/已回退文件/)).toBeInTheDocument();
    expect(screen.getByText("日志已加")).toBeInTheDocument();
    expect(composer()).toHaveValue("");

    // A message after the anchor has no branch in its range and rewinds as usual.
    const later = await openRewind(user, "再加一个日志");
    expect(within(later).queryByText(/有分支从这条消息/)).not.toBeInTheDocument();
    await user.click(within(later).getByRole("button", { name: "仅恢复对话" }));
    await dialogGone();
    await waitFor(() => expect(screen.queryByText("日志已加")).not.toBeInTheDocument());
    expect(screen.getByText("端口已改成 8080")).toBeInTheDocument();
    expect(composer()).toHaveValue("再加一个日志");
  });

  it("stays open with the host's error when the files cannot be restored, and leaves the conversation alone", async () => {
    documentHolding(messages());
    rewindMocks.rewindFiles.mockImplementation(async (request: { dryRun: boolean }) => {
      if (request.dryRun) return { files: changedFiles };
      throw "写入 config.json 失败：拒绝访问";
    });
    const user = userEvent.setup();
    render(<App />);

    const dialog = await openRewind(user, "把端口改成 8080");
    await user.click(within(dialog).getByRole("button", { name: "恢复对话和文件" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent("写入 config.json 失败：拒绝访问");
    expect(screen.getByRole("dialog", { name: "回退到这条消息之前" })).toBeInTheDocument();
    expect(screen.getByText("端口已改成 8080")).toBeInTheDocument();
    expect(composer()).toHaveValue("");
    expect(within(dialog).getByRole("button", { name: "恢复对话和文件" })).toBeEnabled();
  });
});
