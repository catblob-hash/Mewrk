import type {
  ConversationSettings,
  ConversationToolLock,
  ConversationWebSearchSettings,
  FetchProviderSelection,
  ModelCapability,
  ProviderFamily,
  SearchProviderSelection,
  ToolLockRequest
} from "../types";
import { hostMessageContainerOf } from "./hostMessages";
import { appendsTools } from "./modelCapabilities";
import { familySupportsNativeFetch } from "./webSearch";

/**
 * The conversation's tool lock: the tool surface its last request went out
 * with, and which model sent it when.
 *
 * The prompt cache belongs to that model and that surface. While the model
 * selected now is the one the last request used and its cache is still warm, a
 * change that would rewrite the cached prefix is drawn orange and asks once
 * before it is made — picking another model lifts that, and picking this one
 * again puts it back. It is a warning, not a refusal: the change is the user's
 * to make.
 *
 * What rewrites the prefix depends on the model. One that takes a tool
 * mid-conversation (`appendsTools`) is handed an added tool at the end of the
 * transcript, so only taking one away rewrites the declared list. On any other
 * model the sidecar folds an added tool back into that list, so adding one
 * rewrites it just as surely, and the lock covers the whole surface in both
 * directions (`ToolLockState.wholeSurface`).
 *
 * Three fields are pins rather than parts of the surface — native search,
 * native fetch, and which skills the system prompt was built with. Each is
 * `null` until the run that sets it and never moves afterwards, whatever model
 * is selected: a second answer would contradict the transcript rather than
 * extend it. Native search pins only once the transcript holds a report it
 * produced (`nativeSearchRan`), and only while it does — offering `web_search`
 * is not enough, because on a family without native search (Claude Agent,
 * Chat Completions, OpenAI Compatible) every such call fails and nothing is
 * sealed, and the fix the failure asks for is choosing a catalog provider.
 * A host-run search or fetch
 * backend is not a pin: its results are ordinary tool output any backend can
 * follow, so it is part of the surface and only the cache says anything about
 * moving it.
 *
 * Every model the conversation has sent to keeps its own cache for its own
 * lifetime (`modelRequests`), which the composer's model menu marks; the
 * surface and its tone belong to the last request alone.
 */
export const EMPTY_TOOL_LOCK: ConversationToolLock = {
  tools: [],
  mcpIds: [],
  globalMemory: false,
  projectMemory: false,
  skillTool: false,
  mcpToolDiscovery: false,
  webSearch: false,
  planMode: false,
  skillIds: [],
  promptSkillIds: null,
  searchBackend: null,
  fetchBackend: null,
  webFetch: false,
  searchProvider: null,
  fetchProvider: null,
  lastRequest: null,
  modelRequests: [],
  hookIds: null,
  promptProfile: null,
  hostMessageContainer: null
};

/** The built-in prompt profile's id; a conversation that selects none uses it. */
export const BUILTIN_PROMPT_PROFILE_ID = "tooldesc_builtin_en_us";

/** The prompt profile these settings word their requests with. */
function promptProfileOf(settings: ConversationSettings): string {
  return settings.toolDescriptionFileId || BUILTIN_PROMPT_PROFILE_ID;
}

/** How long a model's prompt cache is taken to stay warm when its profile says nothing. */
export const DEFAULT_CACHE_TTL_MINUTES = 30;

/**
 * What this run's settings would put in front of the model, beyond what the
 * settings alone can say.
 *
 * `webFetch` depends on the family of the model this run uses (native fetch is
 * a second tool only on some), and `nativeSearched` on the transcript, neither
 * of which the settings carry.
 */
export interface ToolExposureContext {
  /** Whether this run grants `web_fetch` at all (`grantsWebFetch`). */
  webFetch: boolean;
  /** Whether the transcript holds a report a native search produced (`nativeSearchRan`). */
  nativeSearched: boolean;
}

/** The request a run is about to send: which model, and when. */
export interface ToolLockRequestContext extends ToolExposureContext {
  providerId: string;
  modelId: string;
  /** RFC 3339. */
  at: string;
}

/** The model selected now, as the lock needs to know it. */
export interface ToolLockModel {
  providerId: string;
  modelId: string;
  family: ProviderFamily;
  /** Whether the model takes a tool mid-conversation (`appendsTools`). */
  appendsTools: boolean;
  /** Minutes the model's prompt cache is taken to stay warm. */
  cacheTtlMinutes?: number;
}

