import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ComponentProps, ReactNode } from "react";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createTestDocument } from "../../test/fixtures";
import { CLAUDE_AGENT_LEGAL_URL, ensureClaudeAgentProvider } from "../../lib/claudeAgentProvider";
import { normalizeDocument } from "../../lib/runtime";
import type {
  ApiProvider,
  ClaudeAgentComponentStatus,
  ClaudeAgentLoginStatus,
  GlobalSettings as GlobalSettingsType
} from "../../types";
import { GlobalSettings } from "../GlobalSettings";
import { SettingsSessionProvider } from "../settingsSession";

const runtimeMocks = vi.hoisted(() => ({
  deleteApiKey: vi.fn(),
  saveApiKey: vi.fn(),
  getStoredApiKeyLength: vi.fn(),
  forgetStoredApiKeyLength: vi.fn(),
  revealApiKey: vi.fn(),
  fetchModels: vi.fn(),
  claudeAgentLoginStatus: vi.fn(),
  claudeAgentOpenLogin: vi.fn(),
  claudeAgentComponentStatus: vi.fn(),
  installClaudeAgentComponent: vi.fn(),
  cancelClaudeAgentComponentInstall: vi.fn()
}));

// The settings pages pick the desktop or the preview path from this; the preview is the
// default and the session tests below switch to the desktop.
const backendState = vi.hoisted(() => ({ connected: false }));
vi.mock("../../lib/backend", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/backend")>()),
  hasBackendRuntime: () => backendState.connected
}));

// Only the credential/discovery/login calls are stubbed; the fixtures still need
// the real defaults helpers this module also exports.
vi.mock("../../lib/runtime", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/runtime")>()),
  ...runtimeMocks
}));

import { ClaudeAgentComponentPanel } from "./ClaudeAgentComponentPanel";
import { ClaudeAgentLoginPanel } from "./ClaudeAgentLoginPanel";

// Discovery delegates to the unmocked module so the assertion covers the real
// preview catalog rather than a list the test wrote itself.
const actualRuntime = await vi.importActual<typeof import("../../lib/runtime")>("../../lib/runtime");

const signedOut: ClaudeAgentLoginStatus = {
  signedIn: false,
  authMethod: "none",
  email: null,
  orgName: null,
  subscriptionType: null,
  executable: "/Applications/Mewrk.app/Contents/Resources/claude",
  configDir: "/home/me/.claude",
  loginCommand: "'/Applications/Mewrk.app/Contents/Resources/claude' auth login"
};

const signedIn: ClaudeAgentLoginStatus = {
  signedIn: true,
  authMethod: "claude.ai",
  email: "person@example.com",
  orgName: "Example Org",
  subscriptionType: "max",
  executable: "/Applications/Mewrk.app/Contents/Resources/claude",
  configDir: "/home/me/.claude",
  loginCommand: "'/Applications/Mewrk.app/Contents/Resources/claude' auth login"
};

/** The host's answer once the SDK and Claude Code are installed and nothing newer is out. */
function componentStatus(overrides: Partial<ClaudeAgentComponentStatus> = {}): ClaudeAgentComponentStatus {
  return {
    installed: {
      sdkVersion: "0.3.284",
      claudeCodeVersion: "2.1.261",
      source: "installed",
      installedAt: "2026-10-07T08:00:00Z", compatible: true
    },
    compatible: "^0.3.284",
    latest: { sdkVersion: "0.3.284", claudeCodeVersion: "2.1.261" },
    latestError: null,
    newerIncompatible: null,
    updateAvailable: false,
    task: null,
    lastError: null,
    ...overrides
  };
}

const notInstalled = componentStatus({ installed: null });

/**
 * The host answering a status read the way it does by default: a read that does not
 * check npm (`checkLatest` false) knows what is installed and nothing about newer
 * versions, so `latest` is empty until a check has run.
 */
function hostComponentStatus(checkLatest: boolean): Promise<ClaudeAgentComponentStatus> {
  return Promise.resolve(checkLatest ? componentStatus() : componentStatus({ latest: null }));
}

/** The `checkLatest` argument of every component status read so far, in order. */
function componentReads(): boolean[] {
  return runtimeMocks.claudeAgentComponentStatus.mock.calls.map(([checkLatest]) => checkLatest as boolean);
}

