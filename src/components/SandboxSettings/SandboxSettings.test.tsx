import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { SandboxSupport } from "../../types";
import { SandboxSettings } from ".";

const runtimeMocks = vi.hoisted(() => ({
  machineSandboxSupport: vi.fn(),
  setupLocalSandbox: vi.fn(),
  workspaceSandboxIgnoresCase: vi.fn()
}));

vi.mock("../../lib/runtime", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/runtime")>()),
  ...runtimeMocks
}));

const NEEDS_SETUP: SandboxSupport = {
  backend: "srt-win",
  available: false,
  detail: "The Windows sandbox needs a one-time setup",
  setup: true
};

describe("SandboxSettings", () => {
  beforeEach(() => {
    runtimeMocks.machineSandboxSupport.mockReset();
    runtimeMocks.setupLocalSandbox.mockReset();
    runtimeMocks.workspaceSandboxIgnoresCase.mockReset();
    runtimeMocks.workspaceSandboxIgnoresCase.mockResolvedValue(false);
  });

  it("points out a directory that ignores case where the sandbox is bubblewrap, and still lets it be switched on", async () => {
    const wsl = { kind: "wsl", distro: "Ubuntu" } as const;
    runtimeMocks.machineSandboxSupport.mockResolvedValue({ backend: "bubblewrap", available: true, detail: "/usr/bin/bwrap", setup: false });
    runtimeMocks.workspaceSandboxIgnoresCase.mockResolvedValue(true);
    const onChange = vi.fn();
    render(<SandboxSettings machine={wsl} machineName="WSL: Ubuntu" path="/mnt/c/work/app" settings={undefined} onChange={onChange} />);

    expect(await screen.findByRole("note")).toHaveTextContent(/\.git\/config/);
    expect(runtimeMocks.workspaceSandboxIgnoresCase).toHaveBeenCalledWith(wsl, "/mnt/c/work/app");
    await userEvent.click(screen.getByRole("switch"));
    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({ enabled: true }));
  });

  it("says nothing where the directory tells case apart, and does not ask where the sandbox is not bubblewrap", async () => {
    runtimeMocks.machineSandboxSupport.mockResolvedValue({ backend: "bubblewrap", available: true, detail: "", setup: false });
    const { unmount } = render(<SandboxSettings machine={null} machineName="本机" path="/home/me/app" settings={undefined} onChange={vi.fn()} />);
    await vi.waitFor(() => expect(runtimeMocks.workspaceSandboxIgnoresCase).toHaveBeenCalledTimes(1));
    expect(screen.queryByRole("note")).not.toBeInTheDocument();
    unmount();

    runtimeMocks.workspaceSandboxIgnoresCase.mockClear();
    runtimeMocks.machineSandboxSupport.mockResolvedValue({ backend: "seatbelt", available: true, detail: "", setup: false });
    render(<SandboxSettings machine={null} machineName="本机" path="/Users/me/app" settings={undefined} onChange={vi.fn()} />);
    expect(await screen.findByText(/Seatbelt/)).toBeInTheDocument();
    expect(runtimeMocks.workspaceSandboxIgnoresCase).not.toHaveBeenCalled();
    expect(screen.queryByRole("note")).not.toBeInTheDocument();
  });

  it("offers the one-time Windows setup and shows the sandbox available after it", async () => {
    runtimeMocks.machineSandboxSupport.mockResolvedValue(NEEDS_SETUP);
    runtimeMocks.setupLocalSandbox.mockResolvedValue({ backend: "srt-win", available: true, detail: "", setup: false });
    render(<SandboxSettings machine={null} machineName="本机" path="/work/app" settings={undefined} onChange={vi.fn()} />);

    await userEvent.click(await screen.findByRole("button", { name: /Set up|设置/ }));

    expect(runtimeMocks.machineSandboxSupport).toHaveBeenCalledWith(null);
    expect(runtimeMocks.setupLocalSandbox).toHaveBeenCalledTimes(1);
    expect(await screen.findByText(/Windows sandbox$|Windows 沙箱$/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Set up|设置/ })).not.toBeInTheDocument();
  });

  it("keeps the setup offered and says why when it fails", async () => {
    runtimeMocks.machineSandboxSupport.mockResolvedValue(NEEDS_SETUP);
    runtimeMocks.setupLocalSandbox.mockRejectedValue("The setup was cancelled at the administrator prompt");
    render(<SandboxSettings machine={null} machineName="本机" path="/work/app" settings={undefined} onChange={vi.fn()} />);

    await userEvent.click(await screen.findByRole("button", { name: /Set up|设置/ }));

    expect(await screen.findByRole("alert")).toHaveTextContent("cancelled at the administrator prompt");
    expect(screen.getByRole("button", { name: /Set up|设置/ })).toBeEnabled();
  });

  it("offers no setup where the sandbox is unavailable for another reason", async () => {
    runtimeMocks.machineSandboxSupport.mockResolvedValue({
      backend: "bubblewrap",
      available: false,
      detail: "bubblewrap is not installed",
      setup: false
    });
    render(<SandboxSettings machine={null} machineName="本机" path="/work/app" settings={undefined} onChange={vi.fn()} />);

    expect(await screen.findByText(/bubblewrap is not installed/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Set up|设置/ })).not.toBeInTheDocument();
  });

  it("asks the workspace's own machine, and leaves an SSH machine's setup to that machine", async () => {
    runtimeMocks.machineSandboxSupport.mockResolvedValue({
      ...NEEDS_SETUP,
      detail: "Run `mewrk-remote.exe sandbox-setup` there as an administrator"
    });
    const machine = { kind: "ssh" as const, machineId: "winbox" };
    render(<SandboxSettings machine={machine} machineName="SSH: winbox" path="C:/work/app" settings={undefined} onChange={vi.fn()} />);

    expect(await screen.findByText(/^SSH: winbox · .*sandbox-setup/)).toBeInTheDocument();
    expect(runtimeMocks.machineSandboxSupport).toHaveBeenCalledWith(machine);
    expect(screen.queryByRole("button", { name: /Set up|设置/ })).not.toBeInTheDocument();
    expect(runtimeMocks.setupLocalSandbox).not.toHaveBeenCalled();
  });
});