/**
 * A conversation's lock, treating the absent key of a pre-lock conversation as
 * empty.
 *
 * A backend pin written before host-run backends joined the surface may name
 * one of them; it reads as no pin, since nothing in the transcript it stands
 * for is sealed to that backend. A lock from before `modelRequests` existed
 * still knows one model's request: its last.
 */
export function toolLockOf(settings: ConversationSettings): ConversationToolLock {
  const lock = settings.toolLock;
  if (!lock) return EMPTY_TOOL_LOCK;
  const lastRequest = lock.lastRequest ?? null;
  return {
    tools: lock.tools ?? [],
    mcpIds: lock.mcpIds ?? [],
    globalMemory: lock.globalMemory === true,
    projectMemory: lock.projectMemory === true,
    skillTool: lock.skillTool === true,
    mcpToolDiscovery: lock.mcpToolDiscovery === true,
    webSearch: lock.webSearch === true,
    planMode: lock.planMode === true,
    skillIds: lock.skillIds ?? [],
    promptSkillIds: lock.promptSkillIds ?? null,
    searchBackend: lock.searchBackend ?? null,
    fetchBackend: lock.fetchBackend ?? null,
    webFetch: lock.webFetch === true,
    searchProvider: nativePin(lock.searchProvider),
    fetchProvider: nativePin(lock.fetchProvider),
    lastRequest,
    modelRequests: lock.modelRequests?.length
      ? lock.modelRequests
      : lastRequest ? [lastRequest] : [],
    hookIds: lock.hookIds ?? null,
    promptProfile: lock.promptProfile ?? null,
    hostMessageContainer: lock.hostMessageContainer ?? null
  };
}

/** A backend selection as a pin: only native seals anything into the transcript. */
function nativePin<T extends { kind: string }>(selection: T | null | undefined): T | null {
  return selection?.kind === "native" ? selection : null;
}

/** The exposure these settings ask for, in the same shape as the lock. */
function toolExposureOf(
  settings: ConversationSettings,
  context: ToolExposureContext
): ConversationToolLock {
  const web = settings.webSearchEnabled === true;
  const searching = web && settings.webSearch.provider.kind !== "disabled";
  const fetching = web && context.webFetch;
  return {
    tools: settings.enabledTools,
    mcpIds: settings.mcpIds,
    globalMemory: settings.globalMemoryEnabled === true,
    projectMemory: settings.projectMemoryEnabled === true,
    skillTool: settings.skillToolEnabled === true,
    mcpToolDiscovery: settings.mcpToolDiscoveryEnabled === true,
    webSearch: web,
    planMode: settings.planModeEnabled === true,
    skillIds: settings.skillIds,
    /* What the system prompt would be built from if this were the first run.
       The merge below keeps whatever the first run actually used. */
    promptSkillIds: settings.skillIds,
    /* Both selections are recorded whether or not their tool went out: on a
       model that cannot take a tool mid-conversation the whole surface is put
       back from them, and "off" is part of it. */
    searchBackend: web ? settings.webSearch.provider : null,
    fetchBackend: web ? settings.webSearch.fetchProvider : null,
    webFetch: fetching,
    /* A pin records a native backend a run actually used, so a selection that
       sends no tool pins nothing: "this conversation does not search" leaves no
       provider-sealed residue in the transcript to be bound by. Search goes
       further and waits for the search itself, since only a model whose family
       has native search can produce one. */
    searchProvider: searching && context.nativeSearched ? nativePin(settings.webSearch.provider) : null,
    fetchProvider: fetching ? nativePin(settings.webSearch.fetchProvider) : null,
    lastRequest: null,
    modelRequests: [],
    /* Not tools, but the system prompt is built from them, so the same request
       caches them. */
    hookIds: settings.hookIds,
    promptProfile: promptProfileOf(settings),
    /* Not a tool, but `box` is declared only for it, and every host message
       the transcript holds is projected in it. */
    hostMessageContainer: hostMessageContainerOf(settings)
  };
}

/**
 * The lock a request leaves behind: its own surface and model replace the
 * last one's, each pin keeps the answer the first run that set it gave, and
 * the request becomes its model's latest.
 */