function runningTask(
  overrides: Partial<NonNullable<ClaudeAgentComponentStatus["task"]>> = {}
): NonNullable<ClaudeAgentComponentStatus["task"]> {
  return {
    action: "install",
    sdkVersion: "0.3.284",
    phase: "downloading",
    receivedBytes: 0,
    totalBytes: null,
    ...overrides
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((nextResolve, nextReject) => {
    resolve = nextResolve;
    reject = nextReject;
  });
  return { promise, resolve, reject };
}

/**
 * One settings session around whatever `page` renders. "Leave" and "Return" show and
 * hide the page inside the same session (switching to another provider and back);
 * "Reopen settings" starts a new session (closing the dialog and opening it again).
 */
function SessionHarness({ page }: { page: () => ReactNode }) {
  const [shown, setShown] = useState(true);
  const [opening, setOpening] = useState(0);
  return (
    <>
      <button type="button" onClick={() => setShown((current) => !current)}>{shown ? "Leave" : "Return"}</button>
      <button type="button" onClick={() => setOpening((current) => current + 1)}>Reopen settings</button>
      <SettingsSessionProvider key={opening}>{shown && page()}</SettingsSessionProvider>
    </>
  );
}

function renderProviders() {
  // Normalization is what puts the built-in rows in place, so the fixture goes
  // through it rather than hand-placing a Claude Agent row the product owns.
  const document = normalizeDocument(createTestDocument());
  let currentSettings = document.globalSettings;
  function Harness() {
    const [settings, setSettings] = useState<GlobalSettingsType>(document.globalSettings);
    currentSettings = settings;
    return (
      <GlobalSettings
        initialView="providers"
        settings={settings}
        onChange={setSettings}
        onFlush={vi.fn(() => Promise.resolve())}
      />
    );
  }
  return { ...render(<Harness />), getSettings: () => currentSettings };
}

function claudeAgentProvider(): ApiProvider {
  return {
    id: "provider_claude_agent",
    name: "Claude Agent",
    enabled: false,
    family: "claude_agent",
    baseUrl: "",
    familySettings: {},
    notes: "",
    models: [],
    activeModelId: null
  };
}

function renderPanel(overrides: Partial<ComponentProps<typeof ClaudeAgentLoginPanel>> = {}) {
  const onSignedInChange = vi.fn();
  return {
    onSignedInChange,
    ...render(<ClaudeAgentLoginPanel
      provider={claudeAgentProvider()}
      desktopRuntime
      onSignedInChange={onSignedInChange}
      {...overrides}
    />)
  };
}

/**
 * The Claude Agent family is a built-in row like Codex: normalization ensures
 * exactly one, it is not offered in the add dialog, and it cannot be deleted or
 * repurposed into another family.
 */
describe("Claude Agent provider", () => {
  beforeEach(() => {
    runtimeMocks.deleteApiKey.mockReset().mockResolvedValue({ configured: false });
    runtimeMocks.saveApiKey.mockReset().mockResolvedValue({ configured: true, keyLength: 8 });
    runtimeMocks.getStoredApiKeyLength.mockReset().mockResolvedValue(undefined);
    runtimeMocks.forgetStoredApiKeyLength.mockReset().mockResolvedValue(undefined);
    runtimeMocks.revealApiKey.mockReset().mockResolvedValue("stored-secret-key");
    runtimeMocks.fetchModels.mockReset().mockImplementation(actualRuntime.fetchModels);
    runtimeMocks.claudeAgentLoginStatus.mockReset().mockResolvedValue(signedOut);
    runtimeMocks.claudeAgentOpenLogin.mockReset().mockResolvedValue(undefined);
    runtimeMocks.claudeAgentComponentStatus.mockReset().mockResolvedValue(componentStatus());
    runtimeMocks.installClaudeAgentComponent.mockReset().mockResolvedValue(undefined);
    runtimeMocks.cancelClaudeAgentComponentInstall.mockReset().mockResolvedValue(undefined);
    backendState.connected = false;
  });

  it("is a built-in row that the add dialog does not offer", async () => {
    const user = userEvent.setup();
    const { getSettings } = renderProviders();

    expect(getSettings().apiProviders.filter((provider) => provider.family === "claude_agent"))
      .toHaveLength(1);

    await user.click(screen.getByRole("button", { name: "添加提供商" }));
    const dialog = screen.getByRole("dialog", { name: "添加提供商" });
    const protocols = within(dialog).getByLabelText("对话协议");
    expect(within(protocols).queryByRole("option", { name: /Claude Agent/u })).not.toBeInTheDocument();
  });

  it("removes legacy executable settings without cloning clean settings", () => {
    const legacy = claudeAgentProvider();
    legacy.familySettings = { claude_executable: "C:\\Users\\me\\.local\\bin\\claude.exe" } as typeof legacy.familySettings;
    const cleaned = ensureClaudeAgentProvider([legacy]);
    expect(cleaned[0]?.familySettings).toEqual({});
    const clean = claudeAgentProvider();
    const unchanged = ensureClaudeAgentProvider([clean]);
    expect(unchanged[0]).toBe(clean);
  });

  it("cannot be deleted or repurposed into another family", async () => {
    const user = userEvent.setup();
    renderProviders();

    await user.click(screen.getByRole("button", { name: "Claude Agent" }));
    // The built-in row carries no delete button.
    expect(screen.queryByRole("button", { name: "删除 Claude Agent" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "提供商设置" }));
    const drawer = screen.getByRole("dialog", { name: "提供商设置" });
    const protocol = within(drawer).getByLabelText("API 格式");
    expect(protocol).toBeDisabled();
    expect(within(protocol).getByRole("option", { name: "Claude Agent (Claude Code)" })).toBeInTheDocument();
    expect(within(protocol).queryByRole("option", { name: "Anthropic Messages" })).not.toBeInTheDocument();
  });

  it("offers neither a key, address, nor identity fields", async () => {
    const user = userEvent.setup();
    const { getSettings } = renderProviders();

    await user.click(screen.getByRole("button", { name: "Claude Agent" }));

    expect(getSettings().apiProviders.find((provider) => provider.family === "claude_agent"))
      .toMatchObject({
        name: "Claude Agent",
        family: "claude_agent",
        // The CLI owns both the credential and the endpoint.
        baseUrl: "",
        enabled: false
      });

    expect(screen.queryByLabelText("API Key")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("API 地址")).not.toBeInTheDocument();
    expect(screen.queryByText("API 地址")).not.toBeInTheDocument();
    // The compliance stance is on the panel itself, not buried in a drawer.
    expect(screen.getByRole("link", { name: "Claude Code 使用条款" }))
      .toHaveAttribute("href", CLAUDE_AGENT_LEGAL_URL);

    // Claude Code is bundled by Mewrk, so this provider has no identity fields.
    await user.click(screen.getByRole("button", { name: "提供商设置" }));
    const drawer = screen.getByRole("dialog", { name: "提供商设置" });
    expect(within(drawer).queryByLabelText(/Claude Code.*路径/u)).not.toBeInTheDocument();
    expect(within(drawer).queryByText("身份字段")).not.toBeInTheDocument();
    // No address of any kind, not even the non-chat endpoints.
    expect(within(drawer).queryByText("端点地址")).not.toBeInTheDocument();
  });

  it("tells the browser preview it cannot read the sign-in status", async () => {
    const user = userEvent.setup();
    renderProviders();

    await user.click(screen.getByRole("button", { name: "Claude Agent" }));

    expect(screen.getByText("浏览器预览无法读取 Claude Code 登录状态。")).toBeInTheDocument();
    expect(screen.getByText("浏览器预览无法管理 Claude Agent 组件。")).toBeInTheDocument();
    // Asking a preview would only produce a bridge error.
    expect(runtimeMocks.claudeAgentLoginStatus).not.toHaveBeenCalled();
    expect(runtimeMocks.claudeAgentComponentStatus).not.toHaveBeenCalled();
  });

  it("discovers placeholder rows in the browser preview", async () => {
    const user = userEvent.setup();
    const { getSettings } = renderProviders();

    await user.click(screen.getByRole("button", { name: "Claude Agent" }));
    await user.click(screen.getByRole("button", { name: "拉取模型" }));

    // The preview has no CLI to ask, so its discovery page lists placeholder
    // rows; installing all of them is what turns the list into the provider's models.
    expect(await screen.findByRole("button", { name: "添加到提供商 claude-opus-preview" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "添加全部结果" }));

    await waitFor(() => expect(
      getSettings().apiProviders.find((provider) => provider.family === "claude_agent")!.models
        .map((model) => model.id)
        .sort()
    ).toEqual(["claude-opus-preview", "claude-sonnet-preview"]));
  });
});

describe("ClaudeAgentLoginPanel", () => {
  beforeEach(() => {
    runtimeMocks.claudeAgentLoginStatus.mockReset().mockResolvedValue(signedOut);
    runtimeMocks.claudeAgentOpenLogin.mockReset().mockResolvedValue(undefined);
  });

  /* Mewrk drives its own Claude Code, so the command shown and copied names that
     executable by path, never a `claude` the user may not have installed. */
  it("shows the bundled CLI's sign-in command while the CLI is signed out", async () => {
    const user = userEvent.setup();
    renderPanel();

    expect(await screen.findByText(signedOut.loginCommand)).toBeInTheDocument();
    expect(screen.queryByText("claude auth login")).not.toBeInTheDocument();
    expect(screen.getByText(/两者共用同一份登录/u)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "打开终端登录" }));
    await waitFor(() => expect(runtimeMocks.claudeAgentOpenLogin)
      .toHaveBeenCalledWith(expect.objectContaining({ id: "provider_claude_agent" })));
  });

  it("shows who the CLI is signed in as", async () => {
    runtimeMocks.claudeAgentLoginStatus.mockResolvedValue(signedIn);
    renderPanel();

    expect(await screen.findByText("已登录 Claude Code")).toBeInTheDocument();
    expect(screen.getByText("person@example.com · Example Org · Max")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "打开终端登录" })).not.toBeInTheDocument();
  });

  /// A Console login is billed per token, which is a materially different deal
  /// from a subscription and must not read the same.
  it("names a Console login as API billing", async () => {
    runtimeMocks.claudeAgentLoginStatus.mockResolvedValue({
      ...signedIn,
      authMethod: "console",
      orgName: null,
      subscriptionType: null
    });
    renderPanel();

    expect(await screen.findByText("person@example.com · Console（API 计费）")).toBeInTheDocument();
  });

  it("enables the provider when a re-check finds a completed sign-in", async () => {
    const user = userEvent.setup();
    runtimeMocks.claudeAgentLoginStatus.mockResolvedValueOnce(signedOut).mockResolvedValue(signedIn);
    const { onSignedInChange } = renderPanel();

    await screen.findByRole("button", { name: "打开终端登录" });
    expect(onSignedInChange).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "重新检查" }));
    expect(await screen.findByText("已登录 Claude Code")).toBeInTheDocument();
    await waitFor(() => expect(onSignedInChange).toHaveBeenCalledWith(true));
  });

  /// Coming back from the terminal the sign-in ran in is when the answer changes,
  /// so the window's focus after it lost focus asks again — and the panel stays up
  /// while it does, instead of dropping to "loading" on every focus.
  it("re-checks when the window regains focus, keeping the panel up until the answer is in", async () => {
    const { onSignedInChange } = renderPanel();
    await screen.findByRole("button", { name: "打开终端登录" });
    let answer!: (status: ClaudeAgentLoginStatus) => void;
    runtimeMocks.claudeAgentLoginStatus.mockReturnValueOnce(new Promise<ClaudeAgentLoginStatus>((resolve) => {
      answer = resolve;
    }));

    fireEvent.blur(window);
    fireEvent.focus(window);

    await waitFor(() => expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(2));
    expect(screen.getByRole("button", { name: "打开终端登录" })).toBeInTheDocument();
    expect(screen.queryByText("正在读取登录状态…")).not.toBeInTheDocument();
    expect(onSignedInChange).not.toHaveBeenCalled();

    await act(async () => answer(signedIn));
    expect(await screen.findByText("已登录 Claude Code")).toBeInTheDocument();
    expect(onSignedInChange).toHaveBeenCalledWith(true);
  });

  /// An already-signed-in status on first load is not a transition: the user may
  /// have disabled the row deliberately, so the panel must not re-enable it.
  it("does not touch the enabled flag for a CLI that was already signed in", async () => {
    runtimeMocks.claudeAgentLoginStatus.mockResolvedValue(signedIn);
    const { onSignedInChange } = renderPanel();

    expect(await screen.findByText("已登录 Claude Code")).toBeInTheDocument();
    expect(onSignedInChange).not.toHaveBeenCalled();
  });

  /// Installing the components from this page is the user asking for the provider:
  /// a login the freshly installed CLI finds enables the row.
  it("enables the provider when the first read after a fresh install finds a login", async () => {
    runtimeMocks.claudeAgentLoginStatus.mockResolvedValue(signedIn);
    const { onSignedInChange } = renderPanel({ enableWhenSignedIn: true });

    expect(await screen.findByText("已登录 Claude Code")).toBeInTheDocument();
    expect(onSignedInChange).toHaveBeenCalledWith(true);
  });

  it("leaves the provider alone when the first read after a fresh install finds no login", async () => {
    const { onSignedInChange } = renderPanel({ enableWhenSignedIn: true });

    expect(await screen.findByRole("button", { name: "打开终端登录" })).toBeInTheDocument();
    expect(onSignedInChange).not.toHaveBeenCalled();
  });

  it("offers a retry and explains bundled-install or config-read failures", async () => {
    const user = userEvent.setup();
    runtimeMocks.claudeAgentLoginStatus
      .mockRejectedValueOnce(new Error("找不到 claude 可执行文件"))
      .mockResolvedValue(signedOut);
    renderPanel();

    expect(await screen.findByRole("alert")).toHaveTextContent("找不到 claude 可执行文件");
    expect(screen.getByText(/Mewrk 用的是上面安装的 Claude Code；读不到通常是安装不完整/u)).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "重试" }));
    expect(await screen.findByRole("button", { name: "打开终端登录" })).toBeEnabled();
  });

  it("does not ask the host in a browser preview", async () => {
    renderPanel({ desktopRuntime: false });

    expect(await screen.findByText("浏览器预览无法读取 Claude Code 登录状态。")).toBeInTheDocument();
    expect(runtimeMocks.claudeAgentLoginStatus).not.toHaveBeenCalled();
  });

  it("flushes pending document edits before every host call", async () => {
    const user = userEvent.setup();
    const onBeforeHostCall = vi.fn().mockResolvedValue(undefined);
    renderPanel({ onBeforeHostCall });

    await screen.findByRole("button", { name: "打开终端登录" });
    expect(onBeforeHostCall).toHaveBeenCalledTimes(1);

    await user.click(screen.getByRole("button", { name: "打开终端登录" }));
    await waitFor(() => expect(runtimeMocks.claudeAgentOpenLogin).toHaveBeenCalledTimes(1));
    expect(onBeforeHostCall).toHaveBeenCalledTimes(2);
    expect(onBeforeHostCall.mock.invocationCallOrder[1]).toBeLessThan(
      runtimeMocks.claudeAgentOpenLogin.mock.invocationCallOrder[0]
    );
  });

  /// A copy that fails (the document lost focus) and is retried must leave only
  /// the outcome of the retry on screen: no stale alert beside a "copied" label.
  it("clears a copy failure once a retry succeeds", async () => {
    const user = userEvent.setup();
    const writeText = vi.fn()
      .mockRejectedValueOnce(new Error("Document is not focused."))
      .mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    try {
      renderPanel();
      const copy = await screen.findByRole("button", { name: "复制命令" });

      await user.click(copy);
      expect(await screen.findByRole("alert")).toHaveTextContent("复制命令失败：Document is not focused.");
      expect(screen.queryByRole("button", { name: "已复制" })).not.toBeInTheDocument();

      await user.click(screen.getByRole("button", { name: "复制命令" }));
      expect(await screen.findByRole("button", { name: "已复制" })).toBeInTheDocument();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      expect(writeText).toHaveBeenLastCalledWith(signedOut.loginCommand);
    } finally {
      vi.unstubAllGlobals();
    }
  });
});

