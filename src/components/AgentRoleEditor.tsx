import {
  Blocks,
  Bot,
  Box,
  ChevronDown,
  CopyPlus,
  FileText,
  FolderCog,
  SlidersHorizontal,
  Wrench
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { useI18n } from "../i18n";
import {
  MAX_AGENT_TYPE_CHARS,
  agentModelSelectionIsAvailable,
  canonicalAgentToolNames,
  cloneAgentRole,
  defaultAgentRoleWebSearch,
  sameAgentRole,
  validateAgentTypeName
} from "../lib/agentRoles";
import { supportsVision } from "../lib/modelCapabilities";
import { REASONING_EFFORTS } from "../lib/reasoningEffort";
import type { SaveAgentRoleTarget } from "../lib/runtime";
import {
  isHostDerivedToolName,
  isPreviewLifecycleToolName,
  withPreviewLifecycleTools
} from "../lib/taskTools";
import { familySelectsNativeToolType, familySupportsNativeFetch } from "../lib/webSearch";
import {
  type CapabilityWorkspace,
  parseCapabilityWorkspaceKey,
  workspaceLocationTitle
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
  ConversationTemplateSummary,
  McpProbeReport,
  ReasoningEffort,
  ResourceDescriptor,
  SshMachineConfig,
  ToolDescriptor,
  WebSearchAssets
} from "../types";
import { Dialog, Field } from "./Common";
import { ConversationTemplateEditor } from "./ConversationTemplateEditor";
import { AdvancedToolsPage } from "./AdvancedToolsPage";
import { CapabilitySelectionPage } from "./ConversationSettingsPages";
import { DocsLink } from "./DocsLink";
import { PopoverMenu } from "./PopoverMenu";
import { SettingsLayout, SettingsNavigation } from "./SettingsLayout";
import { SettingsPageHeading } from "./SettingsPageHeading";
import { ToolSelectionGroups } from "./ToolSelectionGroups";
import { HostedWindow } from "./WindowLayer";
import "./AgentRoleEditor.css";

export interface AgentRoleEditorProps {
  /** The role being edited, as the catalog listed it when opened; `null` creates one. */
  resource: AgentRoleResource | null;
  /**
   * The live catalog. Its roles answer the duplicate-name and concurrency
   * checks; its skills, servers and hooks are what the role's own pages pick out
   * of. Never the conversation's selections: a role is not part of any one
   * conversation.
   */
  catalog: CapabilityCatalog;
  providers: readonly ApiProvider[];
  /** The provider the calling conversation runs on — what a role bound to
   * "inherit" will run on, so the web page knows which native tools it has. */
  activeProviderId: string | null;
  /** The whole trusted tool catalogue. A role selects out of it without being
   * bounded by what any conversation enabled. */
  tools: readonly ToolDescriptor[];
  /** Whether the calling conversation's own model reads images — what a role
   * bound to "inherit" will run on, and so the answer its template page needs. */
  conversationImageInputSupported?: boolean;
  /** The search-provider catalogue, so a role can pick its own backends. */
  webSearchAssets: WebSearchAssets;
  /** Every stored template, for the message counts the template page reports. */
  templates: ConversationTemplateSummary[];
  /** Offered on the template page as bodies to copy over the role's own. */
  presets: readonly ConversationPreset[];
  onReadTemplate: (templateId: string) => Promise<ContextItem[]>;
  /** Writes a body and resolves with the id it landed under, minting when empty. */
  onWriteTemplate: (templateId: string, contexts: ContextItem[]) => Promise<string>;
  /**
   * The workspaces of the conversation the window was opened from: the levels a
   * new role may be written at besides the global one. Undefined is a preset's
   * window, which belongs to no workspace and offers the global level alone.
   */
  workspaces?: readonly CapabilityWorkspace[];
  sshMachines?: readonly SshMachineConfig[];
  onRescan?: () => void | Promise<void>;
  onReveal?: (kind: CapabilityResourceKind, workspaceKey: string | null) => void;
  onProbeMcpServer?: (resource: ResourceDescriptor) => Promise<McpProbeReport>;
  /** Writes the role file and resolves with its id once the catalog has it. */
  onSave: (target: SaveAgentRoleTarget, role: AgentRole) => Promise<string>;
  /**
   * A save that wrote a NEW file — a created role, or a built-in saved as a
   * global copy, which then names the built-in it replaces. The opener selects it.
   */
  onCreated?: (id: string, replaces: string | null) => void;
  onClose: () => void;
}

interface EditorState {
  /**
   * The body the window opened on, `null` for a new role. The concurrency check:
   * a save whose file the catalog no longer lists with exactly this body is
   * refused instead of written over.
   */
  original: AgentRole | null;
  /** The level a new role is written at (`capabilityWorkspaceKey`); `null` is the global `~/.mewrk`. */
  workspaceKey: string | null;
  draft: AgentRole;
}

/**
 * The role windows left open in mid-edit, by role.
 *
 * Deliberately module state, and deliberately NOT persisted. A role window is a
 * long form — a name, prose, a model, a tool allowlist, a whole web
 * configuration — and closing it to go look at something else used to throw all
 * of that away. So a draft outlives its window: reopening the same role, or the
 * new-role window, comes back to exactly what was typed.
 *
 * Keyed by the role's catalog id — the file it belongs to, wherever it is opened
 * from — and `new:<workspaceKey|global>` for one not written yet, named by the
 * level it is being written at, because a role belongs to no conversation:
 * opening it from another one is opening the same file. There is only ever one
 * role not written yet: changing its level moves its entry to the new level's
 * key rather than leaving a second draft behind.
 *
 * It does not outlive the process, because it is not a saved thing. Nothing is
 * written to the file until Save, and a draft that survived a restart would be
 * an unsaved edit the user could no longer tell apart from a saved one.
 *
 * Saving clears the entry: the draft has become the file, and keeping a copy
 * would reopen the window on a "draft" identical to what is already stored.
 */
const editorDrafts = new Map<string, EditorState>();

/** A ceiling, so a long session cannot accumulate drafts without bound. */
const MAX_EDITOR_DRAFTS = 64;

/** Where a role not yet written keeps its draft: `new:` and the level it will be
 * written at, `global` or the workspace's key. */
const NEW_ROLE_DRAFT_PREFIX = "new:";

function draftKeyOf(resource: AgentRoleResource | null, level: string | null): string {
  return resource ? resource.id : `${NEW_ROLE_DRAFT_PREFIX}${level ?? "global"}`;
}

/** Drops the draft of the role not written yet, at whichever level it sits. */
function forgetNewRoleDraft(): void {
  for (const key of [...editorDrafts.keys()]) {
    if (key.startsWith(NEW_ROLE_DRAFT_PREFIX)) editorDrafts.delete(key);
  }
}

function rememberDraft(key: string, state: EditorState): void {
  // A new role's level is part of its key, so a change of level must not leave
  // the old key behind as a second, stale copy.
  if (key.startsWith(NEW_ROLE_DRAFT_PREFIX)) forgetNewRoleDraft();
  // Re-inserting moves the key to the end, so eviction drops the least recently
  // touched draft rather than an arbitrary one.
  editorDrafts.delete(key);
  editorDrafts.set(key, state);
  while (editorDrafts.size > MAX_EDITOR_DRAFTS) {
    const oldest = editorDrafts.keys().next();
    if (oldest.done) break;
    editorDrafts.delete(oldest.value);
  }
}

/** Drops a role's draft — its file is gone, and a draft of it would reopen on a
 * role that no longer exists. */
export function forgetAgentRoleDraft(roleId: string): void {
  editorDrafts.delete(roleId);
}

/**
 * The pages the role editor lists down its left edge.
 *
 * Laid out the way a preset's window is — a rail of pages on the left, the page
 * on the right, the save under the rail — because it is the same kind of thing:
 * one reusable body, opened to be edited. `role` is what the role is called,
 * what it runs on and where its file lives; `tools` and `advanced` are the
 * preset's own two tool pages with the sections a role cannot answer left out;
 * `skills`, `mcp` and `hooks` are the catalog pages without the lock and the
 * delivery switches, which stay the calling conversation's; `template` is the
 * opening history.
 */
type RoleEditorPage = "role" | "tools" | "advanced" | RoleCapabilityKind | "template";

type RoleCapabilityKind = "skills" | "mcp" | "hooks";

const ROLE_EDITOR_PAGES: ReadonlyArray<{ id: RoleEditorPage; icon: typeof Bot }> = [
  { id: "role", icon: Bot },
  { id: "tools", icon: Wrench },
  { id: "advanced", icon: SlidersHorizontal },
  { id: "skills", icon: Box },
  { id: "mcp", icon: Blocks },
  { id: "hooks", icon: FolderCog },
  { id: "template", icon: FileText }
];

/** The draft field each capability page edits. */
const ROLE_CAPABILITY_FIELDS = {
  skills: "skillIds",
  mcp: "mcpIds",
  hooks: "hookIds"
} as const;

function isRoleCapabilityKind(page: RoleEditorPage): page is RoleCapabilityKind {
  return page === "skills" || page === "mcp" || page === "hooks";
}

interface ExplicitModelOption {
  key: string;
  providerId: string;
  providerName: string;
  modelId: string;
}

/** The level a catalog entry was read from: `null` global, a workspace key, or the built-ins. */
function levelOf(resource: ResourceDescriptor): string | null {
  return resource.source === "builtin" ? null : resource.workspaceKey ?? null;
}

/**
 * One subagent role's window: its file body, edited as a whole and written with
 * Save.
 *
 * A role answers which model it runs on, which tools, skills, MCP servers and
 * hooks it may use, how its searches and fetches go, and what it tells the model
 * it is for. Every one of those answers except the model and the effort is the
 * role's own, whichever conversation it is opened or called from: a role is a
 * file at the global or a workspace level, like a skill, and this window never
 * reads the calling conversation's tools, selections or web settings — opening
 * the same role from two conversations opens the same file. It carries no system
 * prompt of its own: a named child renders the conversation's prompt through the
 * same subagent addendum an ordinary child gets, so a role is a routing choice
 * rather than a second persona to keep in sync. The description is part of that
 * routing choice and nothing more — prose the model reads when picking a name,
 * never instructions the child receives.
 */
export function AgentRoleEditor({
  resource,
  catalog,
  providers,
  activeProviderId,
  tools,
  conversationImageInputSupported = false,
  webSearchAssets,
  templates,
  presets,
  onReadTemplate,
  onWriteTemplate,
  workspaces,
  sshMachines = [],
  onRescan,
  onReveal,
  onProbeMcpServer,
  onSave,
  onCreated,
  onClose
}: AgentRoleEditorProps) {
  const { t } = useI18n();
  const builtin = resource?.source === "builtin";

  /* Two whole categories are withheld here, for two unrelated reasons.
   *
   * `orchestration` — a child may hold NONE of it. The host strips
   * `agent_spawn` / `task_wait` / `task_list` / `workflow` / `ask_user` from
   * every child template (`SUBAGENT_DISABLED_TOOL_NAMES` in
   * `src-tauri/src/api.rs`), and that list is re-applied to the catalogue a
   * role's allowlist selects out of — so ticking one of them here could
   * never grant it. It could only make the role worse: a role whose
   * allowlist named nothing else resolves to the
   * empty set and its first spawn fails outright ("no usable tools resolved" —
   * the one degenerate case the host lets through is a conversation that
   * enabled no tools at all). Drawing a switch for a capability this screen
   * cannot grant is a promise it cannot keep.
   *
   * Excluded by CATEGORY rather than by name deliberately. The name list lives
   * in Rust and is the authority; a second copy here would drift, and it would
   * drift in the dangerous direction — a newly denied orchestration tool would
   * silently start appearing as a grantable switch. By category the default is
   * closed. The host's own "every tool a role can hold"
   * (`agent_roles::all_role_tool_names`) is drawn by the same rule.
   *
   * Two members of that category are NOT actually withdrawn by this rule.
   * `skill` is available to a child on purpose, and `task_wait` / `task_list`
   * are re-derived for a child that still holds a task-producing tool. All
   * three are host-derived (`isHostDerivedToolName`), so `ToolSelectionGroups`
   * has never offered them in any picker and nothing on this screen decides
   * their fate — the host exempts them from the allowlist in both directions.
   *
   * `memory` — excluded for a different reason entirely: memory availability
   * derives from the conversation's memory switches plus the role's memory
   * binding, never from a tool list.
   *
   * NOTE what is NOT a rule here: any conversation's own enabled set. This
   * picker draws from the whole catalogue, and the host honours that — a tool
   * ticked here is granted even if the calling conversation switched it off. */
  const selectableTools = useMemo(
    () => tools.filter((tool) =>
      tool.category !== "orchestration" && tool.category !== "memory"),
    [tools]
  );
  const selectableToolNameSet = useMemo(
    () => new Set(selectableTools.map((tool) => tool.name)),
    [selectableTools]
  );
  /* The rows the picker draws. An allowlist may legitimately name tools the
   * picker does not offer — an orchestration name from a hand-written file, or a
   * name from a catalogue this build no longer carries. Those entries are left
   * alone rather than stripped on load: the host intersects them away, so they
   * are inert. They must not be COUNTED, though, or the rail would report tools
   * the picker is not drawing. The preview lifecycle tools have no switch of
   * their own either: they follow the other preview tools. */
  const isPickerRow = (name: string) => selectableToolNameSet.has(name)
    && !isHostDerivedToolName(name)
    && !isPreviewLifecycleToolName(name);
  const visibleSelectedCount = (names: readonly string[]) => names.filter(isPickerRow).length;
  /* What a new role may call: every row the picker draws, ticked, with the
     preview lifecycle tools brought along the way the picker brings them. The
     same set the host materialises for a file that names no tools. */
  const everySelectableToolName = () => canonicalAgentToolNames(withPreviewLifecycleTools(
    selectableTools.map((tool) => tool.name).filter(isPickerRow),
    selectableToolNameSet
  ));

  const explicitModelOptions = useMemo<ExplicitModelOption[]>(() => {
    const result: ExplicitModelOption[] = [];
    for (const provider of providers) {
      if (!provider.enabled) continue;
      for (const model of provider.models) {
        result.push({
          key: `configured-model-${result.length}`,
          providerId: provider.id,
          providerName: provider.name,
          modelId: model.id
        });
      }
    }
    return result;
  }, [providers]);

  /* The levels a new role can be written at: the global one, then each workspace
     of the conversation the window was opened from. A preset's window belongs to
     no workspace, so it offers the global level alone. */
  const creatableLevels = useMemo(
    () => [null, ...(workspaces ?? []).map((workspace) => workspace.key)],
    [workspaces]
  );

  /* What a role written at `level` may name: the global level, the built-ins and
     that workspace's own. Everything else is dropped — a global role naming one
     workspace's skill, server or hook would dangle, and fail its spawn, in every
     conversation without that workspace. */
  const withReachablePicks = (role: AgentRole, level: string | null): AgentRole => {
    const reachable = (resources: readonly ResourceDescriptor[], ids: readonly string[]) => {
      const inScope = new Set(resources
        .filter((entry) => entry.source === "builtin" || !entry.workspaceKey || entry.workspaceKey === level)
        .map((entry) => entry.id));
      return ids.filter((id) => inScope.has(id));
    };
    return {
      ...role,
      skillIds: reachable(catalog.skills, role.skillIds),
      mcpIds: reachable(catalog.mcps, role.mcpIds),
      hookIds: reachable(catalog.hooks, role.hookIds)
    };
  };

  /* A brand-new role is a reusable asset from its first keystroke, so nothing
   * about it is borrowed from the conversation the window happened to be opened
   * from: every tool the picker offers, no skills, servers or hooks, and the
   * built-in preset's web configuration. Only the model and the effort start on
   * "follow the caller" — the two answers a role may leave to whoever calls it. */
  const blankState = (): EditorState => ({
    original: null,
    workspaceKey: null,
    draft: {
      name: "",
      description: "",
      modelSelection: { kind: "inherit" },
      effort: null,
      tools: everySelectableToolName(),
      disallowedTools: [],
      skillIds: [],
      mcpIds: [],
      hookIds: [],
      webSearch: defaultAgentRoleWebSearch(),
      templateId: null
    }
  });

  const [editor, setEditor] = useState<EditorState>(() => {
    if (!resource) {
      const remembered = [...editorDrafts.entries()]
        .find(([key]) => key.startsWith(NEW_ROLE_DRAFT_PREFIX))?.[1];
      if (!remembered) return blankState();
      if (creatableLevels.includes(remembered.workspaceKey)) return remembered;
      /* A draft begun from another conversation may name a workspace this one
         does not have. It falls back to the global level rather than writing
         somewhere the window cannot show — and drops the skills, servers and
         hooks of the workspace it left, which no global role can name. */
      return {
        ...remembered,
        workspaceKey: null,
        draft: withReachablePicks(remembered.draft, null)
      };
    }
    const remembered = editorDrafts.get(resource.id);
    const opened = resource.role ? cloneAgentRole(resource.role) : blankState().draft;
    /* A draft left open on this role, unless the file has moved underneath it.
     * A body that changed elsewhere means the saved role is no longer the one
     * this draft was started from, and saving it would be refused anyway — so
     * the file wins and the stale draft is dropped rather than shown as if it
     * were still live. */
    if (remembered?.original && resource.role && sameAgentRole(remembered.original, resource.role)) {
      return remembered;
    }
    if (remembered) editorDrafts.delete(resource.id);
    return { original: resource.role ? cloneAgentRole(resource.role) : null, workspaceKey: levelOf(resource), draft: opened };
  });
  const draft = editor.draft;
  const draftKey = draftKeyOf(resource, editor.workspaceKey);
  const [editorError, setEditorError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  /* Every role opens on its own settings page: the name is the one thing a
   * new role cannot be saved without. */
  const [page, setPage] = useState<RoleEditorPage>("role");
  /* The role's template body, read the first time its page is opened. It lives
   * here rather than in the template editor because the rail needs it too: a
   * preset's body is only copied over this one after asking, and only when
   * there is something here to lose. The body is written straight to the host
   * as it is edited; the role's draft holds only the id, and only once there is
   * one. */
  const [templateBody, setTemplateBody] = useState<ContextItem[] | null>(null);
  /* The id whose body `templateBody` already holds. A first save mints the id
   * the draft then takes, and re-reading a body this editor just wrote would be
   * a second chance to lose what was typed since, not a refresh. */
  const heldTemplateId = useRef<string | null>(null);
  /* Bumped when a body the template editor did not write lands under it — a
   * preset's, copied over — so the editor starts again on that body instead of
   * keeping the draft it was seeded with. */
  const [templateGeneration, setTemplateGeneration] = useState(0);
  /* A preset whose template is waiting on the user's word before it replaces
   * the role's non-empty one. */
  const [pendingOverwrite, setPendingOverwrite] = useState<ConversationPreset | null>(null);
  const [overwriting, setOverwriting] = useState(false);
  const [overwriteError, setOverwriteError] = useState<string | null>(null);
  /* Whether the name has been asked to be valid yet. A create window opens on an
   * empty name, and announcing "a role name is required" before the user has had
   * a chance to type one reads as a complaint about their not having typed it.
   * Save is what asks. A remembered draft has already been typed into, and an
   * existing role arrives with a valid name, so both answer for it at once. */
  const [nameChecked, setNameChecked] = useState(() => Boolean(resource) || Boolean(editor.draft.name.trim()));

  /* Every edit lands in the cache as well as in state, so closing the window
   * needs no save of its own — there is nothing left to lose by then. A function
   * of the current draft where one change is several writes in a row (the
   * native tool version selects the backend and then its version). */
  const replaceEditor = (change: (current: EditorState) => EditorState) => {
    setEditor((current) => {
      const next = change(current);
      rememberDraft(draftKeyOf(resource, next.workspaceKey), next);
      return next;
    });
    setEditorError(null);
  };
  const replaceDraft = (change: Partial<AgentRole> | ((current: AgentRole) => Partial<AgentRole>)) => {
    replaceEditor((current) => ({
      ...current,
      draft: { ...current.draft, ...(typeof change === "function" ? change(current.draft) : change) }
    }));
    // Typing a name is the user taking up the question, so from the first
    // keystroke the field answers live. Clearing it again does not put the
    // complaint back — that is the state the window opened in.
    if (typeof change !== "function" && typeof change.name === "string" && change.name.trim()) {
      setNameChecked(true);
    }
  };

  /* The level the role's file is (or will be) at. A built-in is saved as a
     global copy, so it answers for the global level. */
  const roleLevel = resource && !builtin ? levelOf(resource) : editor.workspaceKey;
  /* The one workspace the role's own capability pages may reach besides the
     global level: a role is called from conversations that may not have any
     other, so it may only name entries every caller of its level can reach. */
  const roleWorkspaces = useMemo((): CapabilityWorkspace[] => {
    if (roleLevel === null) return [];
    const known = workspaces?.find((workspace) => workspace.key === roleLevel);
    return [known ?? { number: 1, key: roleLevel, ...parseCapabilityWorkspaceKey(roleLevel) }];
  }, [roleLevel, workspaces]);
  const levelLabel = (key: string | null) => {
    if (key === null) return t("全局 · ~/.mewrk/agents", "Global · ~/.mewrk/agents");
    const workspace = workspaces?.find((candidate) => candidate.key === key)
      ?? { key, ...parseCapabilityWorkspaceKey(key) };
    return workspaceLocationTitle(workspace.path, workspace.machine, sshMachines);
  };
  /* Moving a new role to another level drops whatever it had picked that the
     new level cannot reach. */
  const changeLevel = (workspaceKey: string | null) => {
    replaceEditor((current) => ({
      ...current,
      workspaceKey,
      draft: withReachablePicks(current.draft, workspaceKey)
    }));
  };

  /* Closing keeps the draft. It is the whole point of the cache: leaving to
   * check something, or to read a tool's page, is not a decision to throw the
   * half-written role away. Nothing has been written to the file either way —
   * Save is still the only thing that writes. */
  const closeEditor = () => {
    setPendingOverwrite(null);
    onClose();
  };

  /* The name as it will be SAVED. A name is free text, so edge whitespace is
   * the one thing still normalised — it is invisible, and a name differing from
   * another only by a trailing space is one nobody can tell apart in the list or
   * point the model at. Trimmed here rather than on each keystroke: a name may
   * contain spaces, and trimming as the user types would eat the one they just
   * pressed before the next word. */
  const draftName = draft.name.trim();
  const nameError = validateAgentTypeName(draftName);
  /* Two roles may share a name across levels — a workspace's own `reviewer`
     shadows the global one, and a global copy of a built-in shadows the
     built-in — but not within one, where the model could not tell them apart. */
  const duplicateName = catalog.agents.some((entry) => (
    entry.source !== "builtin"
    && (builtin || entry.id !== resource?.id)
    && (entry.workspaceKey ?? null) === roleLevel
    && (entry.role?.name ?? entry.name).trim() === draftName
  ));
  /** Whether the name's verdict is the user's business yet. See `nameChecked`. */
  const showNameError = nameChecked && Boolean(nameError || duplicateName);

  const selectedModelSelection = draft.modelSelection;
  const selectedExplicitOption = selectedModelSelection.kind === "explicit"
    ? explicitModelOptions.find((option) => (
        option.providerId === selectedModelSelection.providerId
        && option.modelId === selectedModelSelection.modelId
      )) ?? null
    : null;
  // Same rule the list rows and the host use, rather than a third spelling of
  // it: `explicitModelOptions` is already filtered to enabled providers and
  // models, so a saved selection that finds no option is exactly one the host
  // would refuse to resolve.
  const modelSelectionUnavailable = !agentModelSelectionIsAvailable(selectedModelSelection, providers);
  // No option carries the dead binding, so the select shows nothing selected
  // rather than a placeholder row the user could mistake for a choice. The
  // error styling and hint carry the state instead.
  const modelSelectionValue = selectedModelSelection.kind === "explicit"
    ? selectedExplicitOption?.key ?? ""
    : selectedModelSelection.kind === "unavailable"
      ? ""
      : "inherit";
  /* The provider the role's own runs go through: its bound one, or — riding the
     caller's model — the provider the calling conversation is on. A binding that
     no longer resolves has no provider to ask. */
  const runFamily = selectedModelSelection.kind === "explicit"
    ? providers.find((provider) => provider.id === selectedModelSelection.providerId)?.family
    : selectedModelSelection.kind === "inherit"
      ? providers.find((provider) => provider.id === activeProviderId)?.family
      : undefined;
  /* Whether a message written into this role's template can carry an image.
   * A bound pair is asked directly; "inherit" is whatever the conversation runs
   * on; a binding that no longer resolves answers no, because there is no model
   * left to say yes for it. */
  const selectedModelReadsImages = selectedModelSelection.kind === "explicit"
    ? Boolean((() => {
      const provider = providers.find((candidate) => (
        candidate.enabled && candidate.id === selectedModelSelection.providerId
      ));
      const model = provider?.models.find((candidate) => candidate.id === selectedModelSelection.modelId);
      return model && supportsVision(model);
    })())
    : selectedModelSelection.kind === "inherit" && conversationImageInputSupported;

  /* Names a bound pair that does not currently resolve, WITHOUT printing the
   * `providerId` — it is a random per-installation UUID and reads as a hash.
   * The provider's display name is the actionable half, and it is available
   * whenever the row still exists, which covers the ordinary cases: signed out,
   * disabled, or catalog not fetched yet. Only a provider deleted outright has
   * no name left, and then the model ID stands alone rather than being padded
   * with an identifier the user cannot use. */
  const unavailableBindingLabel = (
    selection: Extract<AgentModelSelection, { kind: "explicit" }>
  ) => {
    const provider = providers.find((candidate) => candidate.id === selection.providerId);
    return provider ? `${provider.name} · ${selection.modelId}` : selection.modelId;
  };

  /* The template page. The body is read the first time the page is opened and
   * only then — a template is big enough that reading it on the chance the page
   * is opened would slow down opening every role — and kept across leaving the
   * page and coming back, since every edit on it is already written through. */
  const editorTemplateId = draft.templateId ?? "";
  const templatePageOpen = page === "template";
  useEffect(() => {
    if (!templatePageOpen || heldTemplateId.current === editorTemplateId) return;
    if (!editorTemplateId) {
      heldTemplateId.current = "";
      setTemplateBody([]);
      return;
    }
    let abandoned = false;
    setTemplateBody(null);
    void (async () => {
      let contexts: ContextItem[] = [];
      try {
        contexts = await onReadTemplate(editorTemplateId);
      } catch {
        // An unreadable body is an empty one to work from, as it is on a
        // preset's page: the page still has to open, and the next save
        // overwrites whatever is there regardless.
      }
      if (abandoned) return;
      heldTemplateId.current = editorTemplateId;
      setTemplateBody(contexts);
    })();
    return () => { abandoned = true; };
  }, [editorTemplateId, onReadTemplate, templatePageOpen]);

  /* How many messages the role's template holds right now: the body in hand
   * when there is one, otherwise what the host's summary last said. */
  const templateMessageCount = templateBody?.length
    ?? templates.find((summary) => summary.id === editorTemplateId)?.messageCount
    ?? 0;
  const presetTemplateCount = (preset: ConversationPreset) => (preset.templateId
    ? templates.find((summary) => summary.id === preset.templateId)?.messageCount ?? 0
    : 0);

  /* The tools the role can actually call, which is what its template may place.
   * Drawn out of the same catalogue the tools page offers, so a template never
   * holds a card for a tool the role could not be given. */
  const roleToolNames = draft.tools.filter((name) => selectableToolNameSet.has(name));

  /* Writes a body under the role's template and makes sure the draft cites it:
   * the host mints the id on a first write, and the draft is the only place the
   * role will ever learn it from. */
  const writeRoleTemplate = async (templateId: string, contexts: ContextItem[]) => {
    const savedId = await onWriteTemplate(templateId, contexts);
    heldTemplateId.current = savedId;
    setTemplateBody(contexts);
    if (savedId !== templateId) replaceDraft({ templateId: savedId });
  };

  /* Copies a preset's opening history over the role's own. The body is written
   * at once, as every other edit on this page is; the editor then starts again
   * on it, because the draft it was seeded with is no longer what is stored. */
  const overwriteTemplate = async (preset: ConversationPreset) => {
    setPendingOverwrite(null);
    setOverwriteError(null);
    setOverwriting(true);
    try {
      const contexts = await onReadTemplate(preset.templateId);
      // The host refuses an empty body, and copying nothing over something is
      // not what the arrow promised either.
      if (!contexts.length) {
        throw new Error(t("这份预设的对话模板是空的。", "This preset's conversation template is empty."));
      }
      await writeRoleTemplate(editorTemplateId, contexts);
      setTemplateGeneration((generation) => generation + 1);
    } catch (reason) {
      setOverwriteError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setOverwriting(false);
    }
  };

  /* Replacing an empty template loses nothing, so it just happens; replacing
   * one that already says something asks first. */
  const requestOverwrite = (preset: ConversationPreset) => {
    if (templateMessageCount > 0) setPendingOverwrite(preset);
    else void overwriteTemplate(preset);
  };

  const pageTitles: Record<RoleEditorPage, string> = {
    role: t("角色设置", "Role settings"),
    tools: t("工具", "Tools"),
    advanced: t("高级工具", "Advanced tools"),
    skills: t("技能", "Skills"),
    mcp: "MCP",
    hooks: t("钩子", "Hooks"),
    template: t("对话模板", "Conversation template")
  };
  /* Chinese sentences run on; English ones are spaced. */
  const sentenceGap = t("", " ");
  /* Said on each capability page: which levels this role may pick from. */
  const scopeNote = roleLevel === null
    ? t(
        "这个角色在全局，只能选全局（~/.mewrk）里的条目。",
        "This role is global, so it can pick only global (~/.mewrk) entries."
      )
    : t(
        "这个角色属于工作区 {path}，可以选全局与这个工作区里的条目。",
        "This role belongs to the workspace {path}, so it can pick global entries and that workspace's own.",
        { path: roleWorkspaces[0]?.path ?? roleLevel }
      );
  const pageBlurbs: Record<RoleEditorPage, string> = {
    role: t(
      "这个角色叫什么、主代理从描述里读到它是干什么的、它跑在哪个模型上，以及它的文件放在哪里。关掉窗口会保留草稿，点保存才会写入文件。",
      "What this role is called, what the main agent reads about what it is for, which model it runs on, and where its file lives. Closing the window keeps the draft; nothing is written to the file until you save."
    ),
    tools: t("这个角色能调用的工具。", "The tools this role may call."),
    /* No lock on any of these: a role's settings are no part of the calling
       conversation's prompt cache, so a change simply reaches the next
       subagent this conversation calls. */
    skills: [
      t(
        "这个角色的子代理能用的技能。改动对之后新调用的子代理生效；正文怎样送到模型面前由调用它的对话的「技能按需加载」决定。",
        "The skills this role's subagents can use. A change applies to the next subagent called; how their bodies reach the model is decided by the calling conversation's Load skills on demand."
      ),
      scopeNote
    ].join(sentenceGap),
    mcp: [
      t(
        "这个角色的子代理接入的 MCP Server，选中的服务器的工具都交给它。改动对之后新调用的子代理生效；工具发现由调用它的对话决定。",
        "The MCP servers this role's subagents attach, with all their tools. A change applies to the next subagent called; tool discovery is decided by the calling conversation."
      ),
      scopeNote
    ].join(sentenceGap),
    hooks: [
      t(
        "这个角色的子代理额外运行的钩子。调用它的对话里那些工具调用前后、权限请求与指令加载的钩子总会照常运行，角色绕不开它们；这里选的在它们之外再加上。会话开始、提交提示词与停止属于主对话的回合。改动对之后新调用的子代理生效，但新选上的钩子要等下一轮开始时确认过才会运行（完全访问不需要确认）。",
        "The hooks this role's subagents run in addition. The calling conversation's hooks before and after a tool call, on a permission request and on instructions loaded always run as well, so a role cannot get around them; the ones picked here are added to those. Session start, prompt submitted and stop belong to the conversation's own turn. A change applies to the next subagent called, but a newly selected hook runs only once the next turn has confirmed it (Full access needs no confirmation)."
      ),
      scopeNote
    ].join(sentenceGap),
    advanced: t(
      "这个角色自己的联网搜索与抓取：后端、原生工具版本、结果整形与域名过滤。联网开关、记忆、工具描述与宿主消息的容器由调用它的对话决定，这里不出现；调用方对话没开联网时，这里怎么选都不会让子代理联网。",
      "This role's own web search and fetch: the backends, the native tool versions, result shaping and the domain filter. Web access, memory, tool descriptions and the host-message container are decided by the conversation that calls it, so they do not appear here; when that conversation is offline, nothing chosen here puts its subagent online."
    ),
    template: t(
      "这个角色的开局历史。模板里的用户消息若恰好含一个 {input}，主代理给的输入会替换到那里；没有或有两个以上时，输入作为最后一条用户消息追加。右上角的「从预设覆盖」可以用一份预设的对话模板换掉这里。",
      "This role's opening history. If the template's user messages contain exactly one {input}, the caller's input replaces it; with none or with two or more, the input is appended as a final user message. Copy from a preset, at the top right, replaces this one with a preset's template."
    )
  };

  const nameErrorText = () => {
    if (duplicateName) {
      return t("同一位置已有同名的角色。", "A role with this name already exists at this location.");
    }
    switch (nameError) {
      case "required":
        return t("角色名称不能为空。", "A role name is required.");
      case "too_long":
        return t(
          "角色名称最多 {count} 个字符。",
          "A role name can contain at most {count} characters.",
          { count: MAX_AGENT_TYPE_CHARS }
        );
      case "characters":
        return t(
          "角色名称不能包含换行或其它控制字符。",
          "A role name cannot contain newlines or other control characters."
        );
      default:
        return null;
    }
  };

  const saveEditor = async () => {
    if (saving) return;
    // Saving is the moment the name has to be right, so it is also the moment
    // the field is allowed to say so.
    setNameChecked(true);
    /* The file this window opened has to be the file still there. Another
     * window, a text editor or the host's own migration may have rewritten it
     * since, and writing this draft over that would silently undo it. A built-in
     * is never written, only copied, so there is nothing of it to overwrite. */
    if (resource && !builtin) {
      const current = catalog.agents.find((entry) => entry.id === resource.id);
      if (!current?.role || !editor.original || !sameAgentRole(current.role, editor.original)) {
        setEditorError(t(
          "此角色的文件已在别处变化。请关闭后重新打开再编辑。",
          "This role's file changed elsewhere. Close and reopen it before editing."
        ));
        return;
      }
    }
    // `modelSelectionUnavailable` is deliberately NOT a save blocker. It is a
    // state the role arrives in on its own — the provider went away, nobody
    // typed anything wrong — so refusing the save would trap every other edit
    // (a rename, a tool change) behind fixing a model the user may not be able
    // to restore right now. The role stays uncallable until the model is set;
    // that is the enforcement, and it does not need a second one here.
    if (nameError || duplicateName) {
      setEditorError(t("请先修正标记的字段。", "Fix the marked fields before saving."));
      // The marked field is on the role page; a refusal read from any other
      // page would point at something the user cannot see.
      setPage("role");
      return;
    }

    /* Lists canonical the way the host keeps them, so a reorder is not a change:
       tools sorted, the three id lists de-duplicated in the order they were
       picked — the prompt lists skills and hooks run in that order. */
    const body: AgentRole = {
      ...cloneAgentRole(draft),
      name: draftName,
      tools: canonicalAgentToolNames(draft.tools),
      disallowedTools: canonicalAgentToolNames(draft.disallowedTools),
      skillIds: [...new Set(draft.skillIds)],
      mcpIds: [...new Set(draft.mcpIds)],
      hookIds: [...new Set(draft.hookIds)]
    };
    const creating = !resource || builtin;
    const target: SaveAgentRoleTarget = creating
      ? { workspaceKey: roleLevel }
      : { id: resource.id, workspaceKey: resource.workspaceKey ?? null };
    setSaving(true);
    setEditorError(null);
    let savedId: string;
    try {
      savedId = await onSave(target, body);
    } catch (reason) {
      setSaving(false);
      setEditorError(reason instanceof Error ? reason.message : String(reason));
      return;
    }
    /* The draft has become the file, so it stops being a draft. */
    if (resource) editorDrafts.delete(draftKey);
    else forgetNewRoleDraft();
    if (creating) onCreated?.(savedId, builtin && resource ? resource.id : null);
    setSaving(false);
    setPendingOverwrite(null);
    onClose();
  };

  const saveLabel = builtin
    ? t("另存为全局角色", "Save as a global role")
    : t("保存角色", "Save role");

  return (
    <>
      <HostedWindow>
        <Dialog
          /* Named after the role, the way a preset's window is named after the
             preset. The name as it was OPENED, not as it is being typed: the
             title is how the user knows which role this window is, and it
             should not change under them mid-rename. */
          title={resource
            ? (resource.role?.name || resource.name) || t("角色设置", "Role settings")
            : t("新建角色", "New role")}
          width="1040px"
          sidebar
          bodyClassName="dialog__body--flush"
          // Deliberately not dismissible: a stray click on the backdrop would
          // discard an in-progress role without asking. The header's own close
          // button is unaffected — that one is a decision, not a slip, and it
          // keeps the draft rather than throwing it away.
          dismissible={false}
          onClose={closeEditor}
        >
          <SettingsLayout
            label={t("角色设置", "Role settings")}
            navigation={(
              <SettingsNavigation
                label={t("角色设置分类", "Role settings categories")}
                groups={[{
                  id: "pages",
                  items: ROLE_EDITOR_PAGES.map((item) => ({
                    id: item.id,
                    icon: item.icon,
                    label: pageTitles[item.id],
                    count: item.id === "template"
                      ? templateMessageCount
                      : item.id === "tools"
                        ? visibleSelectedCount(draft.tools)
                        : isRoleCapabilityKind(item.id)
                          ? draft[ROLE_CAPABILITY_FIELDS[item.id]].length
                          : null
                  }))
                }]}
                view={page}
                onSelect={setPage}
                footer={(
                  <>
                    <button
                      type="button"
                      className="settings-nav__action"
                      // Not disabled on a blank name. A save greyed out with no
                      // explanation is a puzzle; pressing it and being told what
                      // is missing is an answer.
                      disabled={saving}
                      onClick={() => void saveEditor()}
                    >{saving ? t("正在保存…", "Saving…") : saveLabel}</button>
                    {editorError && (
                      <p className="settings-nav__error" role="alert">{editorError}</p>
                    )}
                  </>
                )}
              />
            )}
          >
            <section className={page === "template" ? "settings-page settings-editor-page" : "settings-page"}>
              {/* The rail names the page, so the page says only what it is for,
                  as a preset's window does. */}
              <SettingsPageHeading
                description={pageBlurbs[page]}
                /* On the template page, copying a preset's body over the role's:
                   an act on that page, read against the body it would replace. A
                   preset with no template has nothing to copy, so its row is
                   there but cannot be chosen. The two tool pages end the line
                   with their documentation instead, as the conversation's do. */
                action={page === "tools" || page === "advanced" ? (
                  <DocsLink page="features" />
                ) : page === "template" ? (
                  <div className="settings-page-heading__actions">
                    <PopoverMenu
                      trigger={<>
                        <CopyPlus size={14} aria-hidden="true" />
                        {t("从预设覆盖", "Copy from a preset")}
                        <ChevronDown size={12} aria-hidden="true" />
                      </>}
                      triggerLabel={t("从预设覆盖", "Copy from a preset")}
                      triggerClassName="button button--secondary button--small"
                      disabled={overwriting}
                      menuLabel={t("从预设覆盖", "Copy from a preset")}
                      align="end"
                      emptyLabel={t("还没有对话预设。", "No conversation presets yet.")}
                      sections={[{
                        id: "presets",
                        items: presets.map((preset) => {
                          const name = preset.name || t("未命名预设", "Untitled preset");
                          const count = presetTemplateCount(preset);
                          return {
                            id: preset.id,
                            label: name,
                            description: count === 0
                              ? t("这份预设没有对话模板。", "This preset has no conversation template.")
                              : t("{count} 条消息", "{count} messages", { count }),
                            disabled: count === 0,
                            onSelect: () => requestOverwrite(preset)
                          };
                        })
                      }]}
                    />
                  </div>
                ) : undefined}
              />
              {builtin && page === "role" && <p className="settings-page__note">{t(
                "内置角色随 Mewrk 版本更新，不能修改或删除。在这里做的改动可以另存为 ~/.mewrk/agents/ 里的一份全局角色；本对话选着这个内置角色时，新角色会顶替它。",
                "Built-in roles update with Mewrk and cannot be edited or deleted. What you change here can be saved as a global role in ~/.mewrk/agents/; where this conversation selects the built-in, the new role takes its place."
              )}</p>}
              {page === "template" && overwriteError && (
                <p className="settings-page__note settings-page__note--error" role="alert">{overwriteError}</p>
              )}
              {/* The template page is a timeline, which brings its own scroller
                  and its own edges, so it gets the body flush — as it does on a
                  preset's page. */}
              <div className={page === "template"
                ? "conversation-settings__page-stack conversation-settings__page-stack--flush"
                : "conversation-settings__page-stack"}>
                {page === "role" && (
                  <>
                    <section className="conversation-settings__field agent-role-editor__identity">
                      {/* No standing hint under it. A role name is free text, so
                        * there is no shape to teach — the only thing left to say
                        * about it is that a particular one was refused, and that
                        * is what the line carries when there is one. */}
                      <Field
                        label={t("角色名称", "Role name")}
                        hint={showNameError ? nameErrorText() ?? undefined : undefined}
                        hintIsError
                      >
                        <input
                          className={`input${showNameError ? " input--error" : ""}`}
                          value={draft.name}
                          aria-label={t("角色名称", "Role name")}
                          aria-invalid={showNameError}
                          onChange={(event) => replaceDraft({ name: event.target.value })}
                          autoComplete="off"
                          autoFocus
                        />
                      </Field>

                      {/* Deliberately no character counter and no maximum. This
                        * is the one field on this screen the model actually reads
                        * as prose, and a budget shown next to it would push users
                        * to write a label where a sentence belongs. The host
                        * writes it through verbatim. */}
                      <Field
                        label={t("子代理描述", "Subagent description")}
                        hint={t(
                          "告诉主代理这个角色是干什么的。会拼进本对话「子代理」/「工作流」工具的描述里，每个角色一行；留空则这个角色不出现在那份说明里。",
                          "Tells the main agent what this role is for. It is appended to this conversation's Subagent / Workflow tool description, one line per role; leave it empty and this role contributes no line."
                        )}
                      >
                        <textarea
                          className="input"
                          rows={4}
                          value={draft.description}
                          aria-label={t("子代理描述", "Subagent description")}
                          onChange={(event) => replaceDraft({ description: event.target.value })}
                          placeholder={t(
                            "对抗式审查：负责证伪既有结论、挑错，不产出主线方案。",
                            "Adversarial review: refutes existing conclusions and finds faults; does not produce the main proposal."
                          )}
                        />
                      </Field>

                      {resource && !builtin && editor.original && editor.original.name.trim() !== draftName && (
                        <p className="agent-role-editor__warning">{t(
                          "改名不会换文件，选着它的对话仍然选着它；但已经按旧名称在跑的子代理不会自动跟随新名称。",
                          "Renaming keeps the file, so conversations that select this role still do; a subagent already running under the old name does not follow the new one."
                        )}</p>
                      )}
                    </section>

                    {/* A row rather than a stacked form field: what it is called
                      * and what it means on the left, the one control that sets
                      * it on the right — the shape the features page uses for
                      * the same kind of question. */}
                    <section className="conversation-settings__field">
                      <div className="agent-role-editor__rows field-row">
                        <Field
                          label={t("执行模型", "Execution model")}
                          hint={modelSelectionUnavailable
                            ? selectedModelSelection.kind === "explicit"
                              ? t(
                                  "这个模型现在取不到——提供商还没拉取模型、被停用，或者这一行已经不在了。角色暂时不能被调用，但绑定会一直留着，模型回来就自动恢复。",
                                  "This model cannot be resolved right now — the provider has not fetched its models, is disabled, or the row is gone. The role cannot be called for now, but the binding is kept and recovers by itself once the model is back."
                                )
                              : t(
                                  "这个角色现在没有可用的模型（旧版本记下的失效绑定，或内置角色找不到对应的提供商）；请选择跟随对话或另一个可用模型。",
                                  "This role has no model to run on right now (a broken binding an older build recorded, or a built-in role whose provider is missing). Choose the conversation's model or another available one."
                                )
                            : t(
                                "只列出已启用提供商中的模型；保存精确 provider/model ID。",
                                "Only models from enabled providers are listed; exact provider/model IDs are saved."
                              )}
                        >
                          <select
                            className={`input${modelSelectionUnavailable ? " input--error" : ""}`}
                            value={modelSelectionValue}
                            aria-label={t("执行模型", "Execution model")}
                            aria-invalid={modelSelectionUnavailable}
                            onChange={(event) => {
                              if (event.target.value === "inherit") {
                                replaceDraft({ modelSelection: { kind: "inherit" } });
                                return;
                              }
                              const option = explicitModelOptions.find((candidate) => (
                                candidate.key === event.target.value
                              ));
                              if (!option) return;
                              replaceDraft({
                                modelSelection: {
                                  kind: "explicit",
                                  providerId: option.providerId,
                                  modelId: option.modelId
                                }
                              });
                            }}
                          >
                            {/* `hidden` keeps this out of the dropdown list: it
                              * exists only so the closed select does not fall
                              * through to the first option and display a working
                              * model the role does not have.
                              *
                              * A binding whose pair is still recorded but no longer
                              * offered — its model has not been fetched, or the
                              * provider is disabled — names the model the user
                              * actually chose. It must never print the raw
                              * `providerId`, which is a random per-installation
                              * UUID and reads as a hash; the provider's own name
                              * is the part a person can act on, and when its row
                              * is gone entirely there is no name to give, so the
                              * model ID stands alone. */}
                            {modelSelectionUnavailable && (
                              <option value="" disabled hidden>
                                {selectedModelSelection.kind === "explicit"
                                  ? t(
                                      "{model}（不可用）",
                                      "{model} (unavailable)",
                                      { model: unavailableBindingLabel(selectedModelSelection) }
                                    )
                                  : t("请选择执行模型", "Choose an execution model")}
                              </option>
                            )}
                            <option value="inherit">{t("跟随对话模型", "Follow the conversation's model")}</option>
                            {explicitModelOptions.map((option) => (
                              <option key={option.key} value={option.key}>
                                {option.providerName} · {option.modelId}
                              </option>
                            ))}
                          </select>
                        </Field>

                        <Field
                          label={t("思考程度", "Reasoning effort")}
                          hint={t(
                            "留空表示跟随对话的思考程度。",
                            "Leave unset to follow the conversation's reasoning effort."
                          )}
                        >
                          <select
                            className="input"
                            value={draft.effort ?? "inherit"}
                            aria-label={t("思考程度", "Reasoning effort")}
                            onChange={(event) => replaceDraft({
                              effort: event.target.value === "inherit"
                                ? null
                                : (event.target.value as ReasoningEffort)
                            })}
                          >
                            <option value="inherit">{t("跟随对话", "Follow the conversation")}</option>
                            {REASONING_EFFORTS.map((effort) => (
                              <option key={effort} value={effort}>{effort}</option>
                            ))}
                          </select>
                        </Field>

                        {/* Where the file is. A new role picks its level — the
                          * global one, or one of the workspaces of the
                          * conversation this window was opened from — and is
                          * written there on Save; an existing one stays where it
                          * is, and moving it is moving its file. */}
                        <Field
                          label={t("位置", "Location")}
                          hint={resource
                            ? undefined
                            : t(
                                "全局角色每个对话都能选；工作区角色只有带着这个工作区的对话能选。保存后不能再改位置。",
                                "A global role can be selected by every conversation; a workspace role only by conversations that have that workspace. The location cannot change after saving."
                              )}
                        >
                          {resource && !builtin ? (
                            <span className="agent-role-editor__location" title={resource.location}>
                              {resource.location}
                            </span>
                          ) : builtin ? (
                            <span className="agent-role-editor__location">
                              {t("内置角色，随 Mewrk 版本更新", "Built in; updates with Mewrk")}
                            </span>
                          ) : (
                            <select
                              className="input"
                              aria-label={t("位置", "Location")}
                              value={editor.workspaceKey ?? ""}
                              onChange={(event) => changeLevel(event.target.value || null)}
                            >
                              {creatableLevels.map((key) => (
                                <option key={key ?? ""} value={key ?? ""}>{levelLabel(key)}</option>
                              ))}
                            </select>
                          )}
                        </Field>
                      </div>
                    </section>
                  </>
                )}

                {/* The preset's own two tool pages, not copies of them: a row
                    added or reworded there lands here too. What a role leaves
                    out is exactly what it has no field for — the web access
                    switch (whether to reach the web at all is the caller's
                    decision, and stays the ceiling), the memory tiers (derived
                    from the caller's switches and the role's memory binding,
                    never from a tool list), the tool-description profile and
                    the host-message container. Nothing on either page follows
                    the calling conversation: the list and the web answers are
                    the role's own. */}
                {page === "tools" && (
                  <ToolSelectionGroups
                    tools={selectableTools as ToolDescriptor[]}
                    enabledTools={draft.tools}
                    onChange={(tools) => replaceDraft({ tools })}
                    expansionKey={resource?.id ?? "new-role"}
                  />
                )}

                {page === "advanced" && (
                  <AdvancedToolsPage
                    web={{
                      value: draft.webSearch,
                      onChange: (patch) => replaceDraft((current) => ({
                        webSearch: { ...current.webSearch, ...patch }
                      })),
                      webSearchAssets,
                      // The native backends are the role's own model's, not the
                      // conversation's the hints otherwise speak of.
                      subject: "role",
                      nativeFetchAvailable: familySupportsNativeFetch(runFamily),
                      nativeToolTypeSelectable: familySelectsNativeToolType(runFamily)
                    }}
                  />
                )}

                {/* The catalog pages a conversation's skills, servers and hooks
                    are picked on, so a row reads the same in both places — over
                    the role's own lists, scoped to what every caller of the
                    role's level can reach. Not toned: nothing here is part of
                    the conversation's cache, and there is no delivery switch —
                    how skills and schemas arrive is the conversation's. */}
                {isRoleCapabilityKind(page) && (
                  <CapabilitySelectionPage
                    key={page}
                    kind={page}
                    listId={`role:${resource?.id ?? "new-role"}:${page}`}
                    resources={page === "skills"
                      ? catalog.skills
                      : page === "mcp"
                        ? catalog.mcps
                        : catalog.hooks}
                    selectedIds={draft[ROLE_CAPABILITY_FIELDS[page]]}
                    onChange={(ids) => replaceDraft({ [ROLE_CAPABILITY_FIELDS[page]]: ids })}
                    unreadableLevels={catalog.unreadableLevels ?? []}
                    workspaces={roleWorkspaces}
                    sshMachines={sshMachines}
                    onRescan={onRescan}
                    onReveal={onReveal}
                    onProbeMcpServer={page === "mcp" ? onProbeMcpServer : undefined}
                    searchLabel={page === "skills"
                      ? t("搜索技能", "Search skills")
                      : page === "mcp"
                        ? t("搜索 MCP Server", "Search MCP servers")
                        : t("搜索钩子", "Search hooks")}
                    emptyTitle={page === "skills"
                      ? t("尚未发现技能", "No skills discovered")
                      : page === "mcp"
                        ? t("尚未发现 MCP Server", "No MCP servers discovered")
                        : t("尚未发现钩子", "No hooks discovered")}
                    emptyDescription={page === "skills"
                      ? t(
                          "把技能目录放在 ~/.mewrk/skills/ 或工作区的 .mewrk/skills/ 下，列表很快会自动刷新。",
                          "Put skill folders under ~/.mewrk/skills/ or the workspace's .mewrk/skills/; the list refreshes by itself shortly after."
                        )
                      : page === "mcp"
                        ? t(
                            "把服务器写进 ~/.mewrk/mcp.json 或工作区的 .mewrk/mcp.json，保存后列表很快会自动刷新。",
                            "Declare servers in ~/.mewrk/mcp.json or the workspace's .mewrk/mcp.json; the list refreshes by itself shortly after you save."
                          )
                        : t(
                            "钩子写在 ~/.mewrk/hooks.json 或工作区的 .mewrk/hooks.json 里，保存后列表很快会自动刷新。",
                            "Hooks live in ~/.mewrk/hooks.json or the workspace's .mewrk/hooks.json; the list refreshes by itself shortly after you save."
                          )}
                  />
                )}

                {/* The role's own message queue, edited on the same surface a
                    preset's is. Each edit is written back as it lands, and the
                    first write mints the id the draft then carries. */}
                {page === "template" && (
                  <ConversationTemplateEditor
                    key={templateGeneration}
                    templateId={editorTemplateId}
                    contexts={templateBody}
                    tools={selectableTools as ToolDescriptor[]}
                    enabledTools={roleToolNames}
                    imageInputSupported={selectedModelReadsImages}
                    autosave
                    /* A template that calls a tool the role lacks widens the
                       role's own list, on Save like every other edit here. */
                    onEnableTools={(names) => replaceDraft((current) => ({
                      tools: [...new Set([...current.tools, ...names])]
                    }))}
                    onSave={(contexts) => writeRoleTemplate(editorTemplateId, contexts)}
                  />
                )}
              </div>
            </section>
          </SettingsLayout>
        </Dialog>
      </HostedWindow>

      {pendingOverwrite && (
        <Dialog
          title={t("覆盖对话模板？", "Overwrite the conversation template?")}
          description={t(
            "这个角色的对话模板里已有 {count} 条消息，会被预设「{name}」的对话模板整个替换，替换后无法撤销。",
            "This role's template already holds {count} messages. They will all be replaced by the template of preset “{name}”, and this cannot be undone.",
            {
              count: templateMessageCount,
              name: pendingOverwrite.name || t("未命名预设", "Untitled preset")
            }
          )}
          onClose={() => setPendingOverwrite(null)}
          footer={(
            <>
              <button
                type="button"
                className="button button--secondary"
                onClick={() => setPendingOverwrite(null)}
              >{t("取消", "Cancel")}</button>
              <button
                type="button"
                className="button button--danger"
                onClick={() => void overwriteTemplate(pendingOverwrite)}
              >{t("覆盖", "Overwrite")}</button>
            </>
          )}
        />
      )}
    </>
  );
}