function nextToolLock(
  lock: ConversationToolLock,
  exposure: ConversationToolLock,
  request: { providerId: string; modelId: string; at: string },
  nativeSearched: boolean
): ConversationToolLock {
  const lastRequest: ToolLockRequest = { providerId: request.providerId, modelId: request.modelId, at: request.at };
  return {
    tools: [...new Set(exposure.tools)],
    mcpIds: [...new Set(exposure.mcpIds)],
    globalMemory: exposure.globalMemory,
    projectMemory: exposure.projectMemory,
    skillTool: exposure.skillTool,
    mcpToolDiscovery: exposure.mcpToolDiscovery,
    webSearch: exposure.webSearch,
    /* Sticky, as the host's plan pair is: once offered it stays offered,
       whatever the switch says now. */
    planMode: lock.planMode || exposure.planMode,
    skillIds: [...new Set(exposure.skillIds)],
    promptSkillIds: lock.promptSkillIds ?? exposure.promptSkillIds,
    searchBackend: exposure.searchBackend,
    fetchBackend: exposure.fetchBackend,
    webFetch: exposure.webFetch,
    searchProvider: searchPin(lock, exposure.searchProvider, nativeSearched),
    fetchProvider: lock.fetchProvider ?? exposure.fetchProvider,
    lastRequest,
    modelRequests: withModelRequest(lock.modelRequests, lastRequest),
    hookIds: [...new Set(exposure.hookIds ?? [])],
    promptProfile: exposure.promptProfile,
    hostMessageContainer: exposure.hostMessageContainer
  };
}

/**
 * The native-search pin, kept for as long as the transcript holds what sealed
 * it. Without that evidence there is nothing to contradict, so a pin a build
 * set merely for offering `web_search` — on a model that could never search
 * natively — is lifted at the next request rather than held forever.
 */
function searchPin(
  lock: ConversationToolLock,
  candidate: SearchProviderSelection | null,
  nativeSearched: boolean
): SearchProviderSelection | null {
  return nativeSearched ? lock.searchProvider ?? candidate : null;
}

/** The list with `request` as its model's latest, in place of any earlier one. */
function withModelRequest(requests: readonly ToolLockRequest[], request: ToolLockRequest): ToolLockRequest[] {
  return [
    ...requests.filter((item) => !sameModel(item, request)),
    request
  ];
}

function sameModel(
  left: { providerId: string; modelId: string },
  right: { providerId: string; modelId: string }
): boolean {
  return left.providerId === right.providerId && left.modelId === right.modelId;
}

/**
 * The lock with its last request moved to `at` — the moment a long run's final
 * request actually went out, which is what the cache is counted from — and its
 * native-search pin brought up to date with the transcript the run left: a
 * search that ran during the run pins native, provided the run went out with
 * native search and the selector still names it. Returns the lock unchanged
 * when there is no last request to move.
 */
export function refreshedToolLock(
  settings: ConversationSettings,
  at: string,
  nativeSearched: boolean
): ConversationToolLock {
  const lock = toolLockOf(settings);
  const last = lock.lastRequest;
  if (!last) return lock;
  const lastRequest = { ...last, at };
  const searchedNatively = lock.webSearch
    && lock.searchBackend?.kind === "native"
    && settings.webSearch.provider.kind === "native";
  return {
    ...lock,
    searchProvider: searchPin(lock, searchedNatively ? { kind: "native" } : null, nativeSearched),
    lastRequest,
    modelRequests: withModelRequest(lock.modelRequests ?? [], lastRequest)
  };
}

/** Guards the per-run write so an unchanged lock does not rewrite the conversation. */
function sameToolLock(left: ConversationToolLock, right: ConversationToolLock): boolean {
  return sameIdSet(left.tools, right.tools)
    && sameIdSet(left.mcpIds, right.mcpIds)
    && left.globalMemory === right.globalMemory
    && left.projectMemory === right.projectMemory
    && left.skillTool === right.skillTool
    && left.mcpToolDiscovery === right.mcpToolDiscovery
    && left.webSearch === right.webSearch
    && left.planMode === right.planMode
    && sameIdSet(left.skillIds, right.skillIds)
    && sameIdList(left.promptSkillIds, right.promptSkillIds)
    && sameSelection(left.searchBackend, right.searchBackend)
    && sameSelection(left.fetchBackend, right.fetchBackend)
    && left.webFetch === right.webFetch
    && sameSelection(left.searchProvider, right.searchProvider)
    && sameSelection(left.fetchProvider, right.fetchProvider)
    && sameRequest(left.lastRequest, right.lastRequest)
    && left.modelRequests.length === right.modelRequests.length
    && left.modelRequests.every((item) => right.modelRequests.some((other) => sameRequest(item, other)))
    && sameIdList(left.hookIds, right.hookIds)
    && left.promptProfile === right.promptProfile
    && left.hostMessageContainer === right.hostMessageContainer;
}

