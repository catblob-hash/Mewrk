import { describe, expect, it } from "vitest";
import { estimateContextsTokens, estimateTokens, estimateWireTokens, liveContextTokens, wireView } from "./contextTokens";
import type { ModelRunState } from "./modelStream";
import type { ContextItem, ModelRunRequest, NativeCompaction } from "../types";

function runFixture(overrides: Partial<ModelRunState> = {}): ModelRunState {
  return {
    requestId: "run_test",
    providerName: "provider",
    modelName: "model",
    workspaceId: "ws",
    request: {
      contexts: [],
      provider: { family: "openai_responses" },
      model: { capabilities: [] }
    } as unknown as ModelRunRequest,
    startedAt: "2026-08-26T00:00:00.000Z",
    streamedTextByRound: {},
    streamedReasoningByRound: {},
    completedReasoningByRound: {},
    reasoningStartedAtByRound: {},
    reasoningDurationByRound: {},
    streamedToolsByRound: {},
    streamedHooksByRound: {},
    steeredInputsByRound: {},
    usageByRound: {},
    subagentUsageByCall: {},
    workflowProgressByCall: {},
    workflowRunIdByCall: {},
    usageRevision: 0,
    ...overrides
  };
}

/** 40 ASCII characters, which the shared estimator prices at exactly 10. */
const FORTY = "a".repeat(40);

describe("liveContextTokens", () => {
  it("leans on the caller's estimate alone until the first provider snapshot", () => {
    // The host writes streamed prose back as throttled `streaming` rows, so the
    // caller's estimate of the conversation already covers what has streamed and
    // already climbs. Adding the run's own buffers on top would bill the same
    // tokens twice.
    const usage = liveContextTokens(
      runFixture({ streamedTextByRound: { 0: FORTY } }),
      1_000
    );
    expect(usage).toEqual({ tokens: 1_000, estimated: true });
  });

  it("anchors on the round's reported input and adds what has streamed since", () => {
    // Anthropic reports input at `message_start`, before a single output token
    // exists. That is the exact context the provider read, so it is the anchor;
    // the round's own output is the part that is still growing.
    const usage = liveContextTokens(
      runFixture({
        usageByRound: { 2: { inputTokens: 5_000, cachedInputTokens: 4_000 } },
        streamedTextByRound: { 2: FORTY }
      }),
      1
    );
    expect(usage).toEqual({ tokens: 5_010, estimated: true });
  });

  it("stops estimating a round the provider has finished reporting", () => {
    // Once output tokens land, the round is accounted for in full and nothing
    // is left to estimate — so the number is authoritative, not `~`.
    const usage = liveContextTokens(
      runFixture({
        usageByRound: { 2: { inputTokens: 5_000, outputTokens: 300 } },
        streamedTextByRound: { 2: FORTY }
      }),
      1
    );
    expect(usage).toEqual({ tokens: 5_300, estimated: false });
  });

  it("counts a later round's stream on top of the last closed round", () => {
    const usage = liveContextTokens(
      runFixture({
        usageByRound: { 2: { inputTokens: 5_000, outputTokens: 300 } },
        streamedTextByRound: { 2: "已计入的旧内容", 3: FORTY }
      }),
      1
    );
    expect(usage).toEqual({ tokens: 5_310, estimated: true });
  });

  it("prices a round's tool calls the same way the settled context will", () => {
    const tool = {
      id: "t1",
      callId: "call-1",
      toolName: "read",
      input: { path: "a.ts" },
      result: { success: true, output: FORTY, images: [], executedAt: "", durationMs: 0 },
      streamStatus: "completed" as const,
      live: { contexts: [], updates: [] },
      createdAt: ""
    };
    const usage = liveContextTokens(
      runFixture({
        usageByRound: { 0: { inputTokens: 4_000 } },
        streamedToolsByRound: { 0: [tool] }
      }),
      0
    );
    expect(usage).toEqual({
      tokens: 4_000 + estimateTokens(`read\n{"path":"a.ts"}\n${FORTY}`),
      estimated: true
    });
  });

  it("counts what a reported round's calls returned before the next round reports", () => {
    // The calls are in the round's output; their results reach the provider
    // only with the next request, so until its snapshot they are estimated
    // rather than left out.
    const tool = {
      id: "t1",
      callId: "call-1",
      toolName: "read",
      input: { path: "a.ts" },
      result: { success: true, output: FORTY, images: [], executedAt: "", durationMs: 0 },
      streamStatus: "completed" as const,
      live: { contexts: [], updates: [] },
      createdAt: ""
    };
    const usage = liveContextTokens(
      runFixture({
        usageByRound: { 1: { inputTokens: 5_000, outputTokens: 300 } },
        streamedToolsByRound: { 1: [tool] }
      }),
      1
    );
    expect(usage).toEqual({ tokens: 5_310, estimated: true });
  });

  it("counts a message steered in after the snapshot, not one it already read", () => {
    const steered = (id: string) => ({
      kind: "user" as const,
      id,
      content: FORTY,
      createdAt: ""
    });
    const usage = liveContextTokens(
      runFixture({
        usageByRound: { 2: { inputTokens: 5_000, outputTokens: 300 } },
        steeredInputsByRound: { 2: [steered("read")], 3: [steered("new")] }
      }),
      1
    );
    expect(usage).toEqual({ tokens: 5_310, estimated: true });
  });

  it("counts what the host delivered after the snapshot, not what it already read", () => {
    const delivered = (id: string) => ({
      context: {
        kind: "user" as const,
        id,
        content: FORTY,
        createdAt: ""
      },
      afterSteered: 0
    });
    const usage = liveContextTokens(
      runFixture({
        usageByRound: { 2: { inputTokens: 5_000, outputTokens: 300 } },
        hostContextsByRound: { 2: [delivered("read")], 3: [delivered("new")] }
      }),
      1
    );
    expect(usage).toEqual({ tokens: 5_310, estimated: true });
  });

  it("declines to anchor on a snapshot with no input count", () => {
    // A snapshot that reports only output says nothing about how large the
    // context is; anchoring on it would claim the conversation had shrunk to
    // the size of one response.
    const usage = liveContextTokens(
      runFixture({
        usageByRound: { 0: { outputTokens: 300 } },
        streamedTextByRound: { 0: FORTY }
      }),
      2_000
    );
    expect(usage).toEqual({ tokens: 2_000, estimated: true });
  });
});

