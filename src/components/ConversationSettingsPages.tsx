import {
  Blocks,
  Bot,
  Box,
  CornerDownLeft,
  FolderCog,
  FolderOpen,
  Home,
  Pencil,
  Plug,
  Plus,
  RefreshCw,
  ScrollText,
  Search,
  Settings2,
  SlidersHorizontal
} from "lucide-react";
import { Fragment, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";
import { type TranslationFunction, useI18n } from "../i18n";
import { agentModelSelectionIsAvailable, hasUsableAgentRole } from "../lib/agentRoles";
import { useCatalogOrder } from "../lib/catalogOrder";
import { isBuiltinConversationPreset } from "../lib/conversationPresets";
import { modelChoiceOf } from "../lib/documentUpdates";
import { supportsVision } from "../lib/modelCapabilities";
import { saveAgentRole, type SaveAgentRoleTarget } from "../lib/runtime";
import { isImeKeyEvent } from "../lib/shortcuts";
import type { LockTone } from "../lib/toolLock";
import {
  type CapabilityWorkspace,
  parseCapabilityWorkspaceKey,
  workspaceLocationTitle,
  workspaceMachineLabel
} from "../lib/workspaces";
import type {
  AgentModelSelection,
  AgentRole,
  AgentRoleResource,
  ApiProvider,
  CapabilityCatalog,
  CapabilityResourceKind,
  ContextItem,
  ConversationPreset,
  ConversationSettings as ConversationSettingsType,
  ConversationTemplateSummary,
  GlobalSettings,
  McpProbeReport,
  ResourceDescriptor,
  RunTarget,
  SshMachineConfig,
  ToolDescriptor
} from "../types";
import { AgentRoleEditor, forgetAgentRoleDraft } from "./AgentRoleEditor";
import { CatalogList, CatalogRow, CatalogToggleRow, type CatalogSort, useCatalogSort } from "./CatalogRow";
import { ConfirmDeleteButton, IconButton, Switch } from "./Common";
import { DocsLink } from "./DocsLink";
import type { LockHints } from "./LockTone";
import { PathText } from "./PathText";
import { machineIcon } from "./ProjectChips";

/**
 * Said of a native backend a run has already used.
 *
 * Not the same claim as the hint above: nothing is being withheld, the choice
 * is simply settled. A native search seals its results in blocks only its own
 * provider's models can read back, so a different backend cannot take over
 * half way through. A host-run backend leaves ordinary tool results and is
 * never settled this way — only the cache says anything about moving it.
 */
export function settledBackendHint(t: TranslationFunction): string {
  return t(
    "本对话已经用原生后端跑过了；它的结果封在只有该提供商的模型才读得回的块里，所以不能中途换人。开一段新对话可以重选。",
    "This conversation has already run on the native backend. Its results are sealed in blocks only that provider's models can read back, so it cannot be swapped part-way through — start a new conversation to choose again."
  );
}

const KIND_ICONS = {
  skills: Box,
  mcp: Blocks,
  hooks: FolderCog,
  agents: Bot,
  toolDescriptions: ScrollText
} as const satisfies Record<CapabilityResourceKind | "toolDescriptions", typeof Box>;

/**
 * Everything a row no longer prints, joined into the tooltip it now rides in.
 *
 * A catalog row is one line, so a description, a path on disk, and a reason the
 * entry is inert all have to share `title`. They are still the only way to tell
 * two same-named resources apart, which is why they are kept at all rather than
 * dropped with the second line they used to occupy.
 */
function rowDetail(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join("\n");
}

const resourceId = (resource: ResourceDescriptor) => resource.id;

/**
 * What a connection test on an MCP row currently knows.
 *
 * Held as a whole per resource id so a re-test replaces the last result rather
 * than layering onto it: a row shows one thing at a time.
 */
type McpProbeState =
  | { status: "running" }
  | { status: "ok"; toolCount: number; detail: string }
  | { status: "failed"; detail: string };

/**
 * One section of a capability page: the global level or one workspace, with the
 * place it reads from and the button that opens it in its heading. Each section
 * is its own sortable list, so a row is only ever dragged among the rows of the
 * level it was read from — the catalog's order is one, but a row can never leave
 * its level.
 */
function CapabilitySection({
  listId,
  label,
  heading,
  ids,
  sortable,
  onReorder,
  notice,
  empty,
  children
}: {
  listId: string;
  label: string;
  heading: ReactNode;
  ids: string[];
  sortable: boolean;
  onReorder: (sourceId: string, targetId: string, position: "before" | "after") => void;
  /** Why the section's own entries are missing, when its machine could not be read. */
  notice?: string | null;
  /** Said under the heading when the level holds nothing of this kind. */
  empty?: string | null;
  children: (sort: CatalogSort) => ReactNode;
}) {
  const sort = useCatalogSort({ listId, ids, enabled: sortable, onReorder });
  return (
    <section className="capability-section" aria-label={label}>
      <div className="capability-section__heading">{heading}</div>
      {notice && <p className="field__hint field__hint--error">{notice}</p>}
      {ids.length > 0 && <CatalogList sort={sort}>{children(sort)}</CatalogList>}
      {!ids.length && empty && <p className="capability-section__empty">{empty}</p>}
    </section>
  );
}

/**
 * One page of the conversation-settings pane for a catalog of named things —
 * skills, MCP servers, hooks, subagent roles, or tool-description files.
 *
 * The catalogs differ only in what they are called and which of them a run
 * can lock, so they share one page rather than near-copies. Each entry is
 * a single line that is itself the switch, picked the way a tool is in the tool
 * list, and, where the entry is the user's own to remove, carries the two-step
 * delete on the right. The rows can be dragged
 * into whatever order the user reads them in — see `useCatalogOrder` for why that
 * order is a view preference rather than part of the catalog.
 *
 * The page is divided by where the entries were read: the global `~/.mewrk`
 * first, then each of the conversation's workspaces, headed by its absolute path
 * and its machine. A conversation uses the union of them all. A row states its
 * name and nothing more about where it came from: its section says that, and the
 * path is in the tooltip.
 *
 * `workspaces` is the conversation's own list; undefined means the preset editor,
 * which is reusable and points at no conversation in particular, so it shows a
 * section for every workspace the catalog has entries of. A catalog that only
 * has a global level (tool-description files) passes `[]`.
 *
 * `single` makes the page pick at most one entry: turning one on turns the
 * selected one off. It is for a catalog whose entries replace one another
 * rather than add up — a conversation renders with one tool-description file —
 * and selecting none is then that catalog's default, not an empty set.
 *
 * A catalog whose rows have more to say than a name — a role's model, the button
 * that opens a role — says it through the optional `row*` props rather than
 * through a page of its own.
 */
export function CapabilitySelectionPage<
  R extends ResourceDescriptor = ResourceDescriptor,
  Kind extends keyof typeof KIND_ICONS = keyof typeof KIND_ICONS
>({
  kind,
  listId,
  resources,
  selectedIds,
  single = false,
  toneOf,
  lockHints,
  onChange,
  onUntickDangling,
  onDelete,
  error = null,
  unreadableLevels = [],
  danglingDetail: danglingDetailText,
  selectedUnavailableDetail,
  searchLabel,
  emptyTitle,
  emptyDescription,
  footer = null,
  workspaces,
  sshMachines = [],
  onRescan,
  onReveal,
  onProbeMcpServer,
  rowActions,
  rowDetail: rowDetailOf,
  rowBadge,
  rowLabel
}: {
  kind: Kind;
  /** Unique among the lists on screen: the preset editor mounts a second copy. */
  listId: string;
  resources: R[];
  selectedIds: string[];
  /** At most one entry is selected; turning one on replaces the other. */
  single?: boolean;
  /** How the conversation's lock draws a row (`lockTone` in `toolLock.ts`). */
  toneOf?: (id: string, selected: boolean) => LockTone | null;
  /** What a toned row says about itself. */
  lockHints?: LockHints;
  onChange: (ids: string[]) => void;
  /**
   * Unticks a dangling selection, past the lock: a skill, server or hook that
   * is selected and gone fails every run, so clearing it is never toned, never
   * asks and is never refused (`withoutDanglingSelection`). Without it a
   * dangling row goes through `onChange` like any other.
   */
  onUntickDangling?: (id: string) => void;
  /**
   * Removes the entry from the catalog itself — the skill folder, the server
   * record, the line in `hooks.json` — not merely from this conversation. Omit
   * it where the caller has nowhere to route that. A caller that waits on the
   * removal may return a promise; nothing here reads its value.
   */
  onDelete?: (resource: R) => unknown;
  /** The last failed delete, shown on the page the row is on. */
  error?: string | null;
  /**
   * Workspaces whose machine could not be read, with why, so a selection of one
   * of their entries does not read as deleted. Each is said in its own section.
   */
  unreadableLevels?: ReadonlyArray<{ workspaceKey: string; message: string }>;
  /**
   * What a dangling selection, and a selected entry that cannot be used, say
   * about the run. By default both fail every run; a catalog whose host falls
   * back to a default instead says that.
   */
  danglingDetail?: string;
  selectedUnavailableDetail?: string;
  searchLabel: string;
  emptyTitle: string;
  emptyDescription: string;
  /**
   * Page-specific controls below the list, for a policy that governs the whole
   * catalog rather than one entry. Drawn whether or not the catalog has
   * anything in it: a policy that only appears once a row exists is a policy
   * the user cannot find out about beforehand.
   */
  footer?: ReactNode;
  /**
   * The conversation's workspaces (`capabilityWorkspaces`), each a section after
   * the global one. `[]` (a draft with no project yet) keeps the global level
   * alone; undefined shows every workspace the catalog has entries of (the
   * preset editor).
   */
  workspaces?: readonly CapabilityWorkspace[];
  /** For naming the SSH machine a workspace is on. */
  sshMachines?: readonly SshMachineConfig[];
  /** Re-runs discovery on disk. Drawn as a toolbar button when provided. */
  onRescan?: () => void | Promise<void>;
  /**
   * Opens a level's configuration folder: `null` is the global `~/.mewrk`, a
   * workspace key that workspace's `.mewrk`. Drawn in each section's heading.
   */
  onReveal?: (kind: Kind, workspaceKey: string | null) => void;
  /** Tests an MCP server's connection. Only meaningful on the MCP page. */
  onProbeMcpServer?: (resource: R) => Promise<McpProbeReport>;
  /** Row actions of the page's own, drawn before the delete. */
  rowActions?: (resource: R) => ReactNode;
  /** A line of the page's own for the row's tooltip, after the description. */
  rowDetail?: (resource: R) => string | null | undefined;
  /**
   * A badge of the page's own, for a row that is available but still has
   * something to warn about. An unavailable row and a probe result take the
   * slot first: the badge is one slot with one meaning at a time.
   */
  rowBadge?: (resource: R) => ReactNode;
  /**
   * What the row's switch is called, where the name does not tell two rows
   * apart. Defaults to the name.
   */
  rowLabel?: (resource: R) => string | undefined;
}) {
  const { t } = useI18n();
  const [query, setQuery] = useState("");
  /* One result per resource id, replaced wholesale by the next test. */
  const [probes, setProbes] = useState<Record<string, McpProbeState>>({});
  const Icon: typeof Box = KIND_ICONS[kind];
  const selected = useMemo(() => new Set(selectedIds), [selectedIds]);
  const tone = (id: string) => toneOf?.(id, selected.has(id)) ?? null;
  /* The workspaces drawn as sections. The preset editor has no conversation, so
     it draws every workspace the catalog read something from (or tried to). */
  const sections = useMemo((): Array<CapabilityWorkspace | { number: null; key: string; machine: RunTarget | null; path: string }> => {
    if (workspaces) return [...workspaces];
    const keys = [
      ...resources.map((resource) => resource.workspaceKey),
      ...unreadableLevels.map((level) => level.workspaceKey)
    ].filter((key): key is string => Boolean(key));
    return [...new Set(keys)].map((key) => ({ number: null, key, ...parseCapabilityWorkspaceKey(key) }));
  }, [resources, unreadableLevels, workspaces]);
  /* The entries this conversation may select: the global level (no workspace) and
     each of its workspaces. Anything else belongs to a workspace this conversation
     does not have — a run cannot reach it, so a selection of one (a preset made
     elsewhere) shows as dangling, as the host treats it. */
  const scoped = useMemo(() => {
    if (!workspaces) return resources;
    const keys = new Set(workspaces.map((workspace) => workspace.key));
    return resources.filter((resource) => !resource.workspaceKey || keys.has(resource.workspaceKey));
  }, [resources, workspaces]);
  const { ordered, reorder } = useCatalogOrder(kind, scoped, resourceId);
  /* A selection whose resource has left the catalog is kept, not dropped: the
     scan may simply not have reached it yet, so it stays as a checked row the
     user can clear on purpose. */
  const danglingIds = useMemo(() => {
    const known = new Set(scoped.map((resource) => resource.id));
    return selectedIds.filter((id) => !known.has(id));
  }, [scoped, selectedIds]);

  const needle = query.trim().toLowerCase();
  const matches = (haystack: string) => haystack.toLowerCase().includes(needle);
  const visible = needle
    ? ordered.filter((resource) => (
      matches(resource.name) || matches(resource.description) || matches(resource.location)
    ))
    : ordered;
  const visibleDangling = needle ? danglingIds.filter(matches) : danglingIds;
  const rowsOf = (key: string | null) => visible.filter((resource) => (resource.workspaceKey ?? null) === key);
  /* Numbers are what the model addresses workspaces by, stated to it only once
     there is more than one; a lone workspace's "1" would name nothing it sees. */
  const numbered = sections.length > 1;

  const toggle = (id: string, checked: boolean) => {
    if (single) onChange(checked ? [id] : []);
    else onChange(checked
      ? [...selectedIds.filter((existing) => existing !== id), id]
      : selectedIds.filter((existing) => existing !== id));
  };

  const probeServer = async (resource: R) => {
    if (!onProbeMcpServer) return;
    setProbes((current) => ({ ...current, [resource.id]: { status: "running" } }));
    let next: McpProbeState;
    try {
      const report = await onProbeMcpServer(resource);
      next = report.ok
        ? {
          status: "ok",
          toolCount: report.tools.length,
          /* The row is one line, so who answered has to ride the tooltip. */
          detail: [report.serverName, report.serverVersion].filter(Boolean).join(" ")
        }
        : {
          status: "failed",
          /* A failed probe says why, and then what the server said while it tried:
             the last few stderr lines are usually the whole diagnosis. */
          detail: [report.error, ...report.logs.slice(-5)].filter(Boolean).join("\n")
        };
    } catch (reason) {
      next = { status: "failed", detail: String(reason) };
    }
    setProbes((current) => ({ ...current, [resource.id]: next }));
  };

  /* A missing skill, server or hook fails every run, just as a selected one
     that cannot be used does: the host names it and refuses to start until it is
     unticked. A role is the exception — the host simply does not offer one it
     cannot find or read to the model — so its rows say that instead of
     promising a failure. The caller says otherwise where the host falls back
     instead. */
  const danglingDetail = danglingDetailText ?? (kind === "agents"
    ? t(
      "找不到这个角色的文件，模型看不到它；可以取消勾选。",
      "This role's file is gone, so the model does not see it; untick it."
    )
    : t(
      "目录中已不存在；取消勾选之前，每次运行都会失败",
      "No longer in the catalog. Every run fails until it is unchecked."
    ));
  const unusableSelectedDetail = selectedUnavailableDetail ?? (kind === "agents"
    ? t(
      "已选择，但文件无法使用，模型看不到它；修好文件或取消勾选。",
      "Selected, but the file cannot be used, so the model does not see it; fix the file or untick it."
    )
    : t(
      "已选择；修好或取消勾选之前，每次运行都会失败",
      "Selected. Every run fails until it is fixed or unchecked."
    ));
  const untickDangling = (id: string) => {
    if (onUntickDangling) onUntickDangling(id);
    else onChange(selectedIds.filter((existing) => existing !== id));
  };

  const renderRow = (resource: R, sort: CatalogSort) => {
    const isSelected = selected.has(resource.id);
    const rowTone = tone(resource.id);
    /* An entry the last request carried is not deleted out from under it. */
    const inPlay = isSelected && rowTone !== null;
    const probe = probes[resource.id];
    /* The badge is one slot with one meaning: what a test just said, or —
       where there is no test to report — that the entry cannot run. An
       unavailable row is never probed, so the two never compete. */
    const badge = probe?.status === "running"
      ? <em className="catalog-row__badge">{t("测试中…", "Testing…")}</em>
      : probe?.status === "ok"
        ? <em className="catalog-row__badge">{t(
          "{count} 个工具",
          "{count} tools",
          { count: probe.toolCount }
        )}</em>
        : probe?.status === "failed"
          ? <em className="catalog-row__badge catalog-row__badge--warning">{t("连接失败", "Connection failed")}</em>
          : !resource.available
            ? <em className="catalog-row__badge catalog-row__badge--warning">{t("不可用", "Unavailable")}</em>
            : rowBadge?.(resource) ?? undefined;
    const actions: ReactNode[] = [];
    /* A connection test is only offered where there is something to connect
       to: an unavailable entry already carries the reason it cannot. */
    if (kind === "mcp" && onProbeMcpServer && resource.available) {
      actions.push(
        <IconButton
          key="probe"
          /* Named after its row, like the delete button beside it: a list of
             identical "Test connection" buttons has no accessible name. */
          label={t("测试连接 {name}", "Test connection to {name}", { name: resource.name })}
          disabled={probe?.status === "running"}
          onClick={() => void probeServer(resource)}
        ><Plug size={13} /></IconButton>
      );
    }
    const ownActions = rowActions?.(resource);
    if (ownActions) actions.push(<Fragment key="own">{ownActions}</Fragment>);
    /* A built-in entry has no copy of its own to remove, and one the last
       request carried is still in the model's hands. */
    if (onDelete && resource.source !== "builtin" && !inPlay) {
      actions.push(
        <ConfirmDeleteButton
          key="delete"
          label={t("删除 {name}", "Delete {name}", { name: resource.name })}
          confirmLabel={t("确认删除 {name}", "Confirm deleting {name}", { name: resource.name })}
          onDelete={() => onDelete(resource)}
        />
      );
    }
    return (
      <CatalogToggleRow
        key={resource.id}
        id={resource.id}
        sort={sort}
        name={resource.name}
        label={rowLabel?.(resource)}
        detail={rowDetail(
          resource.description,
          rowDetailOf?.(resource),
          resource.location,
          rowTone && lockHints?.[rowTone],
          /* The scope badge is gone, so the one thing it said that the path
             does not say moves here. */
          !resource.available && t("当前不可用", "Currently unavailable"),
          !inPlay && !resource.available && isSelected && unusableSelectedDetail,
          probe?.status === "ok" && probe.detail,
          probe?.status === "failed" && probe.detail
        )}
        /* A probe result and an unavailable entry both ride this slot; they
           are mutually exclusive because an unavailable row is not probed. */
        badge={badge}
        actions={actions.length ? actions : undefined}
        checked={isSelected}
        tone={rowTone}
        onChange={(checked) => toggle(resource.id, checked)}
      />
    );
  };

  /* Said in a section with nothing of its own while another section has
     something; an empty catalog says it once, below them all. */
  const empty = scoped.length || danglingIds.length ? t("这里还没有条目", "Nothing here yet") : null;
  const globalRows = rowsOf(null);
  const sectionCount = (rows: R[]) => (
    <small className="capability-section__count">
      {rows.filter((resource) => selected.has(resource.id)).length} / {rows.length}
    </small>
  );

  return (
    <>
      {error && <p className="field__hint field__hint--error">{error}</p>}

      <div className="capability-page__toolbar">
        <div className="capability-page__search">
          <Search size={13} aria-hidden="true" />
          <input
            aria-label={searchLabel}
            placeholder={searchLabel}
            value={query}
            onChange={(event) => setQuery(event.target.value)}
          />
        </div>
        <span className="capability-page__count">{t(
          "{selected} / {total} 个已选",
          "{selected} / {total} selected",
          {
            /* Counted over the same population it divides by. A dangling selection is
               still a real row below, but counting it here would read "2 / 1". */
            selected: scoped.filter((resource) => selected.has(resource.id)).length,
            total: scoped.length
          }
        )}</span>
        {/* How to look at the catalog again: re-read the disk. Each section's
            heading opens the folder it was read from. */}
        {onRescan && (
          <div className="capability-page__toolbar-actions">
            <IconButton
              label={t("重新扫描", "Rescan")}
              onClick={() => void onRescan()}
            ><RefreshCw size={13} /></IconButton>
          </div>
        )}
        <DocsLink page={kind} />
      </div>

      <div className="capability-sections">
        {(!needle || globalRows.length > 0) && (
          <CapabilitySection
            listId={`${listId}:global`}
            label={t("全局", "Global")}
            heading={<>
              <Home size={13} aria-hidden="true" />
              <span className="capability-section__label">{t("全局", "Global")}</span>
              <small className="capability-section__where">~/.mewrk</small>
              {sectionCount(globalRows)}
              {onReveal && (
                <IconButton
                  label={t("打开全局配置目录", "Open the global config folder")}
                  onClick={() => onReveal(kind, null)}
                ><FolderOpen size={13} /></IconButton>
              )}
            </>}
            ids={globalRows.map(resourceId)}
            sortable={!needle}
            onReorder={reorder}
            empty={needle ? null : empty}
          >
            {(sort) => globalRows.map((resource) => renderRow(resource, sort))}
          </CapabilitySection>
        )}
        {sections.map((workspace) => {
          const rows = rowsOf(workspace.key);
          if (needle && !rows.length) return null;
          const machine = workspaceMachineLabel(workspace.machine, sshMachines);
          return (
            <CapabilitySection
              key={workspace.key}
              listId={`${listId}:${workspace.key}`}
              label={workspaceLocationTitle(workspace.path, workspace.machine, sshMachines)}
              heading={<>
                {machineIcon(workspace.machine, 13)}
                {numbered && workspace.number !== null && (
                  <span className="capability-section__number" aria-hidden="true">{workspace.number}</span>
                )}
                <PathText className="capability-section__label" path={workspace.path} />
                {machine && <small className="capability-section__where">{machine}</small>}
                {sectionCount(rows)}
                {onReveal && (
                  <IconButton
                    label={t("打开 {path} 的配置目录", "Open the config folder of {path}", { path: workspace.path })}
                    onClick={() => onReveal(kind, workspace.key)}
                  ><FolderOpen size={13} /></IconButton>
                )}
              </>}
              ids={rows.map(resourceId)}
              sortable={!needle}
              onReorder={reorder}
              notice={unreadableLevels.find((level) => level.workspaceKey === workspace.key)?.message}
              empty={needle ? null : empty}
            >
              {(sort) => rows.map((resource) => renderRow(resource, sort))}
            </CapabilitySection>
          );
        })}

        {/* Never toned: the entry it named has nothing left to declare, so
            unticking it rewrites no cache. */}
        {visibleDangling.length > 0 && (
          <CatalogList>
            {visibleDangling.map((id) => (
              <CatalogToggleRow
                key={id}
                name={id}
                detail={danglingDetail}
                badge={<em className="catalog-row__badge catalog-row__badge--warning">{t("悬空", "Dangling")}</em>}
                checked
                tone={null}
                onChange={(checked) => {
                  if (!checked) untickDangling(id);
                }}
              />
            ))}
          </CatalogList>
        )}

        {!visible.length && !visibleDangling.length && (
          <div className="capability-page__empty">
            {needle
              ? <>
                <Search size={18} aria-hidden="true" />
                <strong>{t("没有匹配的条目", "No matching entry")}</strong>
                <span>{t(
                  "搜索会匹配名称、说明与所在路径。",
                  "Search matches names, descriptions and paths."
                )}</span>
              </>
              : <>
                <Icon size={18} aria-hidden="true" />
                <strong>{emptyTitle}</strong>
                <span>{emptyDescription}</span>
              </>}
          </div>
        )}
      </div>
      {footer}
    </>
  );
}

/**
 * The row's model, said the way the host will resolve it. `inherit` has no model
 * ID of its own to print — it rides whatever model the calling conversation is on
 * — so it says so rather than inventing one; `unavailable` has no ID left to
 * print either. A bound pair names its provider by display name, never by the
 * random per-installation `providerId`, which reads as a hash; a provider
 * deleted outright leaves the model ID standing alone.
 */
export function agentRoleModelLabel(
  selection: AgentModelSelection,
  providers: readonly ApiProvider[],
  t: TranslationFunction
): string {
  if (selection.kind === "inherit") return t("跟随对话模型", "Follows the conversation's model");
  if (selection.kind === "unavailable") {
    return t("没有可用的模型，模型看不到这个角色", "No model to run on; hidden from the model");
  }
  const provider = providers.find((candidate) => candidate.id === selection.providerId);
  const label = provider ? `${provider.name} · ${selection.modelId}` : selection.modelId;
  if (!agentModelSelectionIsAvailable(selection, providers)) {
    return t(
      "{model} · 模型暂时取不到，模型看不到这个角色",
      "{model} · model unavailable for now, hidden from the model",
      { model: label }
    );
  }
  return label;
}

/**
 * Which role window is open over the page, if any. An edit holds the entry as it
 * was opened: the window is about that file, and a rescan that no longer finds
 * it must not close the window under the user — the save says so instead.
 */
type OpenRoleEditor = { mode: "create" } | { mode: "edit"; resource: AgentRoleResource };

/**
 * The agent-roles page.
 *
 * A role is a file like a skill is — `~/.mewrk/agents/<file>.json`, a
 * workspace's `.mewrk/agents/<file>.json`, or one of the built-ins — so the page
 * is the same catalog page the skills are drawn on, and the row's switch means
 * the same thing there: this conversation offers that role to the model. What a
 * role page adds is the row's model, the button that opens the role in its own
 * window, and a way to write a new file without leaving the pane. The one policy
 * that governs all of them — whether a subagent may decline to name a role at all
 * — sits below the list, read after the roles it qualifies, and is always drawn
 * so the policy is legible even when no role exists yet.
 *
 * Roles take part in no lock: a role's settings are no part of the calling
 * conversation's prompt cache, so a change simply reaches the next subagent.
 */
export function AgentRolesPage({
  listId,
  settings,
  globalSettings,
  roleTools,
  catalog,
  templates,
  presets,
  onReadTemplate,
  onWriteTemplate,
  workspaces,
  sshMachines = [],
  onRescan,
  onReveal,
  onProbeMcpServer,
  onDelete,
  onSaveRole,
  onUntickDangling,
  error = null,
  onChange
}: {
  /** Unique among the lists on screen: the preset editor mounts a second copy. */
  listId: string;
  settings: ConversationSettingsType;
  globalSettings: GlobalSettings;
  /**
   * The whole trusted tool catalogue, for the role window's tool picker and the
   * tool list a new role starts with. Never a conversation's own list: that one
   * is narrowed to the shells its machines have, and a role is a global or
   * project asset that every conversation may select — on a machine without
   * `zsh` it still has to be able to carry `zsh` for the ones that have it.
   */
  roleTools: ToolDescriptor[];
  /** The whole catalog: the roles, and what a role's own skill, MCP and hook pages pick out of. */
  catalog: CapabilityCatalog;
  templates: ConversationTemplateSummary[];
  /** Offered on a role's template page as bodies to copy over its own. */
  presets: readonly ConversationPreset[];
  onReadTemplate: (templateId: string) => Promise<ContextItem[]>;
  onWriteTemplate: (templateId: string, contexts: ContextItem[]) => Promise<string>;
  /** As on the other catalog pages: the conversation's workspaces, or undefined for a preset's. */
  workspaces?: readonly CapabilityWorkspace[];
  sshMachines?: readonly SshMachineConfig[];
  onRescan?: () => void | Promise<void>;
  onReveal?: (kind: CapabilityResourceKind, workspaceKey: string | null) => void;
  onProbeMcpServer?: (resource: ResourceDescriptor) => Promise<McpProbeReport>;
  /**
   * Deletes a role's file. Never offered for a built-in. A promise resolving
   * `false` says the file is still there, and the role's remembered draft is
   * then kept; anything else counts as removed.
   */
  onDelete?: (resource: AgentRoleResource) => Promise<boolean> | undefined;
  /**
   * Writes a role file and resolves with the id it landed under, once the
   * catalog has been rescanned. Omitted, the host is asked directly and the page
   * rescans through `onRescan`.
   */
  onSaveRole?: (target: SaveAgentRoleTarget, role: AgentRole) => Promise<string>;
  onUntickDangling?: (id: string) => void;
  /** The last failed delete or rescan, shown on the page. */
  error?: string | null;
  onChange: (patch: Partial<ConversationSettingsType>) => void;
}) {
  const { t } = useI18n();
  const [editor, setEditor] = useState<OpenRoleEditor | null>(null);
  const providers = globalSettings.apiProviders;
  /* A save resolves after the catalog has been rescanned, and the selection it
     then writes must land on the settings as they are by then, not as they were
     when Save was pressed. */
  const latest = useRef({ agentIds: settings.agentIds, onChange });
  latest.current = { agentIds: settings.agentIds, onChange };
  /* Unlike the page's count, this asks whether the model can name a role at all:
     a selected role of a level this conversation reaches, whose file could be
     read and whose model resolves. */
  const workspaceKeys = useMemo(
    () => workspaces?.map((workspace) => workspace.key),
    [workspaces]
  );
  const hasUsableRole = useMemo(
    () => hasUsableAgentRole(catalog.agents, settings.agentIds, providers, workspaceKeys),
    [catalog.agents, settings.agentIds, providers, workspaceKeys]
  );
  /* What a role bound to "inherit" will run on, so its template page knows
     whether a message written there may carry an image. */
  const conversationImageInputSupported = useMemo(() => {
    const model = modelChoiceOf(globalSettings).model;
    return Boolean(model && supportsVision(model));
  }, [globalSettings]);

  const saveRole = onSaveRole ?? (async (target: SaveAgentRoleTarget, role: AgentRole) => {
    const id = await saveAgentRole(target, role);
    await onRescan?.();
    return id;
  });
  /* A role written from here is meant to be used here: creating one also selects
     it in this conversation — or in the preset this pane is a window onto. A copy
     of a built-in takes the built-in's place when that was selected, so saving a
     changed built-in does not leave both offered under one name. */
  const selectCreated = (id: string, replaces: string | null) => {
    const { agentIds, onChange: write } = latest.current;
    const next = replaces && agentIds.includes(replaces)
      ? [...new Set(agentIds.map((existing) => (existing === replaces ? id : existing)))]
      : [...agentIds.filter((existing) => existing !== id), id];
    write({ agentIds: next });
  };

  return (
    <>
      <CapabilitySelectionPage<AgentRoleResource, "agents">
        kind="agents"
        listId={listId}
        resources={catalog.agents}
        selectedIds={settings.agentIds}
        onChange={(agentIds) => onChange({ agentIds })}
        onUntickDangling={onUntickDangling}
        /* A draft of a role whose file is gone would reopen on nothing — but
           only once the file is known to be gone: a delete the host refused
           leaves the role, and the half-written edit on it, in place. */
        onDelete={onDelete && (async (resource) => {
          if (await onDelete(resource) !== false) forgetAgentRoleDraft(resource.id);
        })}
        error={error}
        unreadableLevels={catalog.unreadableLevels ?? []}
        workspaces={workspaces}
        sshMachines={sshMachines}
        onRescan={onRescan}
        onReveal={onReveal}
        searchLabel={t("搜索角色", "Search roles")}
        emptyTitle={t("尚未发现角色", "No roles discovered")}
        emptyDescription={t(
          "点下面的「新建角色」，或把角色 JSON 文件放进 ~/.mewrk/agents/ 或工作区的 .mewrk/agents/，列表很快会自动刷新。",
          "Press New role below, or put role JSON files in ~/.mewrk/agents/ or the workspace's .mewrk/agents/; the list refreshes by itself shortly after."
        )}
        rowDetail={(resource) => resource.role
          ? agentRoleModelLabel(resource.role.modelSelection, providers, t)
          : null}
        rowBadge={(resource) => {
          const builtin = resource.source === "builtin";
          const noModel = resource.role !== null
            && !agentModelSelectionIsAvailable(resource.role.modelSelection, providers);
          if (!builtin && !noModel) return null;
          return <>
            {/* A copy of a built-in is a global role of the same name, so the
                two rows have to be told apart by more than their position. */}
            {builtin && <em className="catalog-row__badge">{t("内置", "Built-in")}</em>}
            {noModel && <em className="catalog-row__badge catalog-row__badge--warning">{t("模型不可用", "No model")}</em>}
          </>;
        }}
        rowLabel={(resource) => (resource.source === "builtin"
          ? t("{name}（内置）", "{name} (built-in)", { name: resource.name })
          : undefined)}
        /* An unreadable file has no body to open; its reason is in the tooltip,
           and the file is fixed where it lives. */
        rowActions={(resource) => resource.role && (
          <IconButton
            label={resource.source === "builtin"
              ? t("设置内置角色 {name}", "Configure built-in role {name}", { name: resource.name })
              : t("设置角色 {name}", "Configure role {name}", { name: resource.name })}
            onClick={() => setEditor({ mode: "edit", resource })}
          ><Settings2 size={13} /></IconButton>
        )}
        footer={(
          <>
            {/* Below the list, where the reader lands after the roles
                themselves: a new role is a new file, written at the level the
                window asks for. */}
            <button
              type="button"
              className="agent-role-add"
              onClick={() => setEditor({ mode: "create" })}
            >
              <Plus size={14} aria-hidden="true" />
              {t("新建角色", "New role")}
            </button>

            {/* With no usable role the host allows role-less execution
                regardless, so the switch reads as a statement of intent rather
                than a live control — but it stays on screen so the policy is
                never invisible. */}
            <div className="tool-toggle-row">
              <span><strong>{t("允许无角色子代理", "Allow role-less subagents")}</strong><small>{hasUsableRole
                ? t(
                  "关闭时 agent_spawn 与 workflow 步骤都必须点名一个角色。开启则恢复旧语义：省略角色的子代理沿用本对话的模型。",
                  "When off, agent_spawn and workflow steps must both name a role. When on, the old behaviour returns: a subagent that names none inherits this conversation's model."
                )
                : t(
                  "当前没有可用角色，宿主一律允许无角色执行，这个开关要等到有可用角色后才生效。",
                  "No usable role exists, so the host allows role-less execution either way; this switch takes effect once one does."
                )}</small></span>
              <Switch
                checked={Boolean(settings.allowRolelessSubagents)}
                onChange={(allowRolelessSubagents) => onChange({ allowRolelessSubagents })}
                label={settings.allowRolelessSubagents
                  ? t("角色可选", "Role optional")
                  : t("角色必填", "Role required")}
              />
            </div>
          </>
        )}
      />

      {editor && (
        <AgentRoleEditor
          /* A different role is a different window: nothing typed into one may
             leak into the next. */
          key={editor.mode === "create" ? "create" : editor.resource.id}
          resource={editor.mode === "edit" ? editor.resource : null}
          catalog={catalog}
          providers={providers}
          activeProviderId={globalSettings.activeProviderId}
          tools={roleTools}
          conversationImageInputSupported={conversationImageInputSupported}
          webSearchAssets={globalSettings.webSearch}
          templates={templates}
          presets={presets}
          onReadTemplate={onReadTemplate}
          onWriteTemplate={onWriteTemplate}
          workspaces={workspaces}
          sshMachines={sshMachines}
          onRescan={onRescan}
          onReveal={onReveal}
          onProbeMcpServer={onProbeMcpServer}
          onSave={saveRole}
          onCreated={selectCreated}
          onClose={() => setEditor(null)}
        />
      )}
    </>
  );
}

const presetId = (preset: ConversationPreset) => preset.id;

/**
 * The conversation-presets page.
 *
 * A preset is a saved copy of everything else in this pane, so its settings
 * button opens this same pane on that copy rather than a second, smaller editor
 * that would have to be kept in step with it. The arrow in the left slot stamps
 * the copy onto this conversation and is a one-time action, which is why the
 * applied row is marked rather than tethered — see the preset trace on the
 * conversation itself.
 */
export function ConversationPresetsPage({
  listId,
  presets,
  appliedId,
  error = null,
  onApply,
  onOpen,
  onRename,
  onDelete
}: {
  /** Unique among the lists on screen: the preset editor mounts a second copy. */
  listId: string;
  presets: ConversationPreset[];
  appliedId: string;
  /** The last failed apply. Applying is the only action here that can fail. */
  error?: string | null;
  onApply: (id: string) => void;
  onOpen: (id: string) => void;
  onRename: (id: string, name: string) => void;
  onDelete: (id: string) => void;
}) {
  const { t } = useI18n();
  const [renaming, setRenaming] = useState<{ id: string; original: string; name: string } | null>(null);
  const { ordered, reorder } = useCatalogOrder("presets", presets, presetId);
  const sort = useCatalogSort({ listId, ids: ordered.map(presetId), onReorder: reorder });

  /* Renaming commits by leaving the field, the way the sidebar renames a
   * conversation and the way every other edit in this application is kept: there
   * is no Save button to forget to press. A name emptied or left alone is not a
   * rename, so it closes without writing rather than storing a blank. */
  const finishRename = () => {
    if (!renaming) return;
    const name = renaming.name.trim();
    if (name && name !== renaming.original) onRename(renaming.id, name);
    setRenaming(null);
  };

  return (
    <>
      {error && <p className="field__hint field__hint--error">{error}</p>}
      <CatalogList sort={sort}>
        {ordered.map((preset) => {
          const isApplied = preset.id === appliedId;
          const isRenaming = renaming?.id === preset.id;
          /* The built-in ships with the build: the host rewrites it on every
             start and keeps it through every save, so it has no rename or
             delete to offer. Its window saves a copy instead. */
          const editable = !isBuiltinConversationPreset(preset.id);
          return (
            <CatalogRow
              key={preset.id}
              id={preset.id}
              sort={sort}
              name={preset.name || t("未命名预设", "Untitled preset")}
              on={isApplied}
              detail={rowDetail(
                preset.description,
                !editable && t(
                  "内置预设，随 Mewrk 版本更新，不能改名、修改或删除。",
                  "Built in. It updates with Mewrk and cannot be renamed, edited or deleted."
                )
              )}
              icon={<SlidersHorizontal size={13} aria-hidden="true" />}
              nameEditor={isRenaming ? (
                <input
                  className="input"
                  autoFocus
                  aria-label={t("重命名预设 {name}", "Rename preset {name}", {
                    name: renaming.original || t("未命名预设", "Untitled preset")
                  })}
                  value={renaming.name}
                  onChange={(event) => setRenaming((current) => (
                    current && { ...current, name: event.target.value }
                  ))}
                  onFocus={(event) => event.currentTarget.select()}
                  onBlur={finishRename}
                  onKeyDown={(event) => {
                    if (isImeKeyEvent(event.nativeEvent)) return;
                    if (event.key === "Enter") {
                      event.preventDefault();
                      event.currentTarget.blur();
                    } else if (event.key === "Escape") {
                      event.preventDefault();
                      setRenaming(null);
                    }
                  }}
                />
              ) : undefined}
              lead={(
                <IconButton
                  label={t("套用", "Apply")}
                  onClick={() => onApply(preset.id)}
                ><CornerDownLeft size={13} /></IconButton>
              )}
              actions={<>
                <IconButton
                  label={t("重命名", "Rename")}
                  /* Held down while its own field is open, the way the sidebar
                     holds a conversation's rename button down: the button that
                     opened the field has nothing left to do until it closes. */
                  disabled={!editable || isRenaming}
                  onClick={() => setRenaming({
                    id: preset.id,
                    original: preset.name,
                    name: preset.name
                  })}
                ><Pencil size={13} /></IconButton>
                <IconButton
                  label={t("打开预设 {name}", "Open preset {name}", { name: preset.name })}
                  onClick={() => onOpen(preset.id)}
                ><Settings2 size={13} /></IconButton>
                <ConfirmDeleteButton
                  label={t("删除预设 {name}", "Delete preset {name}", { name: preset.name })}
                  confirmLabel={t("确认删除预设 {name}", "Confirm deleting preset {name}", { name: preset.name })}
                  disabled={!editable}
                  onDelete={() => onDelete(preset.id)}
                />
              </>}
            />
          );
        })}
      </CatalogList>
      <p className="capability-page__hint">{t(
        "行首的箭头把预设的内容复制到本对话，之后改哪一边都不影响另一边；右边的设置按钮在窗口里打开那份预设。拖动整行可以排序。",
        "The arrow at the head of a row copies that preset's contents into this conversation; editing either one afterwards does not affect the other. The settings button on the right opens that preset in a window. Drag a row to reorder."
      )}</p>
    </>
  );
}