function sameRequest(left: ToolLockRequest | null, right: ToolLockRequest | null): boolean {
  if (left === null || right === null) return left === right;
  return sameModel(left, right) && left.at === right.at;
}

function sameIdSet(left: readonly string[], right: readonly string[]): boolean {
  const a = new Set(left);
  const b = new Set(right);
  return a.size === b.size && [...a].every((id) => b.has(id));
}

/** Order matters for neither pin, but "set at all" does. */
function sameIdList(left: string[] | null, right: string[] | null): boolean {
  if (left === null || right === null) return left === right;
  return sameIdSet(left, right);
}

/**
 * Both selections are discriminated unions whose only other field is a provider
 * kind, so the two fields are the whole comparison. Serializing instead would
 * make key order part of the answer.
 */
function sameSelection(
  left: { kind: string; providerKind?: string } | null,
  right: { kind: string; providerKind?: string } | null
): boolean {
  if (left === null || right === null) return left === right;
  return left.kind === right.kind && left.providerKind === right.providerKind;
}

/**
 * The settings a request leaves behind: its exposure recorded as the lock.
 * Returns the argument unchanged when nothing moved, so a caller can skip the
 * write rather than dirty the document once per round.
 */
export function withRunToolLock(
  settings: ConversationSettings,
  tools: string[],
  context: ToolLockRequestContext
): ConversationSettings {
  const current = toolLockOf(settings);
  const next = nextToolLock(
    current,
    { ...toolExposureOf(settings, context), tools },
    context,
    context.nativeSearched
  );
  if (settings.toolLock && sameToolLock(current, next)) return settings;
  return { ...settings, toolLock: next };
}

// ---------------------------------------------------------------- Lock state

/** Where a conversation's lock stands for the model selected now. */
export interface ToolLockState {
  lock: ConversationToolLock;
  /** The selected model is the one the last request used. */
  engaged: boolean;
  /**
   * …and it cannot take a tool mid-conversation, so adding one rewrites the
   * cached prefix as surely as removing one: while the cache is warm, the lock
   * covers the whole surface in both directions.
   */
  wholeSurface: boolean;
  /** …and its prompt cache is still warm: changes that would rewrite it are orange. */
  warm: boolean;
  /** When `warm` runs out, in epoch milliseconds, so a view can redraw then. */
  warmUntil: number | null;
  /** The selected model's family, which decides whether native fetch is a tool of its own. */
  family: ProviderFamily | null;
}

export function toolLockState(
  settings: ConversationSettings,
  model: ToolLockModel | null,
  now: number
): ToolLockState {
  const lock = toolLockOf(settings);
  const last = lock.lastRequest;
  const engaged = Boolean(model && last
    && last.providerId === model.providerId
    && last.modelId === model.modelId);
  if (!engaged || !model || !last) {
    return { lock, engaged: false, wholeSurface: false, warm: false, warmUntil: null, family: model?.family ?? null };
  }
  const warmUntil = cacheExpiry(last, model);
  return {
    lock,
    engaged: true,
    wholeSurface: !model.appendsTools,
    warm: warmUntil !== null && now < warmUntil,
    warmUntil,
    family: model.family
  };
}

/** When a request's cache runs out on `model`, in epoch milliseconds, or `null` for an unreadable time. */
function cacheExpiry(request: ToolLockRequest, model: { cacheTtlMinutes?: number }): number | null {
  const sentAt = Date.parse(request.at);
  const minutes = model.cacheTtlMinutes ?? DEFAULT_CACHE_TTL_MINUTES;
  return Number.isFinite(sentAt) ? sentAt + minutes * 60_000 : null;
}

/**
 * When this conversation's cache on `model` runs out, in epoch milliseconds —
 * counted from that model's own latest request here, whether or not it was the
 * last one — or `null` when that moment has passed or the model never sent.
 * The composer's model menu marks every model this answers for.
 */
