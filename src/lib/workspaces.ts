import { t } from "../i18n";
import type {
  AttachedWorkspace,
  Conversation,
  ConversationWorktree,
  RunTarget,
  SshMachineConfig,
  ToolDescriptor,
  Workspace
} from "../types";

export const TEMPORARY_WORKSPACE_ID = "__temporary__";

/**
 * A machine's identity: `local`, `wsl:<distro>`, or `ssh:<machine id>`. Matches
 * the host `run_environment::env_key` exactly.
 *
 * It is what tells two workspaces apart when their paths are spelled alike: one
 * machine's `/srv/app` is not another's.
 */
export function runEnvKey(target: RunTarget | null | undefined): string {
  if (!target) return "local";
  return target.kind === "wsl" ? `wsl:${target.distro}` : `ssh:${target.machineId}`;
}

/**
 * Key of a workspace's environment-variable table in
 * `ExecutionEnvironmentAssets.envVars`: the machine's {@link runEnvKey} and the
 * directory as the workspace records it, joined by `|`. Matches the host
 * `run_environment::workspace_env_key` exactly.
 *
 * Variables belong to the workspace, not to the machine it is on: two
 * directories on one machine each carry their own table.
 */
export function workspaceEnvKey(machine: RunTarget | null | undefined, path: string): string {
  return `${runEnvKey(machine)}|${path}`;
}

/**
 * Key of a workspace's capability level — its skills, MCP servers and hooks: the machine and the
 * registered directory, less trailing separators. Mirrors the host's `capabilities::workspace_key`.
 * A level belongs to the directory, not to a project or a conversation, so every conversation
 * listing the same directory shares its entries.
 */
export function capabilityWorkspaceKey(workspace: Pick<AttachedWorkspace, "machine" | "path">): string {
  const trimmed = workspace.path.replace(/[\\/]+$/, "");
  return workspaceEnvKey(workspace.machine, trimmed || workspace.path);
}

/** The machine and directory a {@link capabilityWorkspaceKey} names. */
export function parseCapabilityWorkspaceKey(key: string): { machine: RunTarget | null; path: string } {
  const bar = key.indexOf("|");
  const environment = bar < 0 ? "local" : key.slice(0, bar);
  const path = bar < 0 ? key : key.slice(bar + 1);
  if (environment.startsWith("wsl:")) return { machine: { kind: "wsl", distro: environment.slice(4) }, path };
  if (environment.startsWith("ssh:")) return { machine: { kind: "ssh", machineId: environment.slice(4) }, path };
  return { machine: null, path };
}

/** One of a conversation's workspaces whose `.mewrk` it reads. */
export interface CapabilityWorkspace {
  /** The number the model addresses it by. */
  number: number;
  /** Its {@link capabilityWorkspaceKey}, which its catalog rows carry. */
  key: string;
  machine: RunTarget | null;
  /** The registered directory, absolute. */
  path: string;
}

/**
 * The workspaces whose skills, MCP servers and hooks a conversation uses — the union of their
 * levels — numbered as the model addresses them: the project's workspaces, then the
 * conversation's own attached ones. A workspace is listed as registered, since a worktree reads
 * the configuration of the directory it was checked out from; the temporary project's scratch
 * folder has no `.mewrk` of its own but still takes number 1. Mirrors the host's
 * `capabilities::conversation_locations`.
 */
export function capabilityWorkspaces(
  workspace: Workspace | null | undefined,
  conversation?: Pick<Conversation, "attachedWorkspaces"> | null
): CapabilityWorkspace[] {
  if (!workspace) return [];
  const own: Array<AttachedWorkspace | null> = isTemporaryWorkspace(workspace)
    ? [null]
    : registeredProjectWorkspaces(workspace);
  const seen = new Set<string>();
  return [...own, ...(conversation?.attachedWorkspaces ?? [])].flatMap((entry, index) => {
    if (!entry || !entry.path.trim()) return [];
    const key = capabilityWorkspaceKey(entry);
    if (seen.has(key)) return [];
    seen.add(key);
    return [{ number: index + 1, key, machine: entry.machine ?? null, path: entry.path }];
  });
}