/// The refresh rule: a page asks the host the first time it is shown in a settings
/// session and when the window comes back; switching away and back does not ask again.
describe("ClaudeAgentLoginPanel refresh rule", () => {
  beforeEach(() => {
    runtimeMocks.claudeAgentLoginStatus.mockReset().mockResolvedValue(signedOut);
    runtimeMocks.claudeAgentOpenLogin.mockReset().mockResolvedValue(undefined);
  });

  const page = () => (
    <ClaudeAgentLoginPanel
      provider={claudeAgentProvider()}
      desktopRuntime
      onSignedInChange={vi.fn()}
    />
  );

  it("does not ask again when the page is shown again in the same session", async () => {
    const user = userEvent.setup();
    render(<SessionHarness page={page} />);
    await screen.findByRole("button", { name: "打开终端登录" });
    expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(1);

    await user.click(screen.getByRole("button", { name: "Leave" }));
    expect(screen.queryByRole("button", { name: "打开终端登录" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Return" }));

    // Shown from the session at once, not after another read.
    expect(screen.getByRole("button", { name: "打开终端登录" })).toBeInTheDocument();
    expect(screen.queryByText("正在读取登录状态…")).not.toBeInTheDocument();
    expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(1);
  });

  it("asks afresh when the settings window is opened again", async () => {
    const user = userEvent.setup();
    render(<SessionHarness page={page} />);
    await screen.findByRole("button", { name: "打开终端登录" });

    await user.click(screen.getByRole("button", { name: "Reopen settings" }));

    await waitFor(() => expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(2));
    expect(await screen.findByRole("button", { name: "打开终端登录" })).toBeInTheDocument();
  });

  it("joins a first read that is still running when the page is shown again", async () => {
    const user = userEvent.setup();
    const first = deferred<ClaudeAgentLoginStatus>();
    runtimeMocks.claudeAgentLoginStatus.mockReturnValueOnce(first.promise);
    render(<SessionHarness page={page} />);
    await waitFor(() => expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(1));

    await user.click(screen.getByRole("button", { name: "Leave" }));
    await user.click(screen.getByRole("button", { name: "Return" }));
    await act(async () => first.resolve(signedOut));

    expect(await screen.findByRole("button", { name: "打开终端登录" })).toBeInTheDocument();
    expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(1);
  });

  it("asks again when a read failed, because nothing was loaded", async () => {
    const user = userEvent.setup();
    runtimeMocks.claudeAgentLoginStatus.mockRejectedValueOnce(new Error("boom")).mockResolvedValue(signedOut);
    render(<SessionHarness page={page} />);
    expect(await screen.findByRole("alert")).toHaveTextContent("boom");

    await user.click(screen.getByRole("button", { name: "Leave" }));
    await user.click(screen.getByRole("button", { name: "Return" }));

    expect(await screen.findByRole("button", { name: "打开终端登录" })).toBeInTheDocument();
    expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(2);
  });

  it("does not re-check on a focus that no blur preceded", async () => {
    renderPanel();
    await screen.findByRole("button", { name: "打开终端登录" });

    fireEvent.focus(window);
    await act(async () => undefined);

    expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(1);
  });

  it("swallows a failed focus re-check and keeps what it showed", async () => {
    renderPanel();
    await screen.findByRole("button", { name: "打开终端登录" });
    runtimeMocks.claudeAgentLoginStatus.mockRejectedValueOnce(new Error("timed out"));

    fireEvent.blur(window);
    fireEvent.focus(window);
    await waitFor(() => expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(2));
    await act(async () => undefined);

    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "打开终端登录" })).toBeInTheDocument();
  });

  it("reads again, keeping the panel up, when the components panel reports a finished install", async () => {
    const onSignedInChange = vi.fn();
    const props = { provider: claudeAgentProvider(), desktopRuntime: true, onSignedInChange };
    const { rerender } = render(<ClaudeAgentLoginPanel {...props} refreshToken={0} />);
    await screen.findByRole("button", { name: "打开终端登录" });
    const answer = deferred<ClaudeAgentLoginStatus>();
    runtimeMocks.claudeAgentLoginStatus.mockReturnValueOnce(answer.promise);

    rerender(<ClaudeAgentLoginPanel {...props} refreshToken={1} />);

    await waitFor(() => expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(2));
    expect(screen.getByRole("button", { name: "打开终端登录" })).toBeInTheDocument();
    await act(async () => answer.resolve(signedIn));
    expect(await screen.findByText("已登录 Claude Code")).toBeInTheDocument();
    expect(onSignedInChange).toHaveBeenCalledWith(true);
  });
});