export function modelCacheWarmUntil(
  settings: ConversationSettings,
  model: { providerId: string; modelId: string; cacheTtlMinutes?: number },
  now: number
): number | null {
  const request = toolLockOf(settings).modelRequests.find((item) => sameModel(item, model));
  const until = request ? cacheExpiry(request, model) : null;
  return until !== null && now < until ? until : null;
}

/**
 * The kinds of setting the lock covers, by which way of changing them rewrites
 * the cached prefix.
 *
 * - `tool`, `webSearch`: turning one on appends tools, which every protocol
 *   with an append interface takes at the end; only turning one off rewrites
 *   the declared list. A model without that interface takes the added tool
 *   into the declared list, so there either way rewrites it.
 * - `skill`: a skill added later arrives as a host notice at the end; removing
 *   one changes the system prompt it was built into. Skills are not tools, so
 *   adding one is free on every model.
 * - `mcp`, `memory`, `skillTool`, `discovery`: either way rewrites the prefix —
 *   the selected servers are listed in the system prompt, a memory tier's
 *   instructions sit ahead of the history, and the two delivery switches move
 *   skills and schemas between the prompt and the tool list.
 * - `hook`, `profile`: either way rewrites the prefix too — the system
 *   prompt lists the selected hooks, and the prompt profile words every tool
 *   description and the prompt itself.
 * - `hostMessages`: either way rewrites the prefix — every host message the
 *   transcript holds is projected again in the other container, and `box` is
 *   declared or dropped with it.
 */
export type LockedSettingKind =
  | "tool"
  | "webSearch"
  | "skill"
  | "mcp"
  | "memory"
  | "skillTool"
  | "discovery"
  | "hook"
  | "profile"
  | "hostMessages";

/**
 * The one tone the lock draws: orange, a warning that the change throws a warm
 * cache away. Nothing the lock covers is refused. A native backend a run has
 * already used is settled rather than locked (`backendPinned`), and says so in
 * its own words.
 */
export type LockTone = "cache";

const EITHER_WAY: ReadonlySet<LockedSettingKind> = new Set([
  "mcp",
  "memory",
  "skillTool",
  "discovery",
  "hook",
  "profile",
  "hostMessages"
]);

/** The kinds that are no part of the tool surface, whose additions no model folds into the declared list. */
const OFF_SURFACE: ReadonlySet<LockedSettingKind> = new Set(["skill", "hook", "profile"]);

/**
 * How one setting is drawn: orange while the cache is warm and changing it
 * would rewrite the prefix, plain otherwise. On a model that cannot take a tool
 * mid-conversation that is every part of the surface, on or off.
 *
 * A setting moved away from what the last request had is plain again — the
 * cache is already lost for it — and turns orange again once it is moved back.
 */
export function lockTone(
  state: ToolLockState,
  kind: LockedSettingKind,
  lastOn: boolean,
  nowOn: boolean
): LockTone | null {
  if (!state.warm || nowOn !== lastOn) return null;
  if (lastOn || EITHER_WAY.has(kind)) return "cache";
  return state.wholeSurface && !OFF_SURFACE.has(kind) ? "cache" : null;
}

/**
 * How the prompt-profile selector is drawn: orange while the cache is warm and
 * it still names the profile the last request was worded with. A lock from
 * before profiles were recorded knows nothing to protect.
 */
export function promptProfileTone(state: ToolLockState, settings: ConversationSettings): LockTone | null {
  if (state.lock.promptProfile === null) return null;
  return lockTone(state, "profile", state.lock.promptProfile === promptProfileOf(settings), true);
}

/**
 * How the host-message container choice is drawn: orange while the cache is
 * warm and it still names the container the last request projected its host
 * messages in. A lock from before the choice was recorded knows nothing to
 * protect.
 */
export function hostMessageContainerTone(state: ToolLockState, settings: ConversationSettings): LockTone | null {
  if (state.lock.hostMessageContainer === null) return null;
  return lockTone(
    state,
    "hostMessages",
    state.lock.hostMessageContainer === hostMessageContainerOf(settings),
    true
  );
}

