import { describe, expect, it } from "vitest";
import type { ConversationSettings, ConversationToolLock } from "../types";
import { defaultConversationWebSearchSettings } from "./runtime";
import {
  backendPinned,
  backendTone,
  BUILTIN_PROMPT_PROFILE_ID,
  DEFAULT_CACHE_TTL_MINUTES,
  EMPTY_TOOL_LOCK,
  fileWriteGuardsTone,
  hostMessageContainerTone,
  lockTone,
  lockTouch,
  modelCacheWarmUntil,
  planModeTone,
  promptProfileTone,
  refreshedToolLock,
  restoreLockedSettings,
  toolLockModelOf,
  toolLockOf,
  toolLockState,
  withoutDanglingSelection,
  withRunToolLock,
  type ToolLockModel
} from "./toolLock";

function settings(patch: Partial<ConversationSettings> = {}): ConversationSettings {
  return {
    enabledTools: [],
    hookIds: [],
    skillIds: [],
    mcpIds: [],
    toolDescriptionFileId: null,
    agentIds: [],
    allowRolelessSubagents: false,
    webSearch: defaultConversationWebSearchSettings(),
    webSearchEnabled: false,
    reasoningEffort: "medium",
    securityLevel: "request_approval",
    globalMemoryEnabled: false,
    projectMemoryEnabled: false,
    skillToolEnabled: false,
    mcpToolDiscoveryEnabled: false,
    ...patch
  };
}

const SENT_AT = "2026-09-30T10:00:00.000Z";
const SENT = Date.parse(SENT_AT);
const MINUTE = 60_000;

/** A model that appends tools, and one whose protocol cannot. */
const OPUS: ToolLockModel = { providerId: "anthropic", modelId: "claude-opus-5-5", family: "anthropic", appendsTools: true };
const HAIKU: ToolLockModel = { providerId: "anthropic", modelId: "claude-haiku-4-5", family: "anthropic", appendsTools: false };
const GPT: ToolLockModel = { providerId: "openai", modelId: "gpt-5.5", family: "openai_responses", appendsTools: true };

/** A lock as a request by `model` would have left it, so a test only states what it is about. */
function lock(model: ToolLockModel, patch: Partial<ConversationToolLock> = {}): ConversationToolLock {
  return {
    ...EMPTY_TOOL_LOCK,
    promptSkillIds: [],
    lastRequest: { providerId: model.providerId, modelId: model.modelId, at: SENT_AT },
    ...patch
  };
}

const OFFLINE = { webFetch: false, nativeSearched: false };
/** A run that offered `web_fetch` after a native search had already run. */
const FETCHED_AFTER_SEARCH = { webFetch: true, nativeSearched: true };
/** The transcript already holds a report a native search produced. */
const SEARCHED = { webFetch: false, nativeSearched: true };
const AGENT: ToolLockModel = { providerId: "agent", modelId: "claude-opus-5-5", family: "claude_agent", appendsTools: true };
const request = (model: ToolLockModel, context = OFFLINE) => ({
  ...context,
  providerId: model.providerId,
  modelId: model.modelId,
  at: SENT_AT
});

