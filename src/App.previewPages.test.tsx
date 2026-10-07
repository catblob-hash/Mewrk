import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import * as previewApi from "./lib/preview";
import type { PreviewServerSnapshot } from "./lib/preview";
import type { AppDocument } from "./types";
import { configureI18n } from "./i18n";
import { browserMocks, documentWithModel, resetAppMocks, runtimeMocks } from "./test/appMocks";

vi.mock("./lib/runtime", async (importOriginal) => {
  const { runtimeMocks } = await import("./test/appMockInstances");
  return { ...await importOriginal<typeof import("./lib/runtime")>(), ...runtimeMocks };
});
vi.mock("./lib/workspacePicker", async () => (await import("./test/appMockInstances")).workspacePickerMocks);
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

function devbox() {
  return {
    id: "machine-devbox",
    name: "devbox",
    host: "user@devbox.local",
    port: 0,
    identityFile: "",
    createdAt: "2026-01-01T00:00:00.000Z",
    updatedAt: "2026-01-01T00:00:00.000Z"
  };
}

/** A project whose second workspace is a directory on an SSH machine. */
function documentWithRemoteWorkspace(): AppDocument {
  const document = documentWithModel();
  document.globalSettings.executionEnvironments.sshMachines = [devbox()];
  document.workspaces[0].additionalWorkspaces = [
    { machine: { kind: "ssh", machineId: "machine-devbox" }, path: "/srv/api" }
  ];
  return document;
}

function toolbar(): HTMLElement {
  return window.document.querySelector(".pane-toolbar") as HTMLElement;
}

function devServer(handle: string, sessionId: string | null, port = 5173): PreviewServerSnapshot {
  return {
    handle,
    serverId: "dev",
    name: "dev",
    port,
    status: "running",
    startedAt: "2026-09-09T00:00:00Z",
    cwd: "C:\\test\\Mewrk",
    sessionId
  };
}

/** What a page reports while it shows `url`, which its tab is named after. */
function pageShowing(url: string, title: string) {
  return {
    hasPage: true,
    open: true,
    loading: false,
    url,
    title,
    canGoBack: false,
    canGoForward: false,
    zoom: 1,
    viewport: { width: 560, height: 720 }
  };
}

/**
 * Makes every read of a page — opening it, polling it, hiding it for the next tab — say what it
 * shows. A read that fell back to the blank default would make the page look as if it had
 * navigated away, and a page showing nothing is nobody's.
 */
function showPages(shown: (sessionId: string) => ReturnType<typeof pageShowing>) {
  browserMocks.openBrowser.mockImplementation(async (sessionId: string) => shown(sessionId));
  browserMocks.getBrowserStatus.mockImplementation(async (sessionId: string) => shown(sessionId));
  browserMocks.setBrowserPanelBounds.mockImplementation(async (sessionId: string) => shown(sessionId));
  browserMocks.performBrowserAction.mockImplementation(async (sessionId: string, action: string) => (
    action === "close"
      ? { ...shown(sessionId), hasPage: false, url: "" }
      : { ...shown(sessionId), open: action !== "hide" }
  ));
}

afterEach(() => configureI18n("zh-CN"));