/**
 * The SDK and Claude Code are downloaded from npm on this page rather than shipped in
 * the installer, so the page installs, updates and reports on them, and shows the
 * sign-in only once they are there.
 */
describe("ClaudeAgentComponentPanel", () => {
  beforeEach(() => {
    runtimeMocks.claudeAgentComponentStatus.mockReset().mockImplementation(hostComponentStatus);
    runtimeMocks.installClaudeAgentComponent.mockReset().mockResolvedValue(undefined);
    runtimeMocks.cancelClaudeAgentComponentInstall.mockReset().mockResolvedValue(undefined);
  });

  const page = (desktopRuntime = true) => (
    <ClaudeAgentComponentPanel desktopRuntime={desktopRuntime}>
      {({ revision, freshInstall }) => (
        <p data-testid="sign-in">{`sign-in panel ${revision}${freshInstall ? " fresh" : ""}`}</p>
      )}
    </ClaudeAgentComponentPanel>
  );

  function renderComponents() {
    return render(<SessionHarness page={() => page()} />);
  }

  it("lists what is installed at once and checks npm after it, the first time it is shown", async () => {
    const npm = deferred<ClaudeAgentComponentStatus>();
    runtimeMocks.claudeAgentComponentStatus
      // From disk: the versions, and nothing known about npm yet.
      .mockResolvedValueOnce(componentStatus({ latest: null }))
      // The npm check, which stays out until the test lets it answer.
      .mockReturnValueOnce(npm.promise);
    renderComponents();

    // The versions are on screen while npm has not answered, and the check is said to run.
    expect(await screen.findByText("0.3.284")).toBeInTheDocument();
    expect(screen.getByText("2.1.261")).toBeInTheDocument();
    expect(screen.getByText("正在检查更新…")).toBeInTheDocument();
    expect(screen.queryByText("已是最新的兼容版本。")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "检查更新" })).toBeDisabled();
    expect(screen.getByTestId("sign-in")).toHaveTextContent("sign-in panel 0");
    // Disk first, npm second.
    expect(componentReads()).toEqual([false, true]);

    await act(async () => npm.resolve(componentStatus()));

    expect(await screen.findByText("已是最新的兼容版本。")).toBeInTheDocument();
    expect(screen.queryByText("正在检查更新…")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "检查更新" })).toBeEnabled();
    // Up to date: nothing to install or update, and the sign-in follows the components.
    expect(screen.queryByRole("button", { name: "安装" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "更新" })).not.toBeInTheDocument();
    expect(screen.getByTestId("sign-in")).toHaveTextContent("sign-in panel 0");
    // The answer did not start another read.
    expect(componentReads()).toEqual([false, true]);
  });

  it("explains the download and hides the sign-in while nothing is installed", async () => {
    runtimeMocks.claudeAgentComponentStatus.mockResolvedValue(notInstalled);
    renderComponents();

    expect(await screen.findByText(/从 npm 下载 Anthropic 发布的官方包/u)).toBeInTheDocument();
    expect(screen.getByText("将安装 SDK 0.3.284（Claude Code 2.1.261）。")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "安装" })).toBeEnabled();
    expect(screen.queryByTestId("sign-in")).not.toBeInTheDocument();
  });

  it("installs on request, follows the progress, and brings up the sign-in once it is installed", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      runtimeMocks.claudeAgentComponentStatus
        // First read, from disk: nothing installed, nothing known about npm.
        .mockResolvedValueOnce(componentStatus({ installed: null, latest: null }))
        // Then the npm check.
        .mockResolvedValueOnce(notInstalled)
        // Read right after the install started: the host has not asked npm yet.
        .mockResolvedValueOnce(componentStatus({
          installed: null,
          task: runningTask({ phase: "resolving" })
        }))
        // Two polls: halfway through the download, then over.
        .mockResolvedValueOnce(componentStatus({
          installed: null,
          task: runningTask({ receivedBytes: 10_485_760, totalBytes: 41_943_040 })
        }))
        .mockResolvedValueOnce(componentStatus({ latest: null }))
        // The re-read once it ended.
        .mockResolvedValue(componentStatus());
      renderComponents();
      // The npm check has answered: it is what names the version to install.
      await screen.findByText("将安装 SDK 0.3.284（Claude Code 2.1.261）。");
      await screen.findByRole("button", { name: "安装" });
      expect(componentReads()).toEqual([false, true]);

      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "安装" }));
      });
      expect(runtimeMocks.installClaudeAgentComponent).toHaveBeenCalledWith(null);
      expect(screen.getByText("正在查找版本…")).toBeInTheDocument();
      // The size is not known yet: the bar has no value.
      expect(screen.getByRole("progressbar")).not.toHaveAttribute("aria-valuenow");
      expect(screen.queryByRole("button", { name: "安装" })).not.toBeInTheDocument();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(500);
      });
      expect(screen.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "25");
      expect(screen.getByText("10.0 MB / 40.0 MB · 25%")).toBeInTheDocument();
      expect(screen.getByText("正在下载…")).toBeInTheDocument();
      expect(runtimeMocks.claudeAgentComponentStatus).toHaveBeenLastCalledWith(false);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(500);
      });
      expect(screen.queryByRole("progressbar")).not.toBeInTheDocument();
      // Over: the status is read once more, npm included.
      expect(runtimeMocks.claudeAgentComponentStatus).toHaveBeenLastCalledWith(true);
      expect(componentReads()).toEqual([false, true, false, false, false, true]);
      expect(await screen.findByText("0.3.284")).toBeInTheDocument();
      // The sign-in appeared with the install and reads for the first time: no re-read to
      // signal, but it is told the install was a first one, so a login it finds enables the row.
      expect(screen.getByTestId("sign-in")).toHaveTextContent("sign-in panel 0 fresh");
    } finally {
      vi.useRealTimers();
    }
  });

  it("tells the sign-in panel to read again when an update of an installed copy is over", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const newest = { sdkVersion: "0.3.290", claudeCodeVersion: "2.1.270" };
      runtimeMocks.claudeAgentComponentStatus
        // First read, from disk: what is installed, nothing known about npm.
        .mockResolvedValueOnce(componentStatus({ latest: null }))
        // Then the npm check finds a newer version.
        .mockResolvedValueOnce(componentStatus({ latest: newest, updateAvailable: true }))
        // Read right after the update started.
        .mockResolvedValueOnce(componentStatus({
          latest: newest,
          updateAvailable: true,
          task: runningTask({ action: "update", sdkVersion: "0.3.290", phase: "installing" })
        }))
        // The first poll finds the task over, with the new copy in place.
        .mockResolvedValueOnce(componentStatus({
          installed: { sdkVersion: "0.3.290", claudeCodeVersion: "2.1.270", source: "installed", installedAt: null, compatible: true },
          latest: null
        }))
        .mockResolvedValue(componentStatus({
          installed: { sdkVersion: "0.3.290", claudeCodeVersion: "2.1.270", source: "installed", installedAt: null, compatible: true },
          latest: newest
        }));
      renderComponents();
      // Only the npm check can say an update is available.
      await screen.findByText("有新版本：SDK 0.3.290（Claude Code 2.1.270）");
      await screen.findByRole("button", { name: "更新" });
      expect(componentReads()).toEqual([false, true]);
      expect(screen.getByTestId("sign-in")).toHaveTextContent("sign-in panel 0");

      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "更新" }));
      });
      expect(screen.getByTestId("sign-in")).toHaveTextContent("sign-in panel 0");
      await act(async () => {
        await vi.advanceTimersByTimeAsync(500);
      });

      expect(componentReads()).toEqual([false, true, false, false, true]);
      expect(await screen.findByText("0.3.290")).toBeInTheDocument();
      expect(screen.getByTestId("sign-in")).toHaveTextContent("sign-in panel 1");
      expect(screen.getByText("已是最新的兼容版本。")).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "更新" })).not.toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("offers an update when a newer compatible version is out, and starts it", async () => {
    runtimeMocks.claudeAgentComponentStatus.mockResolvedValue(componentStatus({
      latest: { sdkVersion: "0.3.290", claudeCodeVersion: "2.1.270" },
      updateAvailable: true
    }));
    renderComponents();

    expect(await screen.findByText("有新版本：SDK 0.3.290（Claude Code 2.1.270）")).toBeInTheDocument();
    runtimeMocks.claudeAgentComponentStatus.mockResolvedValue(componentStatus({
      latest: { sdkVersion: "0.3.290", claudeCodeVersion: "2.1.270" },
      updateAvailable: true,
      task: runningTask({ action: "update", sdkVersion: "0.3.290", phase: "installing" })
    }));
    await userEvent.setup().click(screen.getByRole("button", { name: "更新" }));

    await waitFor(() => expect(runtimeMocks.installClaudeAgentComponent).toHaveBeenCalledWith(null));
    expect(await screen.findByText("正在更新到 SDK 0.3.290")).toBeInTheDocument();
    expect(screen.getByText("正在安装…")).toBeInTheDocument();
    // The installed copy keeps working while the update runs, so its sign-in stays up.
    expect(screen.getByTestId("sign-in")).toBeInTheDocument();
  });

  it("asks the host to cancel a running task", async () => {
    const user = userEvent.setup();
    runtimeMocks.claudeAgentComponentStatus.mockResolvedValue(componentStatus({
      installed: null,
      task: runningTask({ receivedBytes: 1024, totalBytes: null })
    }));
    renderComponents();

    // An unknown total is shown as bytes so far on an indeterminate bar.
    expect(await screen.findByText("1.0 KB")).toBeInTheDocument();
    expect(screen.getByRole("progressbar")).not.toHaveAttribute("aria-valuenow");
    await user.click(screen.getByRole("button", { name: "取消" }));

    await waitFor(() => expect(runtimeMocks.cancelClaudeAgentComponentInstall).toHaveBeenCalledTimes(1));
    expect(screen.getByRole("button", { name: "取消" })).toBeDisabled();
  });

  it("shows a failed install with a Retry that starts it again", async () => {
    const user = userEvent.setup();
    runtimeMocks.claudeAgentComponentStatus.mockResolvedValue({
      ...notInstalled,
      lastError: "下载中断：连接被重置"
    });
    renderComponents();

    expect(await screen.findByRole("alert")).toHaveTextContent("安装失败：下载中断：连接被重置");
    await user.click(screen.getByRole("button", { name: "重试" }));

    await waitFor(() => expect(runtimeMocks.installClaudeAgentComponent).toHaveBeenCalledWith(null));
  });

  it("reports a refused install start", async () => {
    const user = userEvent.setup();
    runtimeMocks.claudeAgentComponentStatus.mockResolvedValue(notInstalled);
    runtimeMocks.installClaudeAgentComponent.mockRejectedValue(new Error("已有安装任务在进行"));
    renderComponents();

    await user.click(await screen.findByRole("button", { name: "安装" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("已有安装任务在进行");
    expect(screen.getByRole("button", { name: "安装" })).toBeEnabled();
  });

  it("says a newer version needs a newer Mewrk, and states a failed npm check softly", async () => {
    runtimeMocks.claudeAgentComponentStatus.mockResolvedValue(componentStatus({
      latest: null,
      latestError: "无法连接 npm 镜像",
      newerIncompatible: "0.4.0"
    }));
    renderComponents();

    expect(await screen.findByText(/npm 上已有 SDK 0\.4\.0，但这个版本的 Mewrk 只兼容 \^0\.3\.284/u)).toBeInTheDocument();
    expect(screen.getByText("没能检查新版本：无法连接 npm 镜像")).toBeInTheDocument();
    // An npm check that failed is not a failure of the installed components.
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("says an installed copy outside the supported range must be updated, and offers the update", async () => {
    const incompatible = componentStatus({
      installed: { sdkVersion: "0.3.280", claudeCodeVersion: "2.1.280", source: "installed", installedAt: null, compatible: false },
      latest: { sdkVersion: "0.3.292", claudeCodeVersion: "2.1.292" },
      updateAvailable: true
    });
    runtimeMocks.claudeAgentComponentStatus.mockResolvedValue(incompatible);
    renderComponents();

    expect(await screen.findByText(/已安装的版本不在当前 AI SDK 组件支持的范围（\^0\.3\.284）内/u)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "更新" })).toBeEnabled();
  });

  it("marks a development copy", async () => {
    runtimeMocks.claudeAgentComponentStatus.mockResolvedValue(componentStatus({
      installed: { sdkVersion: "0.3.284", claudeCodeVersion: "2.1.261", source: "development", installedAt: null, compatible: true }
    }));
    renderComponents();

    expect(await screen.findByText("开发版本")).toBeInTheDocument();
    expect(screen.getByText(/用的是源码目录里的 SDK 和 Claude Code/u)).toBeInTheDocument();
  });

  it("offers a Retry when the first read fails", async () => {
    const user = userEvent.setup();
    runtimeMocks.claudeAgentComponentStatus.mockRejectedValueOnce(new Error("host unavailable"));
    renderComponents();

    expect(await screen.findByRole("alert")).toHaveTextContent("host unavailable");
    expect(screen.getByText("读取组件状态失败。")).toBeInTheDocument();
    // The read from disk failed, so there was nothing to check npm about.
    expect(componentReads()).toEqual([false]);
    await user.click(screen.getByRole("button", { name: "重试" }));

    expect(await screen.findByText("0.3.284")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(componentReads()).toEqual([false, true]);
  });

  it("checks again on request, with a spinner on the button while it runs", async () => {
    const user = userEvent.setup();
    renderComponents();
    // The first check has answered, so the button is free.
    await screen.findByText("已是最新的兼容版本。");
    expect(componentReads()).toEqual([false, true]);
    const answer = deferred<ClaudeAgentComponentStatus>();
    runtimeMocks.claudeAgentComponentStatus.mockReturnValueOnce(answer.promise);

    await user.click(screen.getByRole("button", { name: "检查更新" }));

    expect(screen.getByRole("button", { name: "检查更新" })).toBeDisabled();
    expect(componentReads()).toEqual([false, true, true]);
    await act(async () => answer.resolve(componentStatus({
      installed: { sdkVersion: "0.3.285", claudeCodeVersion: "2.1.262", source: "installed", installedAt: null, compatible: true },
      latest: { sdkVersion: "0.3.285", claudeCodeVersion: "2.1.262" }
    })));
    expect(await screen.findByText("0.3.285")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "检查更新" })).toBeEnabled();
  });

  describe("session", () => {
    it("does not ask again when it is shown again in the same session", async () => {
      const user = userEvent.setup();
      renderComponents();
      // The first showing: what is installed, then the npm check, which has answered here.
      await screen.findByText("已是最新的兼容版本。");
      expect(componentReads()).toEqual([false, true]);

      await user.click(screen.getByRole("button", { name: "Leave" }));
      expect(screen.queryByText("0.3.284")).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Return" }));

      // From the session, not from another read: the versions and the verdict are on screen at once.
      expect(screen.getByText("0.3.284")).toBeInTheDocument();
      expect(screen.getByText("已是最新的兼容版本。")).toBeInTheDocument();
      expect(screen.queryByText("正在检查更新…")).not.toBeInTheDocument();
      expect(screen.getByTestId("sign-in")).toBeInTheDocument();
      await act(async () => undefined);
      expect(componentReads()).toEqual([false, true]);
    });

    it("checks npm again when the page was left before its check answered, and not once it has", async () => {
      const user = userEvent.setup();
      const abandoned = deferred<ClaudeAgentComponentStatus>();
      const second = deferred<ClaudeAgentComponentStatus>();
      runtimeMocks.claudeAgentComponentStatus
        // From disk, shown at once.
        .mockResolvedValueOnce(componentStatus({ latest: null }))
        // The first check, still out when the page is left.
        .mockReturnValueOnce(abandoned.promise)
        // The check of the next showing.
        .mockReturnValueOnce(second.promise);
      renderComponents();
      expect(await screen.findByText("0.3.284")).toBeInTheDocument();
      expect(screen.getByText("正在检查更新…")).toBeInTheDocument();
      expect(componentReads()).toEqual([false, true]);

      await user.click(screen.getByRole("button", { name: "Leave" }));
      // The page is gone when its check answers: nobody takes the answer up, and it does
      // not count as the session's check.
      await act(async () => abandoned.resolve(componentStatus()));
      await user.click(screen.getByRole("button", { name: "Return" }));

      // What the session has is what the page last showed: the versions, no verdict yet.
      expect(screen.getByText("0.3.284")).toBeInTheDocument();
      await waitFor(() => expect(componentReads()).toEqual([false, true, true]));
      expect(screen.getByText("正在检查更新…")).toBeInTheDocument();
      expect(screen.queryByText("已是最新的兼容版本。")).not.toBeInTheDocument();

      await act(async () => second.resolve(componentStatus()));
      expect(await screen.findByText("已是最新的兼容版本。")).toBeInTheDocument();
      expect(screen.queryByText("正在检查更新…")).not.toBeInTheDocument();

      // This time the check answered: showing the page again asks nothing.
      await user.click(screen.getByRole("button", { name: "Leave" }));
      await user.click(screen.getByRole("button", { name: "Return" }));
      expect(screen.getByText("已是最新的兼容版本。")).toBeInTheDocument();
      expect(screen.queryByText("正在检查更新…")).not.toBeInTheDocument();
      await act(async () => undefined);
      expect(componentReads()).toEqual([false, true, true]);
    });

    it("asks afresh when the settings window is opened again", async () => {
      const user = userEvent.setup();
      renderComponents();
      await screen.findByText("已是最新的兼容版本。");
      expect(componentReads()).toEqual([false, true]);
      runtimeMocks.claudeAgentComponentStatus.mockResolvedValue(componentStatus({
        installed: { sdkVersion: "0.3.290", claudeCodeVersion: "2.1.270", source: "installed", installedAt: null, compatible: true }
      }));

      await user.click(screen.getByRole("button", { name: "Reopen settings" }));

      expect(await screen.findByText("0.3.290")).toBeInTheDocument();
      // A new session starts over: disk, then npm.
      await waitFor(() => expect(componentReads()).toEqual([false, true, false, true]));
      expect(await screen.findByText("已是最新的兼容版本。")).toBeInTheDocument();
      expect(componentReads()).toEqual([false, true, false, true]);
    });

    it("keeps following a task that was running when the page was left", async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true });
      try {
        const running = componentStatus({ installed: null, task: runningTask({ receivedBytes: 1024 }) });
        runtimeMocks.claudeAgentComponentStatus
          // From disk, then the npm check: the task is running through both.
          .mockResolvedValueOnce(running)
          .mockResolvedValueOnce(running)
          // While the page was away the task ended; the first poll after coming back finds out.
          .mockResolvedValueOnce(componentStatus())
          .mockResolvedValue(componentStatus());
        renderComponents();
        await screen.findByRole("progressbar");
        // The check has answered before the page is left, so coming back asks nothing but the polls.
        await waitFor(() => expect(componentReads()).toEqual([false, true]));
        await act(async () => undefined);

        await act(async () => {
          fireEvent.click(screen.getByRole("button", { name: "Leave" }));
        });
        await act(async () => {
          fireEvent.click(screen.getByRole("button", { name: "Return" }));
        });
        // The session remembers the running task, so progress is back on screen at once.
        expect(screen.getByRole("progressbar")).toBeInTheDocument();
        await act(async () => {
          await vi.advanceTimersByTimeAsync(500);
        });

        expect(screen.queryByRole("progressbar")).not.toBeInTheDocument();
        expect(await screen.findByText("0.3.284")).toBeInTheDocument();
        // Showing the page again asked nothing; the poll found the task over, and the status was read once more, npm included.
        expect(componentReads()).toEqual([false, true, false, true]);
      } finally {
        vi.useRealTimers();
      }
    });
  });

  describe("window focus", () => {
    it("reads again on returning to the window, keeping what is shown until the answer is in", async () => {
      renderComponents();
      // After the first showing's two reads, the second being the npm check.
      await screen.findByText("已是最新的兼容版本。");
      expect(componentReads()).toEqual([false, true]);
      const answer = deferred<ClaudeAgentComponentStatus>();
      runtimeMocks.claudeAgentComponentStatus.mockReturnValueOnce(answer.promise);

      fireEvent.blur(window);
      fireEvent.focus(window);

      await waitFor(() => expect(componentReads()).toEqual([false, true, true]));
      expect(screen.getByText("0.3.284")).toBeInTheDocument();
      expect(screen.queryByText("正在读取组件状态…")).not.toBeInTheDocument();

      await act(async () => answer.resolve(componentStatus({
        installed: { sdkVersion: "0.3.290", claudeCodeVersion: "2.1.270", source: "installed", installedAt: null, compatible: true }
      })));
      expect(await screen.findByText("0.3.290")).toBeInTheDocument();
    });

    it("does not read again on a focus that no blur preceded", async () => {
      renderComponents();
      await screen.findByText("已是最新的兼容版本。");

      fireEvent.focus(window);
      await act(async () => undefined);

      expect(componentReads()).toEqual([false, true]);
    });

    it("swallows a failed read on returning to the window", async () => {
      renderComponents();
      await screen.findByText("已是最新的兼容版本。");
      runtimeMocks.claudeAgentComponentStatus.mockRejectedValueOnce(new Error("offline"));

      fireEvent.blur(window);
      fireEvent.focus(window);
      await waitFor(() => expect(componentReads()).toEqual([false, true, true]));
      await act(async () => undefined);

      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      expect(screen.getByText("0.3.284")).toBeInTheDocument();
    });
  });

  describe("browser preview", () => {
    it("says it cannot manage the components and asks the host nothing", async () => {
      render(<SessionHarness page={() => page(false)} />);

      expect(screen.getByText("浏览器预览无法管理 Claude Agent 组件。")).toBeInTheDocument();
      expect(screen.getByTestId("sign-in")).toBeInTheDocument();
      fireEvent.blur(window);
      fireEvent.focus(window);
      await act(async () => undefined);
      expect(runtimeMocks.claudeAgentComponentStatus).not.toHaveBeenCalled();
    });
  });
});