describe("the lock a request leaves", () => {
  it("is empty and unengaged for a conversation that has never run", () => {
    const current = settings({ enabledTools: ["read"], globalMemoryEnabled: true });
    expect(toolLockOf(current).tools).toEqual([]);
    expect(toolLockOf(current).lastRequest).toBeNull();
    // Not the same as "opened its prompt with no skills": until a run says so,
    // every selected skill is still part of the prompt about to be built.
    expect(toolLockOf(current).promptSkillIds).toBeNull();
    expect(toolLockState(current, OPUS, SENT).engaged).toBe(false);
  });

  it("records each request's own surface and model, replacing the last one's", () => {
    const first = withRunToolLock(
      settings({ enabledTools: ["read", "write"], globalMemoryEnabled: true }),
      ["read", "write"],
      request(OPUS)
    );
    expect(toolLockOf(first).tools).toEqual(["read", "write"]);
    expect(toolLockOf(first).lastRequest).toEqual({ providerId: "anthropic", modelId: "claude-opus-5-5", at: SENT_AT });

    // The next request went out without `write` and without global memory, on
    // another model: that is what its cache holds now.
    const second = withRunToolLock({ ...first, globalMemoryEnabled: false }, ["read"], request(GPT));
    expect(toolLockOf(second).tools).toEqual(["read"]);
    expect(toolLockOf(second).globalMemory).toBe(false);
    expect(toolLockOf(second).lastRequest?.modelId).toBe("gpt-5.5");

    // Nothing moved: the same object, so the caller skips the write.
    expect(withRunToolLock(second, ["read"], request(GPT))).toBe(second);
  });

  it("keeps the plan pair once a request offered it, as the host does", () => {
    const planning = withRunToolLock(settings({ planModeEnabled: true }), [], request(OPUS));
    expect(toolLockOf(planning).planMode).toBe(true);
    // The plan was approved and the switch went off; the pair did not leave.
    const approved = withRunToolLock({ ...planning, planModeEnabled: false }, [], request(OPUS));
    expect(toolLockOf(approved).planMode).toBe(true);
  });

  it("records a host-run backend as the surface, and pins only a native one that went out", () => {
    const hostRun = withRunToolLock(settings({
      webSearchEnabled: true,
      webSearch: {
        ...defaultConversationWebSearchSettings(),
        provider: { kind: "explicit", providerKind: "tavily" },
        fetchProvider: { kind: "disabled" }
      }
    }), [], request(OPUS));
    // Its results are ordinary tool output any backend can follow: nothing to pin.
    expect(toolLockOf(hostRun).searchProvider).toBeNull();
    expect(toolLockOf(hostRun).searchBackend).toEqual({ kind: "explicit", providerKind: "tavily" });
    expect(toolLockOf(hostRun).fetchBackend).toEqual({ kind: "disabled" });
    expect(toolLockOf(hostRun).webFetch).toBe(false);
    // No `web_fetch` went out, so nothing about fetching is settled yet.
    expect(toolLockOf(hostRun).fetchProvider).toBeNull();

    const native = withRunToolLock(settings({
      webSearchEnabled: true,
      webSearch: { ...defaultConversationWebSearchSettings(), provider: { kind: "native" }, fetchProvider: { kind: "native" } }
    }), [], request(OPUS, FETCHED_AFTER_SEARCH));
    expect(toolLockOf(native).searchProvider).toEqual({ kind: "native" });
    expect(toolLockOf(native).fetchProvider).toEqual({ kind: "native" });
    expect(toolLockOf(native).webFetch).toBe(true);

    // Native fetch on a family that folds retrieval into search grants no
    // `web_fetch`, so it seals nothing and pins nothing.
    const folded = withRunToolLock(settings({
      webSearchEnabled: true,
      webSearch: { ...defaultConversationWebSearchSettings(), fetchProvider: { kind: "native" } }
    }), [], request(GPT));
    expect(toolLockOf(folded).fetchProvider).toBeNull();

    // Switching backends afterwards leaves a native pin where it was, while the
    // surface follows the request.
    const moved = withRunToolLock({
      ...native,
      webSearch: {
        ...native.webSearch,
        provider: { kind: "explicit", providerKind: "exa" },
        fetchProvider: { kind: "explicit", providerKind: "jina" }
      }
    }, [], request(OPUS, FETCHED_AFTER_SEARCH));
    expect(toolLockOf(moved).searchProvider).toEqual({ kind: "native" });
    expect(toolLockOf(moved).fetchProvider).toEqual({ kind: "native" });
    expect(toolLockOf(moved).searchBackend).toEqual({ kind: "explicit", providerKind: "exa" });

    // A host-run backend used first does not stop native from pinning later.
    const thenNative = withRunToolLock({
      ...hostRun,
      webSearch: { ...hostRun.webSearch, provider: { kind: "native" } }
    }, [], request(OPUS, SEARCHED));
    expect(toolLockOf(thenNative).searchProvider).toEqual({ kind: "native" });
  });

  /* Offering `web_search` with Native is not searching with it: on a family
     with no native search every call fails, and the fix that failure asks for
     is choosing a provider, which a pin would make impossible. */
  it("pins native search only once a native search has run", () => {
    const nativeSearch = settings({
      webSearchEnabled: true,
      webSearch: { ...defaultConversationWebSearchSettings(), provider: { kind: "native" } }
    });
    const offered = withRunToolLock(nativeSearch, [], request(AGENT));
    expect(toolLockOf(offered).searchProvider).toBeNull();
    expect(backendTone(toolLockState(offered, AGENT, SENT + 90 * MINUTE), "search", { kind: "native" }))
      .toBeNull();
    // A provider can be chosen at any time, then.
    const repaired = { ...offered, webSearch: { ...offered.webSearch, provider: { kind: "explicit", providerKind: "exa-mcp" } as const } };
    expect(lockTouch(toolLockState(offered, AGENT, SENT + 90 * MINUTE), offered, repaired)).toBe(false);

    // On a model that searches natively, the run's own search pins native at
    // the run's end, when the report is in the transcript.
    const sent = withRunToolLock(nativeSearch, [], request(OPUS));
    expect(toolLockOf(sent).searchProvider).toBeNull();
    const later = "2026-09-30T10:05:00.000Z";
    expect(refreshedToolLock(sent, later, false).searchProvider).toBeNull();
    const searched = refreshedToolLock(sent, later, true);
    expect(searched.searchProvider).toEqual({ kind: "native" });
    expect(backendPinned(searched, "search")).toBe(true);
    // A run that went out with a host-run backend seals nothing natively.
    const hostRun = withRunToolLock({
      ...nativeSearch,
      webSearch: { ...nativeSearch.webSearch, provider: { kind: "explicit", providerKind: "tavily" } }
    }, [], request(OPUS));
    expect(refreshedToolLock(hostRun, later, true).searchProvider).toBeNull();
  });

  /* A pin an older build set merely for offering native search, with no report
     in the transcript to contradict, is lifted at the next request. */
  it("lifts a native search pin the transcript gives no reason for", () => {
    const stale = settings({
      webSearchEnabled: true,
      webSearch: { ...defaultConversationWebSearchSettings(), provider: { kind: "native" } },
      toolLock: lock(AGENT, { webSearch: true, searchBackend: { kind: "native" }, searchProvider: { kind: "native" } })
    });
    expect(toolLockOf(withRunToolLock(stale, [], request(AGENT))).searchProvider).toBeNull();
    expect(toolLockOf(withRunToolLock(stale, [], request(AGENT, SEARCHED))).searchProvider)
      .toEqual({ kind: "native" });
  });

  it("reads a host-run pin written by an older build as no pin", () => {
    const older = settings({
      toolLock: lock(OPUS, {
        searchProvider: { kind: "explicit", providerKind: "tavily" },
        fetchProvider: { kind: "explicit", providerKind: "jina" }
      })
    });
    expect(toolLockOf(older).searchProvider).toBeNull();
    expect(toolLockOf(older).fetchProvider).toBeNull();
  });

  it("records the opening prompt's skills on the first run, even when there were none", () => {
    const first = withRunToolLock(settings(), ["read"], request(OPUS));
    expect(toolLockOf(first).promptSkillIds).toEqual([]);
    const added = withRunToolLock({ ...first, skillIds: ["skill-a"] }, ["read"], request(OPUS));
    expect(toolLockOf(added).promptSkillIds).toEqual([]);
    expect(toolLockOf(added).skillIds).toEqual(["skill-a"]);
  });
});

