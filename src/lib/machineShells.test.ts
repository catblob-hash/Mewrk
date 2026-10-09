import { describe, expect, it } from "vitest";
import type { ExecutionEnvironmentAssets, MachineShells, ShellBackend } from "../types";
import {
  availableShellBackends,
  backendOfTool,
  effectiveAgentShell,
  isShellBackend,
  knownShells,
  preferredBackend,
  SHELL_BACKENDS_BY_OS,
  shellBackendLabel,
  sshEndpoint,
  terminalShellsFor,
  toolsForShellBackends,
  withAgentShell,
  withDefaultAgentShell,
  withoutProbe
} from "./machineShells";
import type { TerminalShell } from "./workspaces";

const probe = (os: MachineShells["os"], backends: MachineShells["shells"][number]["backend"][]): MachineShells => ({
  os,
  shells: backends.map((backend) => ({ backend, path: `/bin/${backend}` })),
  probedAt: "2026-09-23T00:00:00Z"
});

const assets = (overrides: Partial<ExecutionEnvironmentAssets> = {}): ExecutionEnvironmentAssets => ({
  sshMachines: [{
    id: "m1",
    name: "devbox",
    host: "dev@devbox",
    port: 0,
    identityFile: "",
    createdAt: "",
    updatedAt: ""
  }],
  envVars: {},
  ...overrides
});

describe("the shell × OS table", () => {
  it("registers no shell that cannot carry Mewrk's own scripts", () => {
    expect(SHELL_BACKENDS_BY_OS.windows).toEqual(["pwsh", "powershell", "bash"]);
    for (const os of ["wsl", "macos", "linux"] as const) {
      expect(SHELL_BACKENDS_BY_OS[os]).not.toContain("powershell");
      expect(SHELL_BACKENDS_BY_OS[os]).not.toContain("pwsh");
    }
  });

  it("maps every shell tool back to its backend", () => {
    expect(backendOfTool("zsh")).toBe("zsh");
    expect(backendOfTool("pwsh")).toBe("pwsh");
    expect(backendOfTool("powershell")).toBe("powershell");
    expect(backendOfTool("read")).toBeNull();
    expect(backendOfTool("shell")).toBeNull();
    // Only the exact tool names are shell tools.
    expect(backendOfTool("PowerShell")).toBeNull();
    expect(backendOfTool("powershell7")).toBeNull();
  });

  it("recognizes both PowerShell editions as backends and nothing else spelled like them", () => {
    for (const backend of ["bash", "zsh", "sh", "pwsh", "powershell"]) {
      expect(isShellBackend(backend)).toBe(true);
    }
    for (const value of ["fish", "PowerShell", "pwsh.exe", "", null, undefined, 7]) {
      expect(isShellBackend(value)).toBe(false);
    }
  });

  it("names the two PowerShell editions apart", () => {
    expect(shellBackendLabel("pwsh")).toBe("PowerShell 7");
    expect(shellBackendLabel("powershell")).toBe("Windows PowerShell");
    expect(shellBackendLabel("bash")).toBe("Bash");
    expect(shellBackendLabel("zsh")).toBe("zsh");
    expect(shellBackendLabel("sh")).toBe("sh");
  });

  it("ranks each OS's shells in Mewrk's own fixed order", () => {
    expect(SHELL_BACKENDS_BY_OS.windows).toEqual(["pwsh", "powershell", "bash"]);
    expect(SHELL_BACKENDS_BY_OS.linux).toEqual(["bash", "zsh", "sh"]);
    expect(SHELL_BACKENDS_BY_OS.macos).toEqual(["zsh", "bash", "sh"]);
    expect(SHELL_BACKENDS_BY_OS.wsl).toEqual(["bash", "zsh", "sh"]);
    expect(preferredBackend("windows", ["bash", "powershell", "pwsh"])).toBe("pwsh");
    expect(preferredBackend("windows", ["bash", "powershell"])).toBe("powershell");
    expect(preferredBackend("windows", ["bash", "pwsh"])).toBe("pwsh");
    expect(preferredBackend("windows", ["bash"])).toBe("bash");
    expect(preferredBackend("macos", ["sh", "bash"])).toBe("bash");
    expect(preferredBackend("macos", [])).toBeNull();
  });
});