describe("estimateContextsTokens", () => {
  it("leaves out what is never sent: a cut-off fragment and a host-local record", () => {
    const sent = { id: "a", kind: "assistant" as const, content: "x".repeat(400), createdAt: "2026-10-03T00:00:00Z" };
    const total = estimateContextsTokens([sent]);
    expect(total).toBeGreaterThan(0);
    expect(estimateContextsTokens([
      sent,
      { ...sent, id: "cut", interrupted: true },
      { id: "r", kind: "reasoning" as const, content: "y".repeat(400), interrupted: true, createdAt: "2026-10-03T00:00:00Z" },
      { id: "s", kind: "system" as const, content: "z".repeat(400), localOnly: true, createdAt: "2026-10-03T00:00:00Z" }
    ])).toBe(total);
  });
});

describe("wire view", () => {
  const compaction = (providerId: string): NativeCompaction => ({
    providerId,
    model: "gpt-6-astra",
    parts: [],
    retained: [{ role: "user", sourceId: "u1", content: "kept" }],
    tokensBefore: 200_000,
    tokensAfter: 3_000
  });
  const card = (id: string, providerId: string): ContextItem => ({
    id,
    kind: "system",
    content: "Context compacted natively.",
    localOnly: true,
    nativeCompaction: compaction(providerId),
    createdAt: "2026-10-05T00:00:00Z"
  });
  const user = (id: string, content: string): ContextItem => ({
    id,
    kind: "user",
    content,
    createdAt: "2026-10-05T00:00:00Z"
  });
  const timeline = [
    user("u1", "a".repeat(4000)),
    card("c1", "codex"),
    user("u2", "b".repeat(40)),
    card("c2", "other"),
    user("u3", "c".repeat(40))
  ];
  const codex = { id: "codex", family: "openai_codex" as const };

  it("starts at the latest compaction its provider made, on a model that compacts natively", () => {
    const view = wireView(timeline, { provider: codex, model: { capabilities: ["native_compaction"] } });
    expect(view.contexts.map((item) => item.id)).toEqual(["c1", "u2", "c2", "u3"]);
    expect(view.compaction?.providerId).toBe("codex");
    // The card weighs what the host weighed it; the other provider's card the
    // message it kept ("kept", one token).
    expect(estimateWireTokens(view)).toBe(3_000 + 10 + 1 + 10);
  });

  it("is the whole timeline anywhere else", () => {
    for (const target of [
      null,
      { provider: codex, model: { capabilities: [] } },
      { provider: { id: "codex", family: "anthropic" as const }, model: { capabilities: ["native_compaction" as const] } },
      { provider: { id: "third", family: "openai_codex" as const }, model: { capabilities: ["native_compaction" as const] } }
    ]) {
      const view = wireView(timeline, target);
      expect(view.contexts).toBe(timeline);
      expect(view.compaction).toBeNull();
      expect(estimateWireTokens(view)).toBe(estimateContextsTokens(timeline));
    }
  });
});