describe("where the lock stands for the selected model", () => {
  it("engages only for the model the last request used", () => {
    const current = settings({ toolLock: lock(OPUS, { tools: ["read"] }) });
    expect(toolLockState(current, OPUS, SENT).engaged).toBe(true);
    expect(toolLockState(current, GPT, SENT).engaged).toBe(false);
    expect(toolLockState(current, { ...OPUS, providerId: "relay" }, SENT).engaged).toBe(false);
    expect(toolLockState(current, null, SENT).engaged).toBe(false);
  });

  it("keeps the cache warm for the model's lifetime, thirty minutes by default", () => {
    const current = settings({ toolLock: lock(OPUS) });
    expect(DEFAULT_CACHE_TTL_MINUTES).toBe(30);
    expect(toolLockState(current, OPUS, SENT + 29 * MINUTE).warm).toBe(true);
    expect(toolLockState(current, OPUS, SENT + 30 * MINUTE).warm).toBe(false);
    expect(toolLockState(current, OPUS, SENT).warmUntil).toBe(SENT + 30 * MINUTE);

    const hourly = { ...OPUS, cacheTtlMinutes: 60 };
    expect(toolLockState(current, hourly, SENT + 45 * MINUTE).warm).toBe(true);
    expect(toolLockState(current, { ...OPUS, cacheTtlMinutes: 5 }, SENT + 6 * MINUTE).warm).toBe(false);
  });

  it("covers the whole surface of a model that cannot append tools, but only while its cache is warm", () => {
    const current = settings({ toolLock: lock(HAIKU) });
    const warm = toolLockState(current, HAIKU, SENT);
    expect(warm.wholeSurface).toBe(true);
    expect(warm.warm).toBe(true);
    // Cold, the cache has nothing left to lose: the model alone holds nothing.
    const cold = toolLockState(current, HAIKU, SENT + 2 * 60 * MINUTE);
    expect(cold.warm).toBe(false);
    expect(lockTone(cold, "tool", false, false)).toBeNull();
    expect(lockTone(cold, "tool", true, true)).toBeNull();
    expect(toolLockState(settings({ toolLock: lock(OPUS) }), OPUS, SENT).wholeSurface).toBe(false);
    // Another model's request is not this one's to cover.
    expect(toolLockState(current, OPUS, SENT).wholeSurface).toBe(false);
  });

  it("builds the model's view from the provider and profile, or none", () => {
    expect(toolLockModelOf({ id: "p", family: "anthropic" }, { id: "m", cacheTtlMinutes: 5 }))
      .toEqual({ providerId: "p", modelId: "m", family: "anthropic", appendsTools: false, cacheTtlMinutes: 5 });
    // Whether the lock covers the whole surface is the model's declared
    // capability, read only where the protocol has an append interface.
    expect(toolLockModelOf({ id: "p", family: "anthropic" }, { id: "m", capabilities: ["tool_append"] })?.appendsTools)
      .toBe(true);
    expect(toolLockModelOf({ id: "p", family: "openai_compatible" }, { id: "m", capabilities: ["tool_append"] })?.appendsTools)
      .toBe(false);
    expect(toolLockModelOf(undefined, { id: "m" })).toBeNull();
    expect(toolLockModelOf({ id: "p", family: "anthropic" }, undefined)).toBeNull();
  });
});

describe("how a setting is drawn", () => {
  const warm = toolLockState(settings({ toolLock: lock(OPUS) }), OPUS, SENT);
  const cold = toolLockState(settings({ toolLock: lock(OPUS) }), OPUS, SENT + 31 * MINUTE);
  const whole = toolLockState(settings({ toolLock: lock(HAIKU) }), HAIKU, SENT);

  it("draws a tool the last request carried orange while the cache is warm", () => {
    expect(lockTone(warm, "tool", true, true)).toBe("cache");
    // Adding a tool appends it at the end: nothing cached is lost.
    expect(lockTone(warm, "tool", false, false)).toBeNull();
    // Moved away, the cache is already lost for it; moved back, orange again.
    expect(lockTone(warm, "tool", true, false)).toBeNull();
    expect(lockTone(cold, "tool", true, true)).toBeNull();
  });

  it("draws the settings that rewrite the prefix either way orange whichever way they stand", () => {
    for (const kind of ["mcp", "memory", "skillTool", "discovery", "hostMessages", "fileWriteGuards"] as const) {
      expect(lockTone(warm, kind, false, false)).toBe("cache");
      expect(lockTone(warm, kind, true, true)).toBe("cache");
      expect(lockTone(warm, kind, false, true)).toBeNull();
    }
  });

  it("draws every part of the surface orange, on or off, on a model that cannot append tools while the cache is warm", () => {
    for (const kind of ["tool", "webSearch", "mcp", "memory", "skillTool", "discovery"] as const) {
      // Adding one folds it into the declared list, just as removing one rewrites it.
      expect(lockTone(whole, kind, false, false)).toBe("cache");
      expect(lockTone(whole, kind, true, true)).toBe("cache");
      // Moved away, the cache is already lost for it: plain until it is moved back.
      expect(lockTone(whole, kind, true, false)).toBeNull();
      expect(lockTone(whole, kind, false, true)).toBeNull();
    }
    // Skills are not tools: one added later is a host notice on every model,
    // and only the cache speaks for the ones already sent.
    expect(lockTone(whole, "skill", false, false)).toBeNull();
    expect(lockTone(whole, "skill", true, true)).toBe("cache");
  });

  it("tones nothing on a model that cannot append tools once the cache is cold, nor for another model", () => {
    const current = settings({ toolLock: lock(HAIKU) });
    const cold = toolLockState(current, HAIKU, SENT + 31 * MINUTE);
    const elsewhere = toolLockState(current, OPUS, SENT);
    const kinds = ["tool", "webSearch", "skill", "mcp", "memory", "skillTool", "discovery", "hook", "profile", "hostMessages", "fileWriteGuards"] as const;
    for (const state of [cold, elsewhere]) {
      for (const kind of kinds) {
        expect(lockTone(state, kind, false, false)).toBeNull();
        expect(lockTone(state, kind, true, true)).toBeNull();
      }
    }
  });
});

