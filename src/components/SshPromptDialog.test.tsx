import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { configureI18n } from "../i18n";
import type { SshPrompt } from "../lib/sshPrompts";
import { withSshPrompt, withoutSshPrompt } from "../lib/sshPrompts";
import { SshPromptDialog } from "./SshPromptDialog";

afterEach(() => configureI18n("zh-CN"));

function prompt(overrides: Partial<SshPrompt> = {}): SshPrompt {
  return {
    id: "ssh-prompt-1",
    machine: "dev@devbox",
    kind: "secret",
    prompt: "dev@devbox's password:",
    hostKey: null,
    retry: false,
    ...overrides
  };
}

describe("SshPromptDialog", () => {
  it("shows a new host key's fingerprint and accepts or rejects it", async () => {
    configureI18n("en-US");
    const user = userEvent.setup();
    const onAnswer = vi.fn().mockResolvedValue(undefined);
    const hostKey = prompt({
      kind: "hostKey",
      prompt: "The authenticity of host 'devbox (192.0.2.7)' can't be established…",
      hostKey: { host: "devbox (192.0.2.7)", keyType: "ED25519", fingerprint: "SHA256:8Fz0bQh2Yc0p1X7nq" }
    });
    const { unmount } = render(<SshPromptDialog prompt={hostKey} onAnswer={onAnswer} />);

    expect(screen.getByRole("dialog", { name: "Confirm the host key of dev@devbox" })).toBeInTheDocument();
    expect(screen.getByText("SHA256:8Fz0bQh2Yc0p1X7nq")).toBeInTheDocument();
    expect(screen.getByText("ED25519")).toBeInTheDocument();
    expect(screen.getByText(/added to ~\/\.ssh\/known_hosts/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Accept" }));
    expect(onAnswer).toHaveBeenLastCalledWith("yes");
    unmount();

    render(<SshPromptDialog prompt={hostKey} onAnswer={onAnswer} />);
    await user.click(screen.getByRole("button", { name: "Reject" }));
    expect(onAnswer).toHaveBeenLastCalledWith(null);
  });

  it("takes a password masked, says it stays in memory, and says when the last one failed", async () => {
    const user = userEvent.setup();
    const onAnswer = vi.fn().mockResolvedValue(undefined);
    render(<SshPromptDialog prompt={prompt({ retry: true })} onAnswer={onAnswer} />);

    expect(screen.getByRole("dialog", { name: "登录 dev@devbox" })).toBeInTheDocument();
    const field = screen.getByLabelText(/dev@devbox's password:/);
    expect(field).toHaveAttribute("type", "password");
    expect(field).toHaveFocus();
    expect(screen.getByText("上次输入的没有被接受，请重新输入。")).toBeInTheDocument();
    expect(screen.getByText(/只把它保存在内存里/)).toBeInTheDocument();
    await user.type(field, "hunter2{Enter}");
    expect(onAnswer).toHaveBeenCalledWith("hunter2");
  });

  it("keeps the dialog and says why when the answer cannot be delivered", async () => {
    const user = userEvent.setup();
    const onAnswer = vi.fn().mockRejectedValue(new Error("这个 SSH 询问已经结束"));
    render(<SshPromptDialog prompt={prompt()} onAnswer={onAnswer} />);
    await user.click(screen.getByRole("button", { name: "取消" }));
    expect(onAnswer).toHaveBeenCalledWith(null);
    expect(await screen.findByRole("alert")).toHaveTextContent("这个 SSH 询问已经结束");
    expect(screen.getByRole("button", { name: "登录" })).toBeEnabled();
  });

  it("asks a confirmation as allow or deny", async () => {
    const user = userEvent.setup();
    const onAnswer = vi.fn().mockResolvedValue(undefined);
    render(<SshPromptDialog prompt={prompt({ kind: "confirm", prompt: "Allow use of key id_ed25519?" })} onAnswer={onAnswer} />);
    expect(screen.getByText("Allow use of key id_ed25519?")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "允许" }));
    expect(onAnswer).toHaveBeenCalledWith("yes");
  });
});

describe("SSH prompt queue", () => {
  it("adds each announced question once and takes settled ones away", () => {
    const first = prompt();
    const second = prompt({ id: "ssh-prompt-2" });
    const queue = withSshPrompt(withSshPrompt(withSshPrompt([], first), second), first);
    expect(queue.map((entry) => entry.id)).toEqual(["ssh-prompt-1", "ssh-prompt-2"]);
    expect(withoutSshPrompt(queue, "ssh-prompt-1").map((entry) => entry.id)).toEqual(["ssh-prompt-2"]);
    expect(withoutSshPrompt(queue, "missing")).toBe(queue);
  });
});