/**
 * How the composer's plan-mode switch is drawn. Its pair is sticky — once
 * offered it stays offered, and its guidance is an appended system prompt — so
 * switching plan mode off never touches the cache, and switching it on touches
 * it only before the pair has ever gone out, on a model that folds the two
 * tools into the declared list: orange there while the cache is warm, plain
 * everywhere else.
 */
export function planModeTone(state: ToolLockState, planModeEnabled: boolean): LockTone | null {
  return state.warm && state.wholeSurface && !state.lock.planMode && !planModeEnabled ? "cache" : null;
}

/** The two web legs, each with a backend selector of its own. */
export type WebBackendLeg = "search" | "fetch";

/**
 * How a web backend selector is drawn: orange while the cache is warm and the
 * selector still names the backend the last request had. A leg whose tool did
 * not go out is plain on a model that appends tools, because giving it one is
 * an addition; on any other model giving it one rewrites the declared list, so
 * the selector is orange there too.
 *
 * A pinned native backend is not drawn this way: it is settled for every model
 * (`backendPinned`), and its selector says so instead.
 */
export function backendTone(
  state: ToolLockState,
  leg: WebBackendLeg,
  current: SearchProviderSelection | FetchProviderSelection
): LockTone | null {
  if (!state.warm || backendPinned(state.lock, leg)) return null;
  const last = leg === "search" ? state.lock.searchBackend : state.lock.fetchBackend;
  if (!sameSelection(last, current)) return null;
  return legSent(state.lock, leg) || state.wholeSurface ? "cache" : null;
}

/** Whether the last request offered this leg's tool. */
function legSent(lock: ConversationToolLock, leg: WebBackendLeg): boolean {
  return leg === "search"
    ? lock.webSearch && lock.searchBackend !== null && lock.searchBackend.kind !== "disabled"
    : lock.webFetch;
}

/**
 * Whether a backend choice offers its leg's tool, by the host's one rule for
 * both (`web_search::apply_web_tools`): anything but Off does, except native
 * fetch on a family that reads pages inside its search tool. Whether the
 * provider is configured never enters into it.
 */
function choiceOffersTool(
  leg: WebBackendLeg,
  choice: SearchProviderSelection | FetchProviderSelection,
  family: ProviderFamily | null
): boolean {
  if (choice.kind === "disabled") return false;
  return leg === "fetch" && choice.kind === "native" ? familySupportsNativeFetch(family ?? undefined) : true;
}

/**
 * Whether moving a leg's selector from what the last request had to `choice`
 * rewrites the cached prefix. A leg whose tool went out is rewritten by any
 * move — the tool's own wording follows its backend. A leg whose tool did not
 * is rewritten only by a choice that offers the tool, and only on a model that
 * folds an added tool into the declared list.
 */
function backendMoveRewrites(
  state: ToolLockState,
  leg: WebBackendLeg,
  choice: SearchProviderSelection | FetchProviderSelection
): boolean {
  if (legSent(state.lock, leg)) return true;
  return state.wholeSurface && choiceOffersTool(leg, choice, state.family);
}

/** Whether a leg's native backend is pinned, which no model and no cache lifts. */
export function backendPinned(lock: ConversationToolLock, leg: WebBackendLeg): boolean {
  return (leg === "search" ? lock.searchProvider : lock.fetchProvider) !== null;
}

/** The web settings with each leg named by `restore` put back to what the last request had. */
function restoredBackends(
  web: ConversationWebSearchSettings,
  lock: ConversationToolLock,
  restore: { search: boolean; fetch: boolean }
): ConversationWebSearchSettings {
  const provider = restore.search && lock.searchBackend
    && !sameSelection(lock.searchBackend, web.provider)
    ? lock.searchBackend
    : web.provider;
  const fetchProvider = restore.fetch && lock.fetchBackend
    && !sameSelection(lock.fetchBackend, web.fetchProvider)
    ? lock.fetchBackend
    : web.fetchProvider;
  return provider === web.provider && fetchProvider === web.fetchProvider
    ? web
    : { ...web, provider, fetchProvider };
}

/**
 * Puts the settings the lock holds back to what the last request had, for the
 * moment the model that sent it is selected again.
 *
 * Only what the lock actually holds moves — what `lockTone` and `backendTone`
 * would draw orange. On a model that appends tools, a tool the user added since
 * stays, because adding it cost nothing; on any other model it goes too, since
 * there adding it rewrote the declared list. A cold cache holds nothing.
 * Returns the argument when nothing moves.
 */