describe("putting the lock back when its model is picked again", () => {
  it("returns what the cache holds and keeps what was added for free", () => {
    const lastSent = settings({
      enabledTools: ["write", "bash"],
      mcpIds: ["mcp-b"],
      globalMemoryEnabled: false,
      toolLock: lock(OPUS, { tools: ["read", "write"], mcpIds: ["mcp-a"], globalMemory: true, skillIds: ["s"] })
    });
    const restored = restoreLockedSettings(lastSent, toolLockState(lastSent, OPUS, SENT));
    expect(restored.enabledTools).toEqual(["write", "bash", "read"]);
    expect(restored.mcpIds).toEqual(["mcp-a"]);
    expect(restored.globalMemoryEnabled).toBe(true);
    expect(restored.skillIds).toEqual(["s"]);
  });

  it("puts the whole surface back exactly on a model that cannot append tools while the cache is warm, and nothing once it is cold", () => {
    const moved = settings({
      enabledTools: ["read", "bash"],
      mcpIds: ["mcp-b"],
      webSearchEnabled: true,
      skillIds: ["t"],
      hookIds: ["hook_b"],
      toolDescriptionFileId: null,
      planModeEnabled: true,
      toolLock: lock(HAIKU, {
        tools: ["read", "write"],
        mcpIds: ["mcp-a"],
        skillIds: ["s"],
        hookIds: ["hook_a"],
        promptProfile: "tooldesc_user_terse_01234567"
      })
    });
    const restored = restoreLockedSettings(moved, toolLockState(moved, HAIKU, SENT));
    // A tool added since goes too: on this model adding it rewrote the declared list.
    expect(restored.enabledTools).toEqual(["read", "write"]);
    expect(restored.mcpIds).toEqual(["mcp-a"]);
    expect(restored.webSearchEnabled).toBe(false);
    // A skill added since is a host notice, which cost nothing: it stays.
    expect(restored.skillIds).toEqual(["t", "s"]);
    // What the system prompt was built from goes back as well.
    expect(restored.hookIds).toEqual(["hook_a"]);
    expect(restored.toolDescriptionFileId).toBe("tooldesc_user_terse_01234567");
    // Plan mode is the composer's switch, not part of the surface: putting the
    // tools back leaves it where the user put it.
    expect(restored.planModeEnabled).toBe(true);

    // Cold, the cache holds nothing to put back, whatever the model.
    expect(restoreLockedSettings(moved, toolLockState(moved, HAIKU, SENT + 60 * MINUTE))).toBe(moved);
  });

  it("moves nothing for another model, a cold cache, or settings already in place", () => {
    const moved = settings({ enabledTools: ["bash"], toolLock: lock(OPUS, { tools: ["read"] }) });
    expect(restoreLockedSettings(moved, toolLockState(moved, GPT, SENT))).toBe(moved);
    expect(restoreLockedSettings(moved, toolLockState(moved, OPUS, SENT + 31 * MINUTE))).toBe(moved);
    const wholeMoved = { ...moved, toolLock: lock(HAIKU, { tools: ["read"] }) };
    expect(restoreLockedSettings(wholeMoved, toolLockState(wholeMoved, OPUS, SENT))).toBe(wholeMoved);
    const inPlace = settings({ enabledTools: ["read"], toolLock: lock(OPUS, { tools: ["read"] }) });
    expect(restoreLockedSettings(inPlace, toolLockState(inPlace, OPUS, SENT))).toBe(inPlace);
  });
});

describe("what a change does to the lock", () => {
  const before = settings({
    enabledTools: ["read"],
    skillIds: ["s"],
    toolLock: lock(OPUS, { tools: ["read"], skillIds: ["s"] })
  });
  const warm = toolLockState(before, OPUS, SENT);

  it("warns when an orange setting moves, and only then", () => {
    expect(lockTouch(warm, before, { ...before, enabledTools: [] })).toBe(true);
    expect(lockTouch(warm, before, { ...before, enabledTools: ["read", "bash"] })).toBe(false);
    expect(lockTouch(warm, before, { ...before, skillIds: [] })).toBe(true);
    expect(lockTouch(warm, before, { ...before, skillIds: ["s", "t"] })).toBe(false);
    expect(lockTouch(warm, before, { ...before, mcpIds: ["mcp-a"] })).toBe(true);
    expect(lockTouch(warm, before, { ...before, projectMemoryEnabled: true })).toBe(true);
    // Settings the lock does not cover never warn.
    expect(lockTouch(warm, before, { ...before, securityLevel: "full_access" })).toBe(false);
  });

  it("warns before a tool joins or leaves on a model that cannot append tools while the cache is warm, skills aside", () => {
    const wholeBefore = { ...before, toolLock: lock(HAIKU, { tools: ["read"], skillIds: ["s"] }) };
    const whole = toolLockState(wholeBefore, HAIKU, SENT);
    expect(lockTouch(whole, wholeBefore, { ...wholeBefore, enabledTools: ["read", "bash"] })).toBe(true);
    expect(lockTouch(whole, wholeBefore, { ...wholeBefore, enabledTools: [] })).toBe(true);
    expect(lockTouch(whole, wholeBefore, { ...wholeBefore, webSearchEnabled: true })).toBe(true);
    // Plan mode is the composer's switch, not a setting the lock covers.
    expect(lockTouch(whole, wholeBefore, { ...wholeBefore, planModeEnabled: true })).toBe(false);
    // A skill added later is a host notice on every model.
    expect(lockTouch(whole, wholeBefore, { ...wholeBefore, skillIds: ["s", "t"] })).toBe(false);

    // Cold, nothing warns, whichever way the surface moves.
    const cold = toolLockState(wholeBefore, HAIKU, SENT + 60 * MINUTE);
    expect(lockTouch(cold, wholeBefore, { ...wholeBefore, enabledTools: ["read", "bash"] })).toBe(false);
    expect(lockTouch(cold, wholeBefore, { ...wholeBefore, enabledTools: [] })).toBe(false);
  });

  it("says nothing once another model is selected", () => {
    const elsewhere = toolLockState(before, GPT, SENT);
    expect(lockTouch(elsewhere, before, { ...before, enabledTools: [], mcpIds: ["x"] })).toBe(false);
    // Not even about adding a tool where the last request's model could not take one.
    const wholeBefore = { ...before, toolLock: lock(HAIKU, { tools: ["read"] }) };
    const wholeElsewhere = toolLockState(wholeBefore, OPUS, SENT);
    expect(lockTouch(wholeElsewhere, wholeBefore, { ...wholeBefore, enabledTools: ["read", "bash"] })).toBe(false);
  });
});

