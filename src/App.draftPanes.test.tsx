import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { configureI18n } from "./i18n";
import { gitWorkspaceTarget } from "./lib/git";
import * as previewApi from "./lib/preview";
import type { PreviewServerSnapshot } from "./lib/preview";
import type { AppDocument } from "./types";
import {
  browserMocks,
  documentWithModel,
  gitMocks,
  openTasksPane,
  resetAppMocks,
  runtimeMocks,
  terminalMocks
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

const quietReply = {
  contexts: [],
  usage: {},
  model: "test-model",
  providerName: "",
  durationMs: 1
};

/** The fixture's project plus a second one, with a draft of its own. */
function documentWithTwoProjects(): AppDocument {
  const document = documentWithModel();
  document.workspaces.splice(1, 0, {
    ...document.workspaces[0],
    id: "ws_other",
    name: "Other",
    path: "C:/other",
    conversations: []
  });
  return document;
}

const composer = () => screen.getByLabelText("向 Agent 发送消息");
/** Opens the terminal pane from the top bar's terminal button: the row after its shells. */
async function openTerminalPane(user: ReturnType<typeof userEvent.setup>) {
  const toolbar = window.document.querySelector(".pane-toolbar") as HTMLElement;
  await user.click(within(toolbar).getByRole("button", { name: "终端" }));
  await user.click(within(await screen.findByRole("menu", { name: "新建终端" }))
    .getByRole("menuitem", { name: "显示终端面板" }));
}
/** The first terminal tab's panel, whoever owns it; the owner is part of its DOM id. */
const firstTerminalPanel = () => window.document.querySelector<HTMLElement>(
  'section[id^="conversation-terminal-"][id$="-terminal-1"]'
);
const terminalOwner = (panel: HTMLElement | null) => panel?.id
  .slice("conversation-terminal-".length, -"-terminal-1".length) ?? null;

/**
 * The id the draft's terminals and pages are opened under — the one it will be sent as — read
 * off a terminal opened for it. The pane is put away again; the terminal never starts a shell.
 */
async function draftOwnerId(user: ReturnType<typeof userEvent.setup>) {
  await openTerminalPane(user);
  const ownerId = terminalOwner(firstTerminalPanel());
  await user.click(screen.getByRole("button", { name: "关闭面板" }));
  return ownerId;
}

/** Picks another project in the new task's project chip, which goes to that project's draft. */
async function moveDraftTo(user: ReturnType<typeof userEvent.setup>, from: string, to: string) {
  await user.click(screen.getByRole("button", { name: `项目：${from}` }));
  await user.click(within(screen.getByRole("menu", { name: "选择项目" }))
    .getByRole("menuitemradio", { name: to }));
}

/** The pane toolbar's overflow row named `name`. */
async function paneMenuItem(user: ReturnType<typeof userEvent.setup>, name: string) {
  await user.click(await screen.findByRole("button", { name: "更多选项" }));
  return await screen.findByRole("menuitemradio", { name });
}

function devServer(handle: string, sessionId: string | null): PreviewServerSnapshot {
  return {
    handle,
    serverId: "dev",
    name: "dev",
    port: 5173,
    status: "running",
    startedAt: "2026-09-09T00:00:00Z",
    cwd: "C:\\test\\Mewrk",
    sessionId
  };
}

/**
 * A new task is its project's draft until it is sent, yet it has the whole workbench: its
 * terminals and preview page are opened under the id it will be sent as, so nothing moves when it
 * becomes real. Picking another project goes to that project's draft and ends nothing here.
 */
describe("the new task's panes", () => {
  beforeEach(() => {
    resetAppMocks();
    runtimeMocks.loadDocument.mockResolvedValue(documentWithTwoProjects());
    runtimeMocks.runModel.mockResolvedValue(quietReply);
  });

  it("opens its terminal under the id it is sent as, and keeps that shell across the first send", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));

    await openTerminalPane(user);
    const panel = firstTerminalPanel();
    const ownerId = terminalOwner(panel);
    expect(ownerId).toMatch(/^conv_/);
    expect(panel).not.toHaveAttribute("inert");
    fireEvent.click(within(panel!).getByText("模拟终端历史"));

    await user.type(composer(), "终端里已经在跑了");
    await user.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(runtimeMocks.runModel).toHaveBeenCalledTimes(1));

    // The conversation the draft became is the terminal's owner, and its panel is the same element:
    // nothing was detached, reopened or closed on the way.
    expect(runtimeMocks.runModel.mock.calls[0][0].conversationId).toBe(ownerId);
    expect(firstTerminalPanel()).toBe(panel);
    expect(panel).not.toHaveAttribute("inert");
    expect(terminalMocks.closeTerminal).not.toHaveBeenCalled();
  });

  it("keeps a project's draft shell running while another project's draft is on screen", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await openTerminalPane(user);
    const ownerId = terminalOwner(firstTerminalPanel());
    fireEvent.click(within(firstTerminalPanel()!).getByText("模拟终端历史"));

    // Other's draft has terminals of its own — none yet — and nothing asks or ends a thing here.
    await moveDraftTo(user, "Mewrk", "Other");
    expect(await screen.findByRole("button", { name: "项目：Other" })).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.queryAllByRole("tab")).toEqual([]);
    await openTerminalPane(user);
    const otherOwner = terminalOwner(firstTerminalPanel());
    expect(otherOwner).toMatch(/^conv_/);
    expect(otherOwner).not.toBe(ownerId);

    // Back on Mewrk's draft the shell is where it was, and sending runs under its id.
    await moveDraftTo(user, "Other", "Mewrk");
    await screen.findByRole("button", { name: "项目：Mewrk" });
    await waitFor(() => expect(terminalOwner(firstTerminalPanel())).toBe(ownerId));
    expect(terminalMocks.closeTerminal).not.toHaveBeenCalled();
    await user.type(composer(), "回到原项目");
    await user.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(runtimeMocks.runModel).toHaveBeenCalledTimes(1));
    expect(runtimeMocks.runModel.mock.calls[0][0].conversationId).toBe(ownerId);
  });

  it("shows the picked project's draft with its own panes", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await openTerminalPane(user);
    expect(screen.getAllByRole("tab")).toHaveLength(1);

    await moveDraftTo(user, "Mewrk", "Other");

    expect(await screen.findByRole("button", { name: "项目：Other" })).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.queryAllByRole("tab")).toEqual([]);
  });

  it("opens the tasks pane for a new task", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));

    expect(await openTasksPane(user)).toBeInTheDocument();
  });

  it("opens the files and history panes for a new task", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));

    const files = await paneMenuItem(user, "文件");
    expect(files).toBeEnabled();
    await user.click(files);
    expect(await screen.findByRole("region", { name: "文件" })).toBeInTheDocument();

    const history = await paneMenuItem(user, "历史记录");
    expect(history).toBeEnabled();
    await user.click(history);
    const ledger = await screen.findByRole("region", { name: "历史记录" });
    // Nothing has happened yet, so the history is simply empty.
    expect(await within(ledger).findByText(/这个对话还没有历史记录/)).toBeInTheDocument();
  });

  it("shows tasks and history as the two fixed tabs of one pane, tasks on the left", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));

    await user.click(await paneMenuItem(user, "历史记录"));
    const pane = await screen.findByRole("region", { name: "历史记录" });
    const strip = within(pane).getByRole("tablist", { name: "任务面板标签" });
    const tabs = within(strip).getAllByRole("tab");
    expect(tabs.map((tab) => tab.textContent)).toEqual(["任务", "历史记录"]);
    expect(tabs[1]).toHaveAttribute("aria-selected", "true");
    // The tabs take the title bar in place of a title, and neither one closes.
    expect(pane.querySelector(".side-pane__title")).toBeNull();
    expect(within(strip).queryByRole("button", { name: /关闭/ })).not.toBeInTheDocument();

    await user.click(tabs[0]);
    expect(await screen.findByRole("region", { name: "任务" })).toBe(pane);
    expect(within(pane).queryByText(/这个对话还没有历史记录/)).not.toBeInTheDocument();
    expect(screen.getAllByRole("region").filter((region) => region.classList.contains("side-pane"))).toHaveLength(1);

    // The menu's two rows are the pane's two tabs: the other row switches, the shown one closes.
    expect(await paneMenuItem(user, "任务")).toHaveAttribute("aria-checked", "true");
    await user.click(screen.getByRole("menuitemradio", { name: "历史记录" }));
    expect(await screen.findByRole("region", { name: "历史记录" })).toBe(pane);
    await user.click(await paneMenuItem(user, "历史记录"));
    await waitFor(() => expect(screen.queryByRole("region", { name: "历史记录" })).not.toBeInTheDocument());
    expect(screen.queryByRole("region", { name: "任务" })).not.toBeInTheDocument();
  });

  /** The pane browses any machine, so a task with no directory still has one: this computer's home. */
  it("opens the files pane for the temporary project's new task, at this computer's home", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await moveDraftTo(user, "Mewrk", "临时项目");
    await screen.findByRole("button", { name: "项目：临时项目" });

    const files = await paneMenuItem(user, "文件");
    expect(files).toBeEnabled();
    await user.click(files);
    const pane = await screen.findByRole("region", { name: "文件" });
    expect(within(pane).getByRole("button", { name: "位置：~，点按编辑" })).toBeInTheDocument();
  });
});