export function restoreLockedSettings(
  settings: ConversationSettings,
  state: ToolLockState
): ConversationSettings {
  if (!state.engaged || !state.warm) return settings;
  const lock = state.lock;
  const whole = state.wholeSurface;
  const union = (now: readonly string[], last: readonly string[]) => [...new Set([...now, ...last])];
  /* The container goes back whenever the lock holds one: it is part of the
     surface (`box` comes and goes with it) and of the cached prefix alike. */
  const container: Partial<ConversationSettings> = lock.hostMessageContainer !== null
    && lock.hostMessageContainer !== hostMessageContainerOf(settings)
    ? { hostMessageContainer: lock.hostMessageContainer }
    : {};
  const next: ConversationSettings = {
    ...settings,
    ...container,
    enabledTools: whole ? [...lock.tools] : union(settings.enabledTools, lock.tools),
    mcpIds: [...lock.mcpIds],
    globalMemoryEnabled: lock.globalMemory,
    projectMemoryEnabled: lock.projectMemory,
    skillToolEnabled: lock.skillTool,
    mcpToolDiscoveryEnabled: lock.mcpToolDiscovery,
    webSearchEnabled: whole ? lock.webSearch : settings.webSearchEnabled === true || lock.webSearch,
    skillIds: union(settings.skillIds, lock.skillIds),
    /* On a model that appends tools only a leg whose tool went out is held;
       on any other model every leg the last request recorded is. A pinned leg
       stays on its pin, which no restore moves. */
    webSearch: restoredBackends(settings.webSearch, lock, {
      search: !backendPinned(lock, "search") && (whole || legSent(lock, "search")),
      fetch: !backendPinned(lock, "fetch") && (whole || legSent(lock, "fetch"))
    }),
    /* What the system prompt was built from besides the tools, as far as the
       lock recorded it. */
    ...(lock.hookIds !== null ? { hookIds: [...lock.hookIds] } : {}),
    ...(lock.promptProfile !== null && lock.promptProfile !== promptProfileOf(settings)
      ? {
        toolDescriptionFileId: lock.promptProfile === BUILTIN_PROMPT_PROFILE_ID ? null : lock.promptProfile
      }
      : {})
  };
  return sameSurface(settings, next) ? settings : next;
}

function sameSurface(left: ConversationSettings, right: ConversationSettings): boolean {
  return sameIdSet(left.enabledTools, right.enabledTools)
    && sameIdSet(left.mcpIds, right.mcpIds)
    && sameIdSet(left.skillIds, right.skillIds)
    && Boolean(left.globalMemoryEnabled) === Boolean(right.globalMemoryEnabled)
    && Boolean(left.projectMemoryEnabled) === Boolean(right.projectMemoryEnabled)
    && Boolean(left.skillToolEnabled) === Boolean(right.skillToolEnabled)
    && Boolean(left.mcpToolDiscoveryEnabled) === Boolean(right.mcpToolDiscoveryEnabled)
    && Boolean(left.webSearchEnabled) === Boolean(right.webSearchEnabled)
    && left.webSearch === right.webSearch
    && sameIdSet(left.hookIds, right.hookIds)
    && promptProfileOf(left) === promptProfileOf(right)
    && hostMessageContainerOf(left) === hostMessageContainerOf(right);
}

/** The selection lists a dangling row can belong to. */
export type DanglingSelectionKind = "skills" | "mcp" | "hooks" | "agents";

/**
 * The settings with a dangling selection — an id the catalog no longer has —
 * unticked, from the conversation and from its lock alike.
 *
 * The host fails every run while such an id is selected, so clearing it must
 * always be possible: it is never toned and never asks — the entry it named has
 * nothing left to declare, so nothing joins or leaves. It also leaves the lock,
 * or the next restore of a warm surface would put back the very id that stops
 * every run. A role takes part in no lock — its settings are no part of the
 * calling conversation's prompt cache — so for one only the selection changes.
 */