describe("the web backends", () => {
  const TAVILY = { kind: "explicit", providerKind: "tavily" } as const;
  const EXA = { kind: "explicit", providerKind: "exa" } as const;
  const JINA = { kind: "explicit", providerKind: "jina" } as const;
  /* The last request searched with Tavily and fetched with Jina. */
  const hostRun = (model: ToolLockModel) => settings({
    webSearchEnabled: true,
    webSearch: { ...defaultConversationWebSearchSettings(), provider: TAVILY, fetchProvider: JINA },
    toolLock: lock(model, { webSearch: true, searchBackend: TAVILY, fetchBackend: JINA, webFetch: true })
  });

  it("draws a host-run backend orange while the cache is warm, and plain once it has moved", () => {
    const current = hostRun(OPUS);
    const warm = toolLockState(current, OPUS, SENT);
    expect(backendTone(warm, "search", TAVILY)).toBe("cache");
    expect(backendTone(warm, "fetch", JINA)).toBe("cache");
    expect(backendTone(warm, "search", EXA)).toBeNull();
    expect(backendTone(toolLockState(current, OPUS, SENT + 31 * MINUTE), "search", TAVILY)).toBeNull();
    // Another model's cache is not this one's.
    expect(backendTone(toolLockState(current, GPT, SENT), "search", TAVILY)).toBeNull();
  });

  it("leaves a leg plain on a model that appends tools when its tool did not go out, since giving it one is an addition", () => {
    const current = settings({
      toolLock: lock(OPUS, {
        webSearch: true,
        searchBackend: { kind: "disabled" },
        fetchBackend: { kind: "native" },
        webFetch: false
      })
    });
    const warm = toolLockState(current, OPUS, SENT);
    expect(backendTone(warm, "search", { kind: "disabled" })).toBeNull();
    expect(backendTone(warm, "fetch", { kind: "native" })).toBeNull();
  });

  it("never tones a native pin, for any model, warm or cold", () => {
    for (const sender of [OPUS, HAIKU]) {
      const pinned = settings({
        toolLock: lock(sender, { webSearch: true, searchBackend: { kind: "native" }, searchProvider: { kind: "native" } })
      });
      expect(backendPinned(toolLockOf(pinned), "search")).toBe(true);
      expect(backendPinned(toolLockOf(pinned), "fetch")).toBe(false);
      // Settled rather than locked: its selector says so in its own words.
      for (const model of [OPUS, HAIKU, GPT]) {
        for (const now of [SENT, SENT + 90 * MINUTE]) {
          expect(backendTone(toolLockState(pinned, model, now), "search", { kind: "native" })).toBeNull();
        }
      }
    }
  });

  /* On a model that cannot take a tool mid-conversation, giving a leg its tool
     folds it into the declared list, so the selector of a leg that sent
     nothing is orange too. A move between two choices that both offer no tool
     shows the model nothing new, and does not warn. */
  it("warns on a model that cannot append tools before a backend move adds a tool, but not between two choices that offer none", () => {
    const CHAT: ToolLockModel = { providerId: "openai", modelId: "gpt-chat", family: "openai_chat", appendsTools: false };
    /* Search was Off, and native fetch is no tool of its own on this family:
       the last request offered neither web tool. */
    const before = settings({
      webSearchEnabled: true,
      webSearch: { ...defaultConversationWebSearchSettings(), provider: { kind: "disabled" }, fetchProvider: { kind: "native" } },
      toolLock: lock(CHAT, { webSearch: true, searchBackend: { kind: "disabled" }, fetchBackend: { kind: "native" }, webFetch: false })
    });
    const warm = toolLockState(before, CHAT, SENT);
    expect(warm.wholeSurface).toBe(true);
    expect(backendTone(warm, "search", { kind: "disabled" })).toBe("cache");
    expect(backendTone(warm, "fetch", { kind: "native" })).toBe("cache");

    const moved = (patch: Partial<ConversationSettings["webSearch"]>) => ({
      ...before,
      webSearch: { ...before.webSearch, ...patch }
    });
    expect(lockTouch(warm, before, moved({ provider: EXA }))).toBe(true);
    expect(lockTouch(warm, before, moved({ provider: { kind: "native" } }))).toBe(true);
    expect(lockTouch(warm, before, moved({ fetchProvider: { kind: "explicit", providerKind: "firecrawl" } }))).toBe(true);
    // Off and native fetch both leave `web_fetch` out here.
    expect(lockTouch(warm, before, moved({ fetchProvider: { kind: "disabled" } }))).toBe(false);
    // Cold, nothing warns.
    expect(lockTouch(toolLockState(before, CHAT, SENT + 90 * MINUTE), before, moved({ provider: EXA }))).toBe(false);

    // Picked again while warm, both legs go back; cold, what was picked stays.
    const added = moved({ provider: EXA, fetchProvider: JINA });
    const restored = restoreLockedSettings(added, toolLockState(added, CHAT, SENT));
    expect(restored.webSearch.provider).toEqual({ kind: "disabled" });
    expect(restored.webSearch.fetchProvider).toEqual({ kind: "native" });
    expect(restoreLockedSettings(added, toolLockState(added, CHAT, SENT + 90 * MINUTE))).toBe(added);
  });

  it("warns before a warm host-run backend moves on any model, and never about a pinned native one", () => {
    for (const model of [OPUS, HAIKU]) {
      const before = hostRun(model);
      const warm = toolLockState(before, model, SENT);
      const moved = (patch: Partial<ConversationSettings["webSearch"]>) => ({
        ...before,
        webSearch: { ...before.webSearch, ...patch }
      });
      expect(lockTouch(warm, before, moved({ provider: EXA }))).toBe(true);
      expect(lockTouch(warm, before, moved({ fetchProvider: { kind: "disabled" } }))).toBe(true);
      // Settings beside the backends are not the lock's.
      expect(lockTouch(warm, before, moved({ maxResults: 3 }))).toBe(false);
      expect(lockTouch(toolLockState(before, model, SENT + 31 * MINUTE), before, moved({ provider: EXA }))).toBe(false);

      /* A pinned leg is settled, not locked: its selector does not move, and
         the warning has nothing to say about it. */
      const pinned = {
        ...before,
        webSearch: { ...before.webSearch, provider: { kind: "native" } as const },
        toolLock: lock(model, { webSearch: true, searchBackend: { kind: "native" }, searchProvider: { kind: "native" } })
      };
      const toExa = { ...pinned, webSearch: { ...pinned.webSearch, provider: EXA } };
      expect(lockTouch(toolLockState(pinned, model, SENT), pinned, toExa)).toBe(false);
      expect(lockTouch(toolLockState(pinned, GPT, SENT + 90 * MINUTE), pinned, toExa)).toBe(false);
    }
  });

  it("puts a warm backend back when its model is picked again, and on a model that cannot append tools even a leg that sent nothing", () => {
    const moved = {
      ...hostRun(OPUS),
      webSearch: { ...hostRun(OPUS).webSearch, provider: EXA, fetchProvider: { kind: "disabled" } as const }
    };
    const restored = restoreLockedSettings(moved, toolLockState(moved, OPUS, SENT));
    expect(restored.webSearch.provider).toEqual(TAVILY);
    expect(restored.webSearch.fetchProvider).toEqual(JINA);

    // A leg that sent nothing keeps what was picked since: that was free.
    const offLeg = settings({
      webSearchEnabled: true,
      webSearch: { ...defaultConversationWebSearchSettings(), provider: EXA },
      toolLock: lock(OPUS, { webSearch: true, searchBackend: { kind: "disabled" } })
    });
    expect(restoreLockedSettings(offLeg, toolLockState(offLeg, OPUS, SENT)).webSearch.provider).toEqual(EXA);
    // Where turning a leg on folds a tool into the declared list, even "off"
    // goes back while the cache is warm; cold, nothing does.
    const wholeOff = { ...offLeg, toolLock: lock(HAIKU, { webSearch: true, searchBackend: { kind: "disabled" } }) };
    expect(restoreLockedSettings(wholeOff, toolLockState(wholeOff, HAIKU, SENT)).webSearch.provider)
      .toEqual({ kind: "disabled" });
    expect(restoreLockedSettings(wholeOff, toolLockState(wholeOff, HAIKU, SENT + 90 * MINUTE))).toBe(wholeOff);

    // Backends already in place move nothing.
    const inPlace = hostRun(OPUS);
    expect(restoreLockedSettings(inPlace, toolLockState(inPlace, OPUS, SENT))).toBe(inPlace);
  });
});