/**
 * The same rule through the real settings dialog: one session per opening, shared by
 * the provider pages inside it.
 */
describe("Claude Agent page in a settings session", () => {
  beforeEach(() => {
    backendState.connected = true;
    runtimeMocks.deleteApiKey.mockReset().mockResolvedValue({ configured: false });
    runtimeMocks.saveApiKey.mockReset().mockResolvedValue({ configured: true, keyLength: 8 });
    runtimeMocks.getStoredApiKeyLength.mockReset().mockResolvedValue(undefined);
    runtimeMocks.forgetStoredApiKeyLength.mockReset().mockResolvedValue(undefined);
    runtimeMocks.revealApiKey.mockReset().mockResolvedValue("stored-secret-key");
    runtimeMocks.fetchModels.mockReset().mockResolvedValue([]);
    runtimeMocks.claudeAgentLoginStatus.mockReset().mockResolvedValue(signedOut);
    runtimeMocks.claudeAgentOpenLogin.mockReset().mockResolvedValue(undefined);
    runtimeMocks.claudeAgentComponentStatus.mockReset().mockImplementation(hostComponentStatus);
  });

  function Dialog() {
    const document = normalizeDocument(createTestDocument());
    const [open, setOpen] = useState(true);
    const [settings, setSettings] = useState<GlobalSettingsType>(document.globalSettings);
    return (
      <>
        <button type="button" onClick={() => setOpen((current) => !current)}>{open ? "Close settings" : "Open settings"}</button>
        {open && (
          <GlobalSettings
            initialView="providers"
            settings={settings}
            onChange={setSettings}
            onFlush={vi.fn(() => Promise.resolve())}
          />
        )}
      </>
    );
  }

  it("reads the components and the sign-in once per opening, whatever page is visited between", async () => {
    const user = userEvent.setup();
    render(<Dialog />);

    await user.click(screen.getByRole("button", { name: "Claude Agent" }));
    // The components are read from disk and then checked against npm: two host reads per
    // opening; the sign-in is one read.
    expect(await screen.findByText("0.3.284")).toBeInTheDocument();
    expect(await screen.findByText("已是最新的兼容版本。")).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "打开终端登录" })).toBeInTheDocument();
    expect(componentReads()).toEqual([false, true]);
    expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(1);

    // Another provider, then back.
    await user.click(screen.getByRole("button", { name: "OpenAI Chat Completions" }));
    expect(screen.queryByText("Claude Agent 组件")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Claude Agent" }));

    expect(screen.getByText("0.3.284")).toBeInTheDocument();
    expect(screen.getByText("已是最新的兼容版本。")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "打开终端登录" })).toBeInTheDocument();
    await act(async () => undefined);
    expect(componentReads()).toEqual([false, true]);
    expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(1);

    // Closing the dialog and opening it again is a new session.
    await user.click(screen.getByRole("button", { name: "Close settings" }));
    await user.click(screen.getByRole("button", { name: "Open settings" }));
    await user.click(screen.getByRole("button", { name: "Claude Agent" }));

    await waitFor(() => expect(componentReads()).toEqual([false, true, false, true]));
    await waitFor(() => expect(runtimeMocks.claudeAgentLoginStatus).toHaveBeenCalledTimes(2));
  });

  it("shows no sign-in until the components are installed", async () => {
    const user = userEvent.setup();
    runtimeMocks.claudeAgentComponentStatus.mockResolvedValue(notInstalled);
    render(<Dialog />);

    await user.click(screen.getByRole("button", { name: "Claude Agent" }));

    expect(await screen.findByRole("button", { name: "安装" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "打开终端登录" })).not.toBeInTheDocument();
    expect(runtimeMocks.claudeAgentLoginStatus).not.toHaveBeenCalled();
  });
});