describe("what a conversation's machines offer", () => {
  const windows = { machine: { kind: "ssh" as const, machineId: "m1" }, path: "C:/work" };
  const local = { machine: null, path: "/Users/dev/app" };

  it("lists the union of the machines' probed backends", () => {
    const probes = { local: probe("macos", ["zsh", "bash", "sh"]), "ssh:m1": probe("windows", ["powershell"]) };
    expect(availableShellBackends([local, windows], probes, "MacIntel")).toEqual(["bash", "zsh", "sh", "powershell"]);
    expect(availableShellBackends([windows], probes, "MacIntel")).toEqual(["powershell"]);
    const tools = ["read", "bash", "powershell", "zsh"].map((name) => ({ name }));
    expect(toolsForShellBackends(tools, ["powershell"]).map((tool) => tool.name)).toEqual(["read", "powershell"]);
  });

  it("offers each PowerShell edition's tool only where that edition was found", () => {
    const both = { "ssh:m1": probe("windows", ["powershell", "pwsh", "bash"]) };
    // Tool order, not probe order: pwsh sits before powershell.
    expect(availableShellBackends([windows], both, "MacIntel")).toEqual(["bash", "pwsh", "powershell"]);
    const only7 = { "ssh:m1": probe("windows", ["pwsh"]) };
    expect(availableShellBackends([windows], only7, "MacIntel")).toEqual(["pwsh"]);
    const tools = ["read", "bash", "pwsh", "powershell", "zsh"].map((name) => ({ name }));
    expect(toolsForShellBackends(tools, ["pwsh"]).map((tool) => tool.name)).toEqual(["read", "pwsh"]);
    expect(toolsForShellBackends(tools, ["powershell"]).map((tool) => tool.name)).toEqual(["read", "powershell"]);
    expect(toolsForShellBackends(tools, ["bash", "pwsh", "powershell"]).map((tool) => tool.name))
      .toEqual(["read", "bash", "pwsh", "powershell"]);
  });

  it("assumes bash on a remote machine nobody has probed, and the host OS's shells before the host's probe", () => {
    expect(knownShells(windows.machine, {}, "Win32")).toEqual({ os: null, backends: ["bash"] });
    expect(knownShells({ kind: "wsl", distro: "Ubuntu" }, {}, "Win32").os).toBe("wsl");
    expect(knownShells(null, {}, "Win32").backends).toEqual(["pwsh", "powershell", "bash"]);
    expect(knownShells(null, {}, "MacIntel").backends).toEqual(["zsh", "bash", "sh"]);
    // An unknown platform keeps every tool rather than hiding one the host would run.
    expect(knownShells(null, {}, "").backends).toEqual(["bash", "zsh", "sh", "pwsh", "powershell"]);
    // No workspace at all is this machine.
    expect(availableShellBackends([], {}, "Linux x86_64")).toEqual(["bash", "zsh", "sh"]);
  });
});