export function withoutDanglingSelection(
  settings: ConversationSettings,
  kind: DanglingSelectionKind,
  id: string
): ConversationSettings {
  const drop = (ids: readonly string[]) => ids.filter((existing) => existing !== id);
  const lock = settings.toolLock;
  if (kind === "agents") return { ...settings, agentIds: drop(settings.agentIds) };
  const nextLock = !lock
    ? lock
    : kind === "mcp"
      ? { ...lock, mcpIds: drop(lock.mcpIds ?? []) }
      : kind === "skills"
        ? { ...lock, skillIds: drop(lock.skillIds ?? []) }
        : lock.hookIds ? { ...lock, hookIds: drop(lock.hookIds) } : lock;
  const next: ConversationSettings = kind === "mcp"
    ? { ...settings, mcpIds: drop(settings.mcpIds) }
    : kind === "skills"
      ? { ...settings, skillIds: drop(settings.skillIds) }
      : { ...settings, hookIds: drop(settings.hookIds) };
  return nextLock === lock ? next : { ...next, toolLock: nextLock };
}

/** The lock's view of the model selected now, or `null` when none is. */
export function toolLockModelOf(
  provider: { id: string; family: ProviderFamily } | undefined,
  model: { id: string; capabilities?: readonly ModelCapability[]; cacheTtlMinutes?: number } | undefined
): ToolLockModel | null {
  if (!provider || !model) return null;
  return {
    providerId: provider.id,
    modelId: model.id,
    family: provider.family,
    appendsTools: appendsTools(provider, model),
    cacheTtlMinutes: model.cacheTtlMinutes
  };
}

/**
 * Whether going from `before` to `after` moves any setting `lockTone` draws
 * orange in `before`, so that the move throws the warm cache away — the one
 * question the settings pane asks before writing a change, so every page it
 * draws shares one warning.
 */
export function lockTouch(
  state: ToolLockState,
  before: ConversationSettings,
  after: ConversationSettings
): boolean {
  let touched = false;
  const note = (tone: LockTone | null) => {
    if (tone === "cache") touched = true;
  };
  const lists: Array<[LockedSettingKind, readonly string[], readonly string[], readonly string[]]> = [
    ["tool", state.lock.tools, before.enabledTools, after.enabledTools],
    ["mcp", state.lock.mcpIds, before.mcpIds, after.mcpIds],
    ["skill", state.lock.skillIds, before.skillIds, after.skillIds]
  ];
  /* An older lock never recorded hooks, so it has nothing to say about them. */
  if (state.lock.hookIds !== null) {
    lists.push(["hook", state.lock.hookIds, before.hookIds, after.hookIds]);
  }
  for (const [kind, last, from, to] of lists) {
    const lastSet = new Set(last);
    const fromSet = new Set(from);
    const toSet = new Set(to);
    for (const id of new Set([...from, ...to])) {
      if (fromSet.has(id) === toSet.has(id)) continue;
      note(lockTone(state, kind, lastSet.has(id), fromSet.has(id)));
    }
  }
  const switches: Array<[LockedSettingKind, boolean, boolean | undefined, boolean | undefined]> = [
    ["webSearch", state.lock.webSearch, before.webSearchEnabled, after.webSearchEnabled],
    ["memory", state.lock.globalMemory, before.globalMemoryEnabled, after.globalMemoryEnabled],
    ["memory", state.lock.projectMemory, before.projectMemoryEnabled, after.projectMemoryEnabled],
    ["skillTool", state.lock.skillTool, before.skillToolEnabled, after.skillToolEnabled],
    ["discovery", state.lock.mcpToolDiscovery, before.mcpToolDiscoveryEnabled, after.mcpToolDiscoveryEnabled]
  ];
  for (const [kind, lastOn, from, to] of switches) {
    if (Boolean(from) === Boolean(to)) continue;
    note(lockTone(state, kind, lastOn, Boolean(from)));
  }
  if (promptProfileOf(before) !== promptProfileOf(after)) {
    note(promptProfileTone(state, before));
  }
  if (hostMessageContainerOf(before) !== hostMessageContainerOf(after)) {
    note(hostMessageContainerTone(state, before));
  }
  const legs: Array<[WebBackendLeg, SearchProviderSelection | FetchProviderSelection, SearchProviderSelection | FetchProviderSelection]> = [
    ["search", before.webSearch.provider, after.webSearch.provider],
    ["fetch", before.webSearch.fetchProvider, after.webSearch.fetchProvider]
  ];
  for (const [leg, from, to] of legs) {
    if (sameSelection(from, to) || !backendMoveRewrites(state, leg, to)) continue;
    note(backendTone(state, leg, from));
  }
  return touched;
}