describe("each model's own cache", () => {
  it("keeps every model's latest request, one entry per model", () => {
    const first = withRunToolLock(settings(), [], request(OPUS));
    const later = "2026-09-30T10:10:00.000Z";
    const second = withRunToolLock(first, [], { ...request(GPT), at: later });
    expect(toolLockOf(second).lastRequest?.modelId).toBe("gpt-5.5");
    expect(toolLockOf(second).modelRequests.map((item) => item.modelId)).toEqual(["claude-opus-5-5", "gpt-5.5"]);
    const third = withRunToolLock(second, [], { ...request(OPUS), at: later });
    expect(toolLockOf(third).modelRequests).toEqual([
      { providerId: "openai", modelId: "gpt-5.5", at: later },
      { providerId: "anthropic", modelId: "claude-opus-5-5", at: later }
    ]);
  });

  it("marks each model warm for its own lifetime, whichever sent last", () => {
    const current = withRunToolLock(
      withRunToolLock(settings(), [], request(OPUS)),
      [],
      { ...request(GPT), at: new Date(SENT + 20 * MINUTE).toISOString() }
    );
    // Opus sent first and is not selected, yet its cache still holds.
    expect(modelCacheWarmUntil(current, OPUS, SENT + 25 * MINUTE)).toBe(SENT + 30 * MINUTE);
    expect(modelCacheWarmUntil(current, OPUS, SENT + 30 * MINUTE)).toBeNull();
    expect(modelCacheWarmUntil(current, GPT, SENT + 45 * MINUTE)).toBe(SENT + 50 * MINUTE);
    expect(modelCacheWarmUntil(current, { ...GPT, cacheTtlMinutes: 5 }, SENT + 26 * MINUTE)).toBeNull();
    expect(modelCacheWarmUntil(current, HAIKU, SENT)).toBeNull();
  });

  it("reads a lock from before the list existed as knowing its last request", () => {
    const older = settings({ toolLock: { ...lock(OPUS), modelRequests: undefined as never } });
    expect(modelCacheWarmUntil(older, OPUS, SENT)).toBe(SENT + 30 * MINUTE);
  });

  it("counts from the moment a long run's last request went out, for that model too", () => {
    const current = withRunToolLock(withRunToolLock(settings(), [], request(GPT)), [], request(OPUS));
    const later = "2026-09-30T10:40:00.000Z";
    const refreshed = refreshedToolLock(current, later, false);
    expect(refreshed.lastRequest?.at).toBe(later);
    expect(refreshed.modelRequests.find((item) => item.modelId === "claude-opus-5-5")?.at).toBe(later);
    expect(refreshed.modelRequests.find((item) => item.modelId === "gpt-5.5")?.at).toBe(SENT_AT);
    expect(refreshedToolLock(settings(), later, false)).toEqual(EMPTY_TOOL_LOCK);
  });
});

describe("unticking a dangling selection", () => {
  it("leaves the conversation and the lock alike, so no restore puts it back", () => {
    const selected = settings({
      mcpIds: ["mcp_gone", "mcp_ok"],
      skillIds: ["skill_gone"],
      hookIds: ["hook_gone"],
      toolLock: lock(HAIKU, { mcpIds: ["mcp_gone", "mcp_ok"], skillIds: ["skill_gone"] })
    });
    const cleared = withoutDanglingSelection(selected, "mcp", "mcp_gone");
    expect(cleared.mcpIds).toEqual(["mcp_ok"]);
    expect(cleared.toolLock?.mcpIds).toEqual(["mcp_ok"]);
    const restored = restoreLockedSettings(cleared, toolLockState(cleared, HAIKU, SENT));
    expect(restored.mcpIds).toEqual(["mcp_ok"]);

    const skill = withoutDanglingSelection(selected, "skills", "skill_gone");
    expect(skill.skillIds).toEqual([]);
    expect(skill.toolLock?.skillIds).toEqual([]);
    expect(withoutDanglingSelection(selected, "hooks", "hook_gone").hookIds).toEqual([]);
    // A conversation that never ran has no lock to edit.
    expect(withoutDanglingSelection(settings({ mcpIds: ["mcp_gone"] }), "mcp", "mcp_gone").toolLock)
      .toBeUndefined();
  });
});