describe("the shells a terminal offers", () => {
  const wsl = { kind: "wsl" as const, distro: "Ubuntu" };
  const ssh = { kind: "ssh" as const, machineId: "m1" };

  it("lists the machine's probed shells in its OS's priority order", () => {
    const probes = { local: probe("macos", ["zsh", "bash", "sh"]), "wsl:Ubuntu": probe("wsl", ["bash", "sh"]) };
    // This machine's sh is left out: its line editor cannot hold a line for the Git mutex.
    expect(terminalShellsFor(null, probes, "MacIntel")).toEqual(["zsh", "bash"]);
    // A shell the probe did not find is not offered, however high it ranks.
    expect(terminalShellsFor(wsl, probes, "Win32")).toEqual(["bash", "sh"]);
  });

  it("offers PowerShell on a Windows host and a Windows SSH machine, never in WSL", () => {
    const probes = {
      local: probe("windows", ["powershell", "bash"]),
      "ssh:m1": probe("windows", ["powershell", "bash"]),
      "wsl:Ubuntu": probe("wsl", ["bash"])
    };
    expect(terminalShellsFor(null, probes, "Win32")).toEqual(["powershell", "bash"]);
    expect(terminalShellsFor(ssh, probes, "Win32")).toEqual(["powershell", "bash"]);
    expect(terminalShellsFor(wsl, probes, "Win32")).toEqual(["bash"]);
  });

  it("offers ONE PowerShell terminal however many editions a machine has", () => {
    // The terminal is not split like the shell tools: it starts PowerShell 7
    // where installed and Windows PowerShell otherwise.
    const probes = {
      local: probe("windows", ["pwsh", "powershell", "bash"]),
      "ssh:m1": probe("windows", ["powershell", "pwsh", "bash"])
    };
    expect(terminalShellsFor(null, probes, "Win32")).toEqual(["powershell", "bash"]);
    expect(terminalShellsFor(ssh, probes, "Win32")).toEqual(["powershell", "bash"]);
    // Either edition alone is the one PowerShell terminal as well.
    expect(terminalShellsFor(null, { local: probe("windows", ["pwsh", "bash"]) }, "Win32"))
      .toEqual(["powershell", "bash"]);
    expect(terminalShellsFor(null, { local: probe("windows", ["pwsh"]) }, "Win32")).toEqual(["powershell"]);
    expect(terminalShellsFor(null, { local: probe("windows", ["powershell"]) }, "Win32")).toEqual(["powershell"]);
  });

  it("keeps the priority order while de-duplicating the PowerShell editions", () => {
    // An OS the renderer cannot name ranks every backend in tool order, where
    // the two editions sit after bash: PowerShell is listed once, last.
    expect(terminalShellsFor(null, {}, "")).toEqual(["bash", "zsh", "powershell"]);
    // On Windows the editions outrank bash, so the one PowerShell leads.
    const windows = { local: probe("windows", ["bash", "powershell", "pwsh"]) };
    expect(terminalShellsFor(null, windows, "Win32")).toEqual(["powershell", "bash"]);
  });

  it("never offers a PowerShell terminal in WSL, whatever the probe reports", () => {
    const odd = { "wsl:Ubuntu": probe("wsl", ["bash", "pwsh", "powershell"]) };
    expect(terminalShellsFor(wsl, odd, "Win32")).toEqual(["bash"]);
  });

  it("types a terminal shell so a pwsh backend cannot leak into it", () => {
    // `pwsh` is a backend, not a terminal shell; the terminal's name for both
    // PowerShell editions is `powershell`.
    // @ts-expect-error a ShellBackend is wider than a TerminalShell
    const leaked: TerminalShell = "pwsh" as ShellBackend;
    expect(leaked).toBe("pwsh");
    const everyBackend: ShellBackend[] = ["bash", "zsh", "sh", "pwsh", "powershell"];
    const shells = terminalShellsFor(null, { local: probe("windows", everyBackend) }, "Win32");
    expect(shells).not.toContain("pwsh");
    expect(shells).toEqual(["powershell", "bash"]);
  });

  it("falls back to what is assumed of a machine nobody has probed", () => {
    expect(terminalShellsFor(null, {}, "Win32")).toEqual(["powershell", "bash"]);
    expect(terminalShellsFor(undefined, {}, "Linux x86_64")).toEqual(["bash", "zsh"]);
    expect(terminalShellsFor(ssh, {}, "MacIntel")).toEqual(["bash"]);
  });

  it("offers nothing when the probe found nothing a terminal can start", () => {
    expect(terminalShellsFor(ssh, { "ssh:m1": probe("linux", []) }, "MacIntel")).toEqual([]);
  });
});