describe("preview pages across a conversation's workspaces", () => {
  beforeEach(() => {
    resetAppMocks();
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    vi.spyOn(previewApi, "listPreviewConfigurations").mockResolvedValue({
      launchJsonPath: "/srv/api/.mewrk/launch.json",
      servers: [],
      malformed: []
    });
    vi.spyOn(previewApi, "listPreviewServers").mockResolvedValue([]);
  });

  it("keeps the top bar's button a plain toggle while there is one workspace", async () => {
    runtimeMocks.loadDocument.mockResolvedValue(documentWithModel());
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    const conversationId = (await runtimeMocks.loadDocument.mock.results[0].value as AppDocument)
      .workspaces[0].conversations[0].id;

    await user.click(within(toolbar()).getByRole("button", { name: "预览" }));

    expect(screen.queryByRole("menu", { name: "打开预览" })).not.toBeInTheDocument();
    await waitFor(() => expect(browserMocks.openBrowser).toHaveBeenCalled());
    expect(browserMocks.openBrowser.mock.calls[0]?.[0]).toBe(conversationId);
    // Bound to its workspace before the page exists, so its first request leaves from there.
    expect(browserMocks.setBrowserPageNetwork).toHaveBeenCalledWith(conversationId, { conversationId });
  });

  /**
   * The host can refuse an open (a page it is still creating, a full set of live pages). The pane
   * used to stay open with nothing in it and the button pressed, so the next click closed it and
   * only the one after that opened the page.
   */
  it("closes the pane when the host refuses the open, so one click tries again", async () => {
    runtimeMocks.loadDocument.mockResolvedValue(documentWithModel());
    browserMocks.openBrowser.mockRejectedValueOnce(
      new Error("this browser task page is being created or restored; try again shortly")
    );
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    const preview = within(toolbar()).getByRole("button", { name: "预览" });

    await user.click(preview);
    await waitFor(() => expect(browserMocks.openBrowser).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(preview).toHaveAttribute("aria-pressed", "false"));

    await user.click(preview);
    await waitFor(() => expect(browserMocks.openBrowser).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(preview).toHaveAttribute("aria-pressed", "true"));
  });

  it("opens a page for the workspace picked from the top bar, on that workspace's machine", async () => {
    const document = documentWithRemoteWorkspace();
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const conversationId = document.workspaces[0].conversations[0].id;
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");

    await user.click(within(toolbar()).getByRole("button", { name: "预览" }));
    const menu = await screen.findByRole("menu", { name: "打开预览" });
    expect(within(menu).getAllByRole("menuitem").map((item) => item.textContent))
      .toEqual(["C:\\test\\Mewrk1", "/srv/api2", "打开预览面板"]);

    await user.click(within(menu).getByRole("menuitem", { name: /^\/srv\/api/ }));

    await waitFor(() => expect(browserMocks.setBrowserPageNetwork)
      .toHaveBeenCalledWith(conversationId, { conversationId, workspace: 2 }));
    await waitFor(() => expect(browserMocks.openBrowser).toHaveBeenCalled());
    expect(browserMocks.openBrowser.mock.calls[0]?.[0]).toBe(conversationId);
    // The page reads the remote workspace's launch.json, by its number.
    await waitFor(() => expect(previewApi.listPreviewConfigurations)
      .toHaveBeenCalledWith({ conversationId, workspace: 2 }));
    const tabs = await screen.findAllByRole("tab");
    expect(tabs.map((tab) => tab.textContent)).toEqual(["2/srv/api"]);
  });

  it("adds a page from the pane's + for another workspace, as a tab of its own", async () => {
    const document = documentWithRemoteWorkspace();
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const conversationId = document.workspaces[0].conversations[0].id;
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(within(toolbar()).getByRole("button", { name: "预览" }));
    await user.click(within(await screen.findByRole("menu", { name: "打开预览" }))
      .getByRole("menuitem", { name: /^\/srv\/api/ }));
    await screen.findAllByRole("tab");

    await user.click(screen.getByRole("button", { name: "新建预览页面" }));
    const menu = await screen.findByRole("menu", { name: "为哪个工作区打开预览" });
    // The same workspaces, and no pane row: the pane is already open.
    expect(within(menu).getAllByRole("menuitem").map((item) => item.textContent))
      .toEqual(["C:\\test\\Mewrk1", "/srv/api2"]);
    await user.click(within(menu).getByRole("menuitem", { name: /^C:\\test\\Mewrk/ }));

    await waitFor(() => expect(screen.getAllByRole("tab")).toHaveLength(2));
    const second = browserMocks.setBrowserPageNetwork.mock.calls.at(-1);
    expect(second?.[0]).toMatch(new RegExp(`^${conversationId}#tab_`));
    expect(second?.[1]).toEqual({ conversationId });
    expect(screen.getAllByRole("tab").map((tab) => tab.textContent)).toEqual(["2/srv/api", "1C:\\test\\Mewrk"]);
  });

  it("files a page under the workspace whose machine the host moved it onto", async () => {
    const document = documentWithRemoteWorkspace();
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(within(toolbar()).getByRole("button", { name: "预览" }));
    await user.click(within(await screen.findByRole("menu", { name: "打开预览" }))
      .getByRole("menuitem", { name: /^C:\\test\\Mewrk/ }));
    expect((await screen.findAllByRole("tab")).map((tab) => tab.textContent)).toEqual(["1C:\\test\\Mewrk"]);

    // The model's preview_start ran a server in workspace 2 and put the page on devbox's network.
    browserMocks.getBrowserStatus.mockResolvedValue({
      hasPage: true,
      open: true,
      loading: false,
      url: "http://localhost:5173/",
      title: "Vite App",
      canGoBack: false,
      canGoForward: false,
      zoom: 1,
      viewport: { width: 560, height: 720 },
      networkMachine: "ssh:machine-devbox"
    });

    await waitFor(() => expect(screen.getAllByRole("tab").map((tab) => tab.textContent))
      .toEqual(["2Vite App"]), { timeout: 3000 });
  });

  it("opens the chip-selected workspace's start page when there is no page yet", async () => {
    const document = documentWithRemoteWorkspace();
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const conversationId = document.workspaces[0].conversations[0].id;
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: /^工作区：/ }));
    await user.click(within(await screen.findByRole("menu", { name: "选择工作区" }))
      .getByRole("menuitemradio", { name: /^\/srv\/api/ }));

    await user.click(within(toolbar()).getByRole("button", { name: "预览" }));
    await user.click(within(await screen.findByRole("menu", { name: "打开预览" }))
      .getByRole("menuitem", { name: "打开预览面板" }));

    await waitFor(() => expect(browserMocks.setBrowserPageNetwork)
      .toHaveBeenCalledWith(conversationId, { conversationId, workspace: 2 }));
    expect((await screen.findAllByRole("tab")).map((tab) => tab.textContent)).toEqual(["2/srv/api"]);
  });
});