/**
 * The other built-in row. Its address left the main pane with this change; the
 * only remaining writer is the drawer field reserved for a local test double,
 * which the fake-backend e2e depends on.
 */
describe("OpenAI Codex provider address", () => {
  beforeEach(() => {
    runtimeMocks.deleteApiKey.mockReset().mockResolvedValue({ configured: false });
    runtimeMocks.saveApiKey.mockReset().mockResolvedValue({ configured: true, keyLength: 8 });
    runtimeMocks.getStoredApiKeyLength.mockReset().mockResolvedValue(undefined);
    runtimeMocks.forgetStoredApiKeyLength.mockReset().mockResolvedValue(undefined);
    runtimeMocks.revealApiKey.mockReset().mockResolvedValue("stored-secret-key");
    runtimeMocks.fetchModels.mockReset().mockResolvedValue([]);
    runtimeMocks.claudeAgentLoginStatus.mockReset().mockResolvedValue(signedOut);
    runtimeMocks.claudeAgentOpenLogin.mockReset().mockResolvedValue(undefined);
  });

  it("keeps the address out of the main pane and in the drawer as a test-stub field", async () => {
    const user = userEvent.setup();
    const { getSettings } = renderProviders();

    await user.click(screen.getByRole("button", { name: "OpenAI Codex" }));
    expect(screen.queryByLabelText("API 地址")).not.toBeInTheDocument();
    expect(screen.queryByText("API 地址")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "提供商设置" }));
    const drawer = screen.getByRole("dialog", { name: "提供商设置" });
    const stub = within(drawer).getByLabelText("本机测试桩地址");
    await user.type(stub, "http://127.0.0.1:1466/backend-api/codex");
    expect(getSettings().apiProviders.find((provider) => provider.family === "openai_codex")!.baseUrl)
      .toBe("http://127.0.0.1:1466/backend-api/codex");
  });
});