/** How many projects and conversations have a workspace on one machine. */
export interface MachineUsage {
  projects: number;
  conversations: number;
}

/**
 * How many projects and conversations still have a workspace on `machine`
 * (`null` for this one): a project by its first workspace or any of its further
 * ones, a conversation by one of its attached workspaces. The temporary project
 * has no directory of its own and never counts.
 */
export function machineUsage(
  workspaces: readonly Workspace[] | null | undefined,
  machine: RunTarget | null
): MachineUsage {
  const key = runEnvKey(machine);
  const onMachine = (entry: Pick<AttachedWorkspace, "machine">) => runEnvKey(entry.machine) === key;
  let projects = 0;
  let conversations = 0;
  for (const project of workspaces ?? []) {
    if (!isTemporaryWorkspace(project)
      && (onMachine(project) || (project.additionalWorkspaces ?? []).some(onMachine))) {
      projects += 1;
    }
    for (const conversation of project.conversations) {
      if ((conversation.attachedWorkspaces ?? []).some(onMachine)) conversations += 1;
    }
  }
  return { projects, conversations };
}

/** Whether two machine bindings name the same machine. */
export function sameMachine(
  left: RunTarget | null | undefined,
  right: RunTarget | null | undefined
): boolean {
  return runEnvKey(left) === runEnvKey(right);
}

/**
 * Whether `machine` is an SSH machine that has been deleted from the catalog. Its workspaces
 * stop working until they are given a machine and directory again: registering the same host
 * again makes a new machine id, so nothing else brings them back.
 */
export function isDeletedMachine(
  machine: RunTarget | null | undefined,
  sshMachines: readonly SshMachineConfig[]
): boolean {
  return machine?.kind === "ssh" && !sshMachines.some((entry) => entry.id === machine.machineId);
}

/**
 * How a machine that is not this one is named beside a path: `WSL: Ubuntu`,
 * `SSH: devbox`. `null` for the host machine, which needs no qualifier.
 *
 * The SSH catalog name, not the id: a machine the user renamed is the one they
 * recognize by its name. A machine deleted from the catalog is called that,
 * since its id means nothing to anyone.
 */
export function workspaceMachineLabel(
  machine: RunTarget | null | undefined,
  sshMachines: readonly SshMachineConfig[]
): string | null {
  if (!machine) return null;
  if (machine.kind === "wsl") return `WSL: ${machine.distro}`;
  const name = sshMachines.find((entry) => entry.id === machine.machineId)?.name;
  return name === undefined ? t("已删除的机器", "Deleted machine") : `SSH: ${name}`;
}

/**
 * The chip label for a directory: its last segment, since a chip cannot hold an
 * absolute path. The full path stays in the chip's `title`, which is what the
 * user checks when two directories share a name.
 */
export function workspaceDirectoryLabel(path: string): string {
  const trimmed = path.trim().replace(/[\\/]+$/, "");
  const separator = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  // A root has no last segment; it is labelled by itself.
  return trimmed.slice(separator + 1) || trimmed || path.trim();
}

/** A path with its machine appended when the machine is not this one. */
export function workspaceLocationTitle(
  path: string,
  machine: RunTarget | null | undefined,
  sshMachines: readonly SshMachineConfig[]
): string {
  const label = workspaceMachineLabel(machine, sshMachines);
  return label ? `${path} (${label})` : path;
}

/** Whether two records name the same directory on the same machine. Mirrors the host's `same_location`. */
function sameWorkspaceLocation(left: AttachedWorkspace, right: AttachedWorkspace): boolean {
  const trimmed = (path: string) => path.replace(/[\\/]+$/, "");
  return sameMachine(left.machine, right.machine) && trimmed(left.path) === trimmed(right.path);
}

/**
 * The project's workspaces as registered — never a worktree standing in for one — in order:
 * its first directory, then the ones added after it.
 */