describe("hooks and the prompt profile", () => {
  const before = settings({
    hookIds: ["hook_a"],
    toolDescriptionFileId: "tooldesc_user_terse_01234567",
    toolLock: lock(OPUS, {
      hookIds: ["hook_a"],
      promptProfile: "tooldesc_user_terse_01234567"
    })
  });
  const warm = toolLockState(before, OPUS, SENT);

  it("records what the request's system prompt was built from", () => {
    const sent = withRunToolLock(before, [], request(OPUS));
    expect(toolLockOf(sent).hookIds).toEqual(["hook_a"]);
    expect(toolLockOf(sent).promptProfile).toBe("tooldesc_user_terse_01234567");
    // No selection is the built-in profile.
    const builtin = withRunToolLock(settings({ toolDescriptionFileId: null }), [], request(OPUS));
    expect(toolLockOf(builtin).promptProfile).toBe(BUILTIN_PROMPT_PROFILE_ID);
  });

  it("warns before any of them moves while the cache is warm, either way", () => {
    expect(lockTouch(warm, before, { ...before, hookIds: [] })).toBe(true);
    expect(lockTouch(warm, before, { ...before, hookIds: ["hook_a", "hook_b"] })).toBe(true);
    expect(lockTouch(warm, before, { ...before, toolDescriptionFileId: null })).toBe(true);
    expect(promptProfileTone(warm, before)).toBe("cache");
    expect(lockTone(warm, "hook", true, true)).toBe("cache");
    // Moved away, the cache is already lost for it: plain again.
    expect(promptProfileTone(warm, { ...before, toolDescriptionFileId: null })).toBeNull();
    // Cold, nothing warns.
    const cold = toolLockState(before, OPUS, SENT + 31 * MINUTE);
    expect(lockTouch(cold, before, { ...before, hookIds: [] })).toBe(false);
  });

  it("warns before they move on a model that cannot append tools just the same, and only while the cache is warm", () => {
    const wholeBefore = {
      ...before,
      toolLock: lock(HAIKU, { hookIds: ["hook_a"], promptProfile: "tooldesc_user_terse_01234567" })
    };
    const whole = toolLockState(wholeBefore, HAIKU, SENT);
    expect(lockTouch(whole, wholeBefore, { ...wholeBefore, hookIds: [] })).toBe(true);
    expect(lockTouch(whole, wholeBefore, { ...wholeBefore, hookIds: ["hook_a", "hook_b"] })).toBe(true);
    expect(lockTouch(whole, wholeBefore, { ...wholeBefore, toolDescriptionFileId: null })).toBe(true);
    expect(promptProfileTone(whole, wholeBefore)).toBe("cache");
    const cold = toolLockState(wholeBefore, HAIKU, SENT + 60 * MINUTE);
    expect(lockTouch(cold, wholeBefore, { ...wholeBefore, hookIds: [] })).toBe(false);
    expect(promptProfileTone(cold, wholeBefore)).toBeNull();
  });

  it("tones nothing a lock from before they were recorded never saw", () => {
    const older = settings({ hookIds: ["hook_a"], toolLock: lock(OPUS) });
    const state = toolLockState(older, OPUS, SENT);
    expect(lockTouch(state, older, { ...older, hookIds: [], toolDescriptionFileId: "x" })).toBe(false);
    expect(restoreLockedSettings(older, state)).toBe(older);
  });

  it("puts them back when the warm model is picked again, whether or not it appends tools", () => {
    const moved = { ...before, hookIds: ["hook_b"], toolDescriptionFileId: null };
    const restored = restoreLockedSettings(moved, toolLockState(moved, OPUS, SENT));
    expect(restored.hookIds).toEqual(["hook_a"]);
    expect(restored.toolDescriptionFileId).toBe("tooldesc_user_terse_01234567");

    const wholeMoved = {
      ...moved,
      toolLock: lock(HAIKU, { hookIds: ["hook_a"], promptProfile: "tooldesc_user_terse_01234567" })
    };
    const wholeRestored = restoreLockedSettings(wholeMoved, toolLockState(wholeMoved, HAIKU, SENT));
    expect(wholeRestored.hookIds).toEqual(["hook_a"]);
    expect(wholeRestored.toolDescriptionFileId).toBe("tooldesc_user_terse_01234567");
  });
});

describe("the host-message container", () => {
  const before = settings({
    hostMessageContainer: "box",
    toolLock: lock(OPUS, { hostMessageContainer: "box" })
  });
  const warm = toolLockState(before, OPUS, SENT);

  it("records the container each request projected its host messages in", () => {
    expect(toolLockOf(withRunToolLock(before, ["box"], request(OPUS))).hostMessageContainer).toBe("box");
    // Absent is the host's default: user messages.
    expect(toolLockOf(withRunToolLock(settings(), [], request(OPUS))).hostMessageContainer).toBe("user");
  });

  it("warns before it moves while the cache is warm, either way", () => {
    expect(hostMessageContainerTone(warm, before)).toBe("cache");
    expect(lockTouch(warm, before, { ...before, hostMessageContainer: "user" })).toBe(true);
    // Moved away, the cache is already lost for it: plain again.
    expect(hostMessageContainerTone(warm, { ...before, hostMessageContainer: "user" })).toBeNull();
    const cold = toolLockState(before, OPUS, SENT + 31 * MINUTE);
    expect(hostMessageContainerTone(cold, before)).toBeNull();
  });

  it("warns on a model that cannot append tools while the cache is warm, and holds nothing once it is cold", () => {
    const wholeBefore = settings({ toolLock: lock(HAIKU, { hostMessageContainer: "user" }) });
    const whole = toolLockState(wholeBefore, HAIKU, SENT);
    expect(hostMessageContainerTone(whole, wholeBefore)).toBe("cache");
    expect(lockTouch(whole, wholeBefore, { ...wholeBefore, hostMessageContainer: "box" })).toBe(true);
    // Put back when the warm model is picked again: `box` comes and goes with it.
    const moved = { ...wholeBefore, hostMessageContainer: "box" as const };
    expect(restoreLockedSettings(moved, toolLockState(moved, HAIKU, SENT)).hostMessageContainer).toBe("user");

    const cold = toolLockState(wholeBefore, HAIKU, SENT + 60 * MINUTE);
    expect(hostMessageContainerTone(cold, wholeBefore)).toBeNull();
    expect(lockTouch(cold, wholeBefore, { ...wholeBefore, hostMessageContainer: "box" })).toBe(false);
    expect(restoreLockedSettings(moved, toolLockState(moved, HAIKU, SENT + 60 * MINUTE))).toBe(moved);
  });

  it("puts it back when the warm model is picked again, and tones nothing an older lock never saw", () => {
    const moved = { ...before, hostMessageContainer: "user" as const };
    expect(restoreLockedSettings(moved, toolLockState(moved, OPUS, SENT)).hostMessageContainer).toBe("box");
    const older = settings({ hostMessageContainer: "box", toolLock: lock(OPUS) });
    const state = toolLockState(older, OPUS, SENT);
    expect(hostMessageContainerTone(state, older)).toBeNull();
    expect(lockTouch(state, older, { ...older, hostMessageContainer: "user" })).toBe(false);
    expect(restoreLockedSettings(older, state)).toBe(older);
  });
});