describe("the new task's host-backed panes", () => {
  beforeEach(() => {
    resetAppMocks();
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    runtimeMocks.loadDocument.mockResolvedValue(documentWithTwoProjects());
    runtimeMocks.runModel.mockResolvedValue(quietReply);
  });
  afterEach(() => vi.restoreAllMocks());

  it("reads Git for the project whose draft is picked", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await waitFor(() => expect(gitMocks.getGitWorkspaceSummary)
      .toHaveBeenCalledWith(gitWorkspaceTarget("ws_mewrk"), undefined));

    await moveDraftTo(user, "Mewrk", "Other");

    await waitFor(() => expect(gitMocks.getGitWorkspaceSummary)
      .toHaveBeenCalledWith(gitWorkspaceTarget("ws_other"), undefined));
  });

  it("opens its preview page under the id it is sent as, and keeps it while another project is picked", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    const ownerId = await draftOwnerId(user);

    const toolbar = window.document.querySelector(".pane-toolbar") as HTMLElement;
    await user.click(within(toolbar).getByRole("button", { name: "预览" }));
    await waitFor(() => expect(browserMocks.openBrowser).toHaveBeenCalled());
    expect(browserMocks.openBrowser.mock.calls[0]?.[0]).toBe(ownerId);

    await moveDraftTo(user, "Mewrk", "Other");
    await screen.findByRole("button", { name: "项目：Other" });
    expect(browserMocks.closeBrowserSession).not.toHaveBeenCalledWith(ownerId, expect.any(Number));
  });

  it("starts its dev servers under the id it is sent as", async () => {
    vi.spyOn(previewApi, "listPreviewConfigurations").mockResolvedValue({
      launchJsonPath: "C:\\test\\Mewrk\\.mewrk\\launch.json",
      servers: [{ name: "web", command: "npm", args: ["run", "dev"], cwd: "C:\\test\\Mewrk", port: 5173 }],
      malformed: []
    });
    vi.spyOn(previewApi, "listPreviewServers").mockResolvedValue([]);
    const start = vi.spyOn(previewApi, "startPreviewServer")
      .mockResolvedValue({ server: devServer("srv-web", null), reused: false });
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    const ownerId = await draftOwnerId(user);

    const toolbar = window.document.querySelector(".pane-toolbar") as HTMLElement;
    await user.click(within(toolbar).getByRole("button", { name: "预览" }));
    const trigger = await screen.findByRole("button", { name: "服务器与设置" });
    await waitFor(() => expect(trigger).toBeEnabled());
    await user.click(trigger);
    await user.click(within(await screen.findByRole("menu", { name: "浏览器菜单" }))
      .getByRole("menuitemradio", { name: "运行 web" }));

    // Addressed like a terminal: the id it will be sent as, the project it is aimed at, and the
    // server is that id's own.
    await waitFor(() => expect(start)
      .toHaveBeenCalledWith({ conversationId: ownerId, draftWorkspaceId: "ws_mewrk" }, "web"));
  });

  it("stops only the dev servers its draft started when its project is deleted", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "在 Other 新建任务" }));
    await screen.findByRole("button", { name: "项目：Other" });
    const ownerId = (await draftOwnerId(user))!;
    const list = vi.spyOn(previewApi, "listPreviewServers").mockResolvedValue([
      devServer("srv-mine", ownerId),
      devServer("srv-shared", null),
      devServer("srv-theirs", "conv_other")
    ]);
    const stop = vi.spyOn(previewApi, "stopPreviewServer").mockResolvedValue(true);

    const navigation = screen.getByRole("complementary", { name: "项目和对话" });
    await user.click(within(navigation).getByRole("button", { name: "删除项目 Other" }));
    await user.click(within(navigation).getByRole("button", {
      name: "确认永久删除项目 Other 及其所有任务（目录里的文件不受影响）"
    }));

    // A server nobody owns, or another conversation's, is not the draft's to stop.
    await waitFor(() => expect(stop).toHaveBeenCalledWith("srv-mine"));
    expect(stop).toHaveBeenCalledTimes(1);
    expect(list).toHaveBeenCalledWith({ conversationId: ownerId, draftWorkspaceId: "ws_other" });
    await waitFor(() => expect(within(navigation).queryByText("Other")).not.toBeInTheDocument());
  });
});