export function registeredProjectWorkspaces(workspace: Workspace | null | undefined): AttachedWorkspace[] {
  if (!workspace || isTemporaryWorkspace(workspace)) return [];
  return [
    { machine: workspace.machine ?? null, path: workspace.path },
    ...(workspace.additionalWorkspaces ?? [])
  ];
}

/**
 * The worktree standing in for the project workspace at 1-based `member`, registered at
 * `registered`. Mirrors the host's `Conversation::worktree_for`: a record names its workspace by
 * machine and path, never by position, and a record with no workspace is workspace 1's.
 */
export function worktreeFor(
  conversation: Pick<Conversation, "worktrees"> | null | undefined,
  member: number,
  registered: AttachedWorkspace
): ConversationWorktree | null {
  return conversation?.worktrees.find((worktree) => (
    worktree.workspace ? sameWorkspaceLocation(worktree.workspace, registered) : member === 1
  )) ?? null;
}

/**
 * `worktrees` with the record for the project workspace at `member` (registered at `registered`)
 * replaced by `worktree`, or removed when it is null.
 */
export function withConversationWorktree(
  worktrees: readonly ConversationWorktree[],
  member: number,
  registered: AttachedWorkspace,
  worktree: ConversationWorktree | null
): ConversationWorktree[] {
  const others = worktrees.filter((candidate) => !(
    candidate.workspace ? sameWorkspaceLocation(candidate.workspace, registered) : member === 1
  ));
  return worktree ? [...others, worktree] : others;
}

/** A worktree's short name: the last segment of its directory, which is what Git calls it. */
export function worktreeName(worktree: Pick<ConversationWorktree, "path">): string {
  return workspaceDirectoryLabel(worktree.path);
}

/**
 * A project's own workspaces in order: its first directory, then the ones added
 * after it. These are the directories every conversation of the project shares.
 *
 * With a conversation, each entry is the directory that conversation's tools
 * actually use: its isolated worktree of that workspace when it has one. A
 * temporary project has no shared directory and so no entries.
 */
export function projectWorkspaces(
  workspace: Workspace | null | undefined,
  conversation?: Pick<Conversation, "worktrees"> | null
): AttachedWorkspace[] {
  return registeredProjectWorkspaces(workspace).map((registered, index) => {
    const worktree = worktreeFor(conversation, index + 1, registered);
    return worktree ? { machine: registered.machine ?? null, path: worktree.path } : registered;
  });
}

/**
 * The conversation's workspaces in the order the model addresses them: the
 * project's workspaces first — its primary is 1 — and the conversation's own
 * attached ones after them.
 *
 * This mirrors `workspace_set::WorkspaceSet::resolve` on the host, which is the
 * authority — the renderer reads this only to label chips and to decide what the
 * tool picker may offer. A project entry uses the worktree when the
 * conversation has one of it, because that is the directory its tools resolve against.
 * A temporary project still counts as workspace 1: the host gives it a scratch
 * directory of its own.
 */
export function conversationWorkspaces(
  workspace: Workspace | null | undefined,
  conversation: Pick<Conversation, "worktrees" | "attachedWorkspaces"> | null | undefined
): AttachedWorkspace[] {
  const project: AttachedWorkspace[] = !workspace
    ? []
    : isTemporaryWorkspace(workspace)
      ? [{ machine: null, path: workspace.path }]
      : projectWorkspaces(workspace, conversation);
  return [...project, ...(conversation?.attachedWorkspaces ?? [])];
}

/**
 * A shell the built-in terminal can start. Mirrors the host's `terminal::TerminalShell`.
 * Which ones a workspace's menu offers is `machineShells::terminalShellsFor`'s answer.
 */
export type TerminalShell = "powershell" | "bash" | "zsh" | "fish" | "sh";

/** Whether the renderer runs on a Windows host. */
export function hostIsWindows(platform: string): boolean {
  return /^win/i.test(platform.trim());
}

/** The name a shell goes by in a menu and on a tab. */
export function terminalShellLabel(shell: TerminalShell): string {
  switch (shell) {
    case "powershell": return "PowerShell";
    case "bash": return "bash";
    case "zsh": return "zsh";
    case "fish": return "fish";
    case "sh": return "sh";
  }
}