/**
 * A tab is how the user ends a page, and a dev server the conversation started for it is not left
 * running behind nothing. Only that conversation's own server is the tab's to stop, and only when
 * no other page is still showing it.
 */
describe("closing a preview page's tab", () => {
  const WEB = "http://localhost:5173/";
  const API = "http://localhost:4000/";
  let conversationId: string;

  beforeEach(() => {
    resetAppMocks();
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    const document = documentWithModel();
    conversationId = document.workspaces[0].conversations[0].id;
    runtimeMocks.loadDocument.mockResolvedValue(document);
    vi.spyOn(previewApi, "listPreviewConfigurations").mockResolvedValue({
      launchJsonPath: "C:\\test\\Mewrk\\.mewrk\\launch.json",
      servers: [],
      malformed: []
    });
  });
  afterEach(() => vi.restoreAllMocks());

  /** Lists `servers` and opens the preview pane, whose first page is the conversation's own. */
  async function openPreviewWith(user: ReturnType<typeof userEvent.setup>, servers: PreviewServerSnapshot[]) {
    const list = vi.spyOn(previewApi, "listPreviewServers").mockResolvedValue(servers);
    const stop = vi.spyOn(previewApi, "stopPreviewServer").mockResolvedValue(true);
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await waitFor(() => expect(list).toHaveBeenCalled());
    // The close reads the list the task bar holds, so it has to have landed before a tab closes.
    await list.mock.results[0].value;
    await user.click(within(toolbar()).getByRole("button", { name: "预览" }));
    return stop;
  }

  const closeTab = async (user: ReturnType<typeof userEvent.setup>, title: string) => {
    await user.click(await screen.findByRole("button", { name: `关闭页面 ${title}` }));
    await waitFor(() => expect(screen.queryByRole("tab", { name: new RegExp(`^${title}$`) }))
      .not.toBeInTheDocument());
  };

  it("stops the conversation's server the closed page was showing", async () => {
    showPages(() => pageShowing(WEB, "Vite App"));
    const user = userEvent.setup();
    const stop = await openPreviewWith(user, [devServer("srv-web", conversationId)]);
    await screen.findByRole("tab", { name: "Vite App" });
    expect(stop).not.toHaveBeenCalled();

    await closeTab(user, "Vite App");

    await waitFor(() => expect(stop).toHaveBeenCalledWith("srv-web"));
    expect(stop).toHaveBeenCalledTimes(1);
    expect(browserMocks.closeBrowserSession).toHaveBeenCalledWith(conversationId, expect.any(Number));
  });

  it("leaves the server running while another page of the conversation still shows it", async () => {
    // Two tabs on one server: a route and the page after it share its origin.
    showPages((sessionId) => (
      sessionId === conversationId ? pageShowing(WEB, "Vite App") : pageShowing(`${WEB}about`, "Vite about")
    ));
    const user = userEvent.setup();
    const stop = await openPreviewWith(user, [devServer("srv-web", conversationId)]);
    await screen.findByRole("tab", { name: "Vite App" });
    await user.click(screen.getByRole("button", { name: "新建预览页面" }));
    await screen.findByRole("tab", { name: "Vite about" });

    await closeTab(user, "Vite about");

    // The page that is left is the server's last one, and closing that is what stops it.
    expect(stop).not.toHaveBeenCalled();
    await closeTab(user, "Vite App");
    await waitFor(() => expect(stop).toHaveBeenCalledWith("srv-web"));
    expect(stop).toHaveBeenCalledTimes(1);
  });

  it.each([
    ["no conversation owns it", null],
    ["another conversation owns it", "conv_other"]
  ])("leaves a server alone when %s", async (_owner, owner) => {
    // The first page shows a server that is not the conversation's; the second shows its own.
    showPages((sessionId) => (
      sessionId === conversationId ? pageShowing(WEB, "Vite App") : pageShowing(API, "API docs")
    ));
    const user = userEvent.setup();
    const stop = await openPreviewWith(user, [
      devServer("srv-web", owner),
      devServer("srv-api", conversationId, 4000)
    ]);
    await screen.findByRole("tab", { name: "Vite App" });
    await user.click(screen.getByRole("button", { name: "新建预览页面" }));
    await screen.findByRole("tab", { name: "API docs" });

    await closeTab(user, "Vite App");

    expect(stop).not.toHaveBeenCalled();
    await closeTab(user, "API docs");
    // The other server was the conversation's own, so the same close does reach it.
    await waitFor(() => expect(stop).toHaveBeenCalledWith("srv-api"));
    expect(stop).toHaveBeenCalledTimes(1);
  });
});