describe("the file write guards", () => {
  const before = settings({
    fileWriteGuardsEnabled: false,
    toolLock: lock(OPUS, { fileWriteGuards: false })
  });
  const warm = toolLockState(before, OPUS, SENT);

  it("records whether each request ran with the guards, absent meaning on", () => {
    expect(toolLockOf(withRunToolLock(before, [], request(OPUS))).fileWriteGuards).toBe(false);
    expect(toolLockOf(withRunToolLock(settings(), [], request(OPUS))).fileWriteGuards).toBe(true);
    expect(toolLockOf(withRunToolLock(settings({ fileWriteGuardsEnabled: true }), [], request(OPUS))).fileWriteGuards)
      .toBe(true);
    // A lock that holds the same answer is not a change to write.
    const sent = withRunToolLock(before, [], request(OPUS));
    expect(withRunToolLock(sent, [], request(OPUS))).toBe(sent);
    // The switch moving is: the next request leaves the other answer behind.
    const flipped = withRunToolLock({ ...sent, fileWriteGuardsEnabled: true }, [], request(OPUS));
    expect(flipped).not.toBe(sent);
    expect(toolLockOf(flipped).fileWriteGuards).toBe(true);
  });

  it("reads a lock written before the switch as knowing nothing", () => {
    expect(EMPTY_TOOL_LOCK.fileWriteGuards).toBeNull();
    const { fileWriteGuards: _gone, ...legacy } = lock(OPUS);
    expect(toolLockOf(settings({ toolLock: legacy as ConversationToolLock })).fileWriteGuards).toBeNull();
    expect(toolLockOf(settings({ toolLock: lock(OPUS, { fileWriteGuards: true }) })).fileWriteGuards).toBe(true);
  });

  it("warns before it moves while the cache is warm, either way", () => {
    expect(fileWriteGuardsTone(warm, before)).toBe("cache");
    expect(lockTouch(warm, before, { ...before, fileWriteGuardsEnabled: true })).toBe(true);
    // The other way too: guards on at the last request, switched off now.
    const on = settings({ toolLock: lock(OPUS, { fileWriteGuards: true }) });
    const onWarm = toolLockState(on, OPUS, SENT);
    expect(fileWriteGuardsTone(onWarm, on)).toBe("cache");
    expect(lockTouch(onWarm, on, { ...on, fileWriteGuardsEnabled: false })).toBe(true);
    // Writing the value it already has is no move.
    expect(lockTouch(onWarm, on, { ...on, fileWriteGuardsEnabled: true })).toBe(false);
    // Moved away, the cache is already lost for it: plain again.
    expect(fileWriteGuardsTone(warm, { ...before, fileWriteGuardsEnabled: true })).toBeNull();
    const cold = toolLockState(before, OPUS, SENT + 31 * MINUTE);
    expect(fileWriteGuardsTone(cold, before)).toBeNull();
    expect(lockTouch(cold, before, { ...before, fileWriteGuardsEnabled: true })).toBe(false);
  });

  it("warns on a model that cannot append tools while the cache is warm, and holds nothing once it is cold", () => {
    const wholeBefore = settings({ toolLock: lock(HAIKU, { fileWriteGuards: true }) });
    const whole = toolLockState(wholeBefore, HAIKU, SENT);
    expect(fileWriteGuardsTone(whole, wholeBefore)).toBe("cache");
    expect(lockTouch(whole, wholeBefore, { ...wholeBefore, fileWriteGuardsEnabled: false })).toBe(true);
    const moved = { ...wholeBefore, fileWriteGuardsEnabled: false };
    expect(restoreLockedSettings(moved, toolLockState(moved, HAIKU, SENT)).fileWriteGuardsEnabled).toBe(true);

    const cold = toolLockState(wholeBefore, HAIKU, SENT + 60 * MINUTE);
    expect(fileWriteGuardsTone(cold, wholeBefore)).toBeNull();
    expect(lockTouch(cold, wholeBefore, { ...wholeBefore, fileWriteGuardsEnabled: false })).toBe(false);
    expect(restoreLockedSettings(moved, toolLockState(moved, HAIKU, SENT + 60 * MINUTE))).toBe(moved);
  });

  it("puts it back when the warm model is picked again, and tones nothing an older lock never saw", () => {
    const moved = { ...before, fileWriteGuardsEnabled: true };
    expect(restoreLockedSettings(moved, toolLockState(moved, OPUS, SENT)).fileWriteGuardsEnabled).toBe(false);
    // Absent is on, so a lock holding on puts back nothing against an absent field.
    const absent = settings({ toolLock: lock(OPUS, { fileWriteGuards: true }) });
    expect(restoreLockedSettings(absent, toolLockState(absent, OPUS, SENT))).toBe(absent);

    const older = settings({ fileWriteGuardsEnabled: false, toolLock: lock(OPUS) });
    const state = toolLockState(older, OPUS, SENT);
    expect(fileWriteGuardsTone(state, older)).toBeNull();
    expect(lockTouch(state, older, { ...older, fileWriteGuardsEnabled: true })).toBe(false);
    expect(restoreLockedSettings(older, state)).toBe(older);
  });
});

describe("the plan-mode switch", () => {
  /* Its pair is sticky and its guidance is appended, so it touches the cache
     only when switched on before the pair has ever gone out, on a model that
     folds the two tools into the declared list. */
  it("is orange only on a model that cannot append tools, before the pair went out, while the cache is warm", () => {
    const never = settings({ toolLock: lock(HAIKU) });
    expect(planModeTone(toolLockState(never, HAIKU, SENT), false)).toBe("cache");
    // Already switched on: moved away, so the cache is already lost for it.
    expect(planModeTone(toolLockState(never, HAIKU, SENT), true)).toBeNull();
    // Cold, another model, or a model that appends tools: nothing to protect.
    expect(planModeTone(toolLockState(never, HAIKU, SENT + 90 * MINUTE), false)).toBeNull();
    expect(planModeTone(toolLockState(never, OPUS, SENT), false)).toBeNull();
    const appended = settings({ toolLock: lock(OPUS) });
    expect(planModeTone(toolLockState(appended, OPUS, SENT), false)).toBeNull();
    // Once the pair has gone out it stays out, so switching moves no tool.
    const offered = settings({ toolLock: lock(HAIKU, { planMode: true }) });
    expect(planModeTone(toolLockState(offered, HAIKU, SENT), false)).toBeNull();
  });
});

describe("a pinned backend under restore", () => {
  it("stays on its pin when the warm model is picked again, whatever the last request recorded", () => {
    const EXA = { kind: "explicit", providerKind: "exa" } as const;
    for (const model of [OPUS, HAIKU]) {
      const current = settings({
        webSearchEnabled: true,
        webSearch: { ...defaultConversationWebSearchSettings(), provider: { kind: "native" } },
        toolLock: lock(model, { webSearch: true, searchBackend: EXA, searchProvider: { kind: "native" } })
      });
      const restored = restoreLockedSettings(current, toolLockState(current, model, SENT));
      expect(restored.webSearch.provider).toEqual({ kind: "native" });
    }
  });
});
