import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { configureI18n } from "./i18n";
import { documentWithModel, resetAppMocks, runtimeMocks } from "./test/appMocks";
import { emitAppPushEvent } from "./test/appMockInstances";

const sshPromptMocks = vi.hoisted(() => ({
  listSshPrompts: vi.fn(),
  answerSshPrompt: vi.fn()
}));

vi.mock("./lib/appEvents", async () => (await import("./test/appMockInstances")).appEventsModuleMock());
vi.mock("./lib/sshPrompts", async (importOriginal) => ({
  ...await importOriginal<typeof import("./lib/sshPrompts")>(),
  ...sshPromptMocks
}));
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

/**
 * An SSH connection's question reaches whatever is on screen: the host pushes it, the app shows
 * it over everything, and the answer goes back to the host.
 */
describe("SSH questions from the host", () => {
  beforeEach(() => {
    resetAppMocks();
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    runtimeMocks.loadDocument.mockResolvedValue(documentWithModel());
    sshPromptMocks.listSshPrompts.mockReset().mockResolvedValue([{
      id: "ssh-prompt-1",
      machine: "dev@devbox",
      kind: "hostKey",
      prompt: "The authenticity of host 'devbox' can't be established…",
      hostKey: { host: "devbox", keyType: "ED25519", fingerprint: "SHA256:abc" },
      retry: false
    }]);
    sshPromptMocks.answerSshPrompt.mockReset().mockResolvedValue(undefined);
  });

  it("shows waiting questions one at a time and sends each answer to the host", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");

    expect(await screen.findByRole("dialog", { name: "确认 dev@devbox 的主机密钥" })).toBeInTheDocument();
    act(() => emitAppPushEvent({
      type: "sshPromptRequested",
      id: "ssh-prompt-2",
      machine: "dev@devbox",
      kind: "secret",
      prompt: "dev@devbox's password:",
      hostKey: null,
      retry: false
    }));
    expect(screen.queryByRole("dialog", { name: "登录 dev@devbox" })).toBeNull();

    await user.click(screen.getByRole("button", { name: "接受" }));
    expect(sshPromptMocks.answerSshPrompt).toHaveBeenCalledWith("ssh-prompt-1", "yes");
    const signIn = await screen.findByRole("dialog", { name: "登录 dev@devbox" });
    expect(signIn).toBeInTheDocument();

    // Settled elsewhere — its connection gave up — the question goes away.
    act(() => emitAppPushEvent({ type: "sshPromptSettled", id: "ssh-prompt-2" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "登录 dev@devbox" })).toBeNull());
  });
});