describe("agent shells", () => {
  it("records a new machine's first probe as its OS's most preferred backend it has", () => {
    const next = withDefaultAgentShell(assets(), "ssh:m1", probe("linux", ["sh", "zsh", "bash"]));
    expect(next.sshMachines[0]!.agentShell).toBe("bash");
    // A machine that already has a choice keeps it.
    expect(withDefaultAgentShell(next, "ssh:m1", probe("linux", ["bash"]))).toBe(next);
    // This machine has no agent shell, and a deleted machine has nothing to record.
    const untouched = assets();
    expect(withDefaultAgentShell(untouched, "local", probe("macos", ["zsh"]))).toBe(untouched);
    expect(withDefaultAgentShell(untouched, "ssh:gone", probe("linux", ["bash"]))).toBe(untouched);
  });

  it("records a Windows machine's first probe as PowerShell 7 when it has it, else Windows PowerShell", () => {
    const withBoth = withDefaultAgentShell(assets(), "ssh:m1", probe("windows", ["bash", "powershell", "pwsh"]));
    expect(withBoth.sshMachines[0]!.agentShell).toBe("pwsh");
    const only5 = withDefaultAgentShell(assets(), "ssh:m1", probe("windows", ["bash", "powershell"]));
    expect(only5.sshMachines[0]!.agentShell).toBe("powershell");
    const onlyBash = withDefaultAgentShell(assets(), "ssh:m1", probe("windows", ["bash"]));
    expect(onlyBash.sshMachines[0]!.agentShell).toBe("bash");
  });

  it("lets either PowerShell edition be chosen as an agent shell", () => {
    const machine = { kind: "ssh" as const, machineId: "m1" };
    const pwsh = withAgentShell(assets(), machine, "pwsh");
    expect(pwsh.sshMachines[0]!.agentShell).toBe("pwsh");
    // The same value again changes nothing.
    expect(withAgentShell(pwsh, machine, "pwsh")).toBe(pwsh);
    const both = probe("windows", ["pwsh", "powershell", "bash"]);
    expect(effectiveAgentShell(machine, pwsh, { "ssh:m1": both })).toBe("pwsh");
    // A saved Windows PowerShell choice is kept even though PowerShell 7 outranks it.
    const five = withAgentShell(assets(), machine, "powershell");
    expect(effectiveAgentShell(machine, five, { "ssh:m1": both })).toBe("powershell");
    // PowerShell 7 uninstalled: the choice falls back by priority, to the other edition.
    expect(effectiveAgentShell(machine, pwsh, { "ssh:m1": probe("windows", ["powershell", "bash"]) }))
      .toBe("powershell");
    // Neither edition left: bash.
    expect(effectiveAgentShell(machine, pwsh, { "ssh:m1": probe("windows", ["bash"]) })).toBe("bash");
    // Before any probe the configured choice stands.
    expect(effectiveAgentShell(machine, pwsh, {})).toBe("pwsh");
  });

  it("records a WSL distribution's choice by name", () => {
    const next = withDefaultAgentShell(assets(), "wsl:Ubuntu", probe("wsl", ["bash", "sh"]));
    expect(next.wslAgentShells).toEqual({ Ubuntu: "bash" });
    expect(withAgentShell(next, { kind: "wsl", distro: "Ubuntu" }, "sh").wslAgentShells).toEqual({ Ubuntu: "sh" });
  });

  it("resolves a choice the machine no longer has the way the host does", () => {
    const chosen = withAgentShell(assets(), { kind: "ssh", machineId: "m1" }, "zsh");
    const machine = { kind: "ssh" as const, machineId: "m1" };
    expect(effectiveAgentShell(machine, chosen, { "ssh:m1": probe("linux", ["zsh", "bash"]) })).toBe("zsh");
    expect(effectiveAgentShell(machine, chosen, { "ssh:m1": probe("linux", ["bash", "sh"]) })).toBe("bash");
    expect(effectiveAgentShell(machine, chosen, {})).toBe("zsh");
    expect(effectiveAgentShell(null, chosen, {})).toBeNull();
  });
});

describe("a machine moved to another address", () => {
  const machine = { kind: "ssh" as const, machineId: "m1" };
  const moved = (host: string) => {
    const base = assets();
    return { ...base, sshMachines: base.sshMachines.map((entry) => ({ ...entry, host })) };
  };

  it("is told apart by its endpoint, not its id", () => {
    const before = sshEndpoint(assets(), machine);
    expect(before).not.toBeNull();
    expect(sshEndpoint(assets(), machine)).toBe(before);
    expect(sshEndpoint(moved("dev@other"), machine)).not.toBe(before);
    expect(sshEndpoint({ ...assets(), sshMachines: [] }, machine)).toBeNull();
    expect(sshEndpoint(assets(), { kind: "wsl", distro: "Ubuntu" })).toBeNull();
    expect(sshEndpoint(assets(), null)).toBeNull();
  });

  it("loses its old probe, so it reads as never probed until the new address answers", () => {
    const probes = { local: probe("macos", ["zsh"]), "ssh:m1": probe("linux", ["bash"]) };
    const next = withoutProbe(probes, "ssh:m1");
    expect(next).toEqual({ local: probes.local });
    expect(knownShells(machine, next, "MacIntel")).toEqual({ os: null, backends: ["bash"] });
    expect(withoutProbe(next, "ssh:m1")).toBe(next);
  });
});