/**
 * The tools whose wire schema the host gives a `workspace` argument once the
 * conversation has more than one workspace, besides every shell tool. Mirrors
 * `builtin_schemas::takes_a_workspace`; a tool absent here takes no workspace
 * number.
 */
const WORKSPACE_SCOPED_TOOLS: ReadonlySet<string> = new Set([
  "ls", "grep", "find", "read", "write", "edit", "lsp"
]);

/** The backend a shell tool runs in. Mirrors `ShellBackend::of_tool`. */
function shellOfTool(toolName: string): string | null {
  const match = /^(bash|zsh|sh|powershell)$/.exec(toolName);
  return match ? match[1]! : null;
}

/**
 * The descriptors a timeline's manual tool cards are edited against, with the
 * `workspace` argument the host adds on the wire when the conversation has
 * more than one workspace. The static descriptors cannot carry it: which
 * numbers exist is a property of the conversation, not of the tool, and a
 * card placed by hand has to be able to name workspace 2 the same way the
 * model does.
 *
 * The argument is optional and starts empty, so an untouched field records
 * nothing and the host applies its own default of 1. A shell tool lists only
 * the workspaces whose machine has its shell — `shellsAt` answers that from
 * the machines' probes — and with none, the host withdraws the tool from the
 * wire and the descriptor is left alone.
 */
export function withWorkspaceArgument(
  tools: ToolDescriptor[],
  workspaces: readonly AttachedWorkspace[],
  shellsAt: (machine: RunTarget | null) => readonly string[],
  label: string
): ToolDescriptor[] {
  if (workspaces.length < 2) return tools;
  const all = workspaces.map((_, position) => position + 1);
  const shells = workspaces.map((workspace) => shellsAt(workspace.machine ?? null));
  return tools.map((tool) => {
    const shell = shellOfTool(tool.name);
    if (!shell && !WORKSPACE_SCOPED_TOOLS.has(tool.name)) return tool;
    if (tool.parameters.some((parameter) => parameter.name === "workspace")) return tool;
    const addresses = shell
      ? all.filter((address) => shells[address - 1]!.includes(shell))
      : all;
    if (addresses.length === 0) return tool;
    return {
      ...tool,
      parameters: [
        ...tool.parameters,
        {
          name: "workspace",
          label,
          type: "number",
          required: false,
          placeholder: addresses.join(" | ")
        }
      ]
    };
  });
}

/**
 * Identifies the entry point that creates a conversation. The sidebar's New Task,
 * Ctrl+N, and empty-state button use `global` and always apply the global default
 * preset. A workspace title-row `+` uses `workspace`, first checking its default
 * preset, then its remembered conversation settings.
 */
export type NewConversationSource = "global" | "workspace";

export function isTemporaryWorkspace(workspace: Workspace | null | undefined): boolean {
  return workspace?.kind === "temporary" || workspace?.id === TEMPORARY_WORKSPACE_ID;
}

export function isReservedWorkspace(workspace: Workspace | null | undefined): boolean {
  return isTemporaryWorkspace(workspace);
}

/**
 * The temporary project always closes the list: projects are ordered among themselves, and
 * the scratch space they all fall back to stays at the bottom whatever is added or moved.
 * Returns the same array when it is already in that order.
 */
export function withTemporaryWorkspaceLast(workspaces: Workspace[]): Workspace[] {
  const temporaryIndex = workspaces.findIndex(isTemporaryWorkspace);
  if (temporaryIndex < 0 || temporaryIndex === workspaces.length - 1) return workspaces;
  return [
    ...workspaces.filter((workspace) => !isTemporaryWorkspace(workspace)),
    ...workspaces.filter(isTemporaryWorkspace)
  ];
}

export function createTemporaryWorkspace(conversations: Workspace["conversations"] = []): Workspace {
  return {
    id: TEMPORARY_WORKSPACE_ID,
    name: "临时工作区",
    kind: "temporary",
    path: "",
    createdAt: new Date().toISOString(),
    defaultConversationPresetId: "",
    lastConversationSettings: null,
    conversations
  };
}
