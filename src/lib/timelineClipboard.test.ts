import { describe, expect, it } from "vitest";
import type {
  AssistantContext,
  ContextItem,
  ImageAttachment,
  ReasoningContext,
  SubagentRunRecord,
  SystemContext,
  ToolContext,
  ToolDescriptor,
  UserContext
} from "../types";
import {
  copiesForPaste,
  timelineClipboardText,
  timelineCopyable,
  timelineMovable,
  withCopiedToolProofs
} from "./timelineClipboard";

const AT = "2026-10-02T00:00:00.000Z";

const user = (id: string, extra: Partial<UserContext> = {}): UserContext => ({
  id,
  kind: "user",
  content: id,
  createdAt: AT,
  ...extra
});

const assistant = (id: string, extra: Partial<AssistantContext> = {}): AssistantContext => ({
  id,
  kind: "assistant",
  content: id,
  createdAt: AT,
  ...extra
});

const system = (id: string, extra: Partial<SystemContext> = {}): SystemContext => ({
  id,
  kind: "system",
  content: id,
  createdAt: AT,
  ...extra
});

const reasoning = (id: string, extra: Partial<ReasoningContext> = {}): ReasoningContext => ({
  id,
  kind: "reasoning",
  content: id,
  createdAt: AT,
  ...extra
});

const tool = (id: string, extra: Partial<ToolContext> = {}): ToolContext => ({
  id,
  kind: "tool",
  toolName: "read",
  input: { path: "a.txt" },
  result: { success: true, output: "ok", executedAt: AT, durationMs: 1 },
  createdAt: AT,
  ...extra
});

const image = (id: string, shortId?: number): ImageAttachment => ({
  id,
  name: `${id}.png`,
  mime: "image/png",
  width: 4,
  height: 4,
  bytes: 16,
  ...(shortId === undefined ? {} : { shortId })
});

const descriptor = (name: string, category: ToolDescriptor["category"]): ToolDescriptor => ({
  name,
  label: name,
  description: "",
  category,
  dangerous: false,
  parameters: []
});

const nativeCompaction = { providerId: "p", model: "m", parts: [], tokensBefore: 10, tokensAfter: 2 };
const hookExecution = {
  executionId: "exec",
  hookId: "hook",
  hookName: "Hook",
  event: "PreToolUse",
  status: "succeeded",
  contextInjected: true
} as const;
const subagent: SubagentRunRecord = { task: "look around", status: "completed", contexts: [], updates: [] };

/** Ids numbered per prefix, so the numbers do not depend on the order the copies ask in. */
const counter = () => {
  const counts = { ctx: 0, turn: 0 };
  return (prefix: "ctx" | "turn") => `${prefix}_${++counts[prefix]}`;
};

const idsOf = (contexts: readonly ContextItem[]) => contexts.map((context) => context.id);

describe("timelineCopyable", () => {
  it("copies plain user, assistant, system, reasoning and tool contexts", () => {
    for (const context of [user("u"), assistant("a"), system("s"), reasoning("r"), tool("t")]) {
      expect(timelineCopyable(context, [])).toBe(true);
    }
  });

  it("copies a context that is no longer streaming", () => {
    expect(timelineCopyable(assistant("a", { streaming: false }), [])).toBe(true);
    expect(timelineCopyable(tool("t", { streaming: false }), [])).toBe(true);
  });

  it("does not copy a context that is still streaming", () => {
    expect(timelineCopyable(assistant("a", { streaming: true }), [])).toBe(false);
    expect(timelineCopyable(reasoning("r", { streaming: true }), [])).toBe(false);
    expect(timelineCopyable(tool("t", { streaming: true }), [])).toBe(false);
  });

  it.each([
    "ctx_agent-result_1",
    "ctx_agent-message_1",
    "ctx_structured-output-nudge_1",
    "ctx_system-append_1",
    "ctx_handoff_index",
    "ctx_skill_review",
    "ctx_hook-continuation_1"
  ])("does not copy a card the host minted under %s", (id) => {
    expect(timelineCopyable(user(id), [])).toBe(false);
    expect(timelineCopyable(system(id), [])).toBe(false);
    expect(timelineCopyable(tool(id), [])).toBe(false);
  });

  it("does not copy a system context that is a compaction, a tools notice or a hook record", () => {
    expect(timelineCopyable(system("s", { nativeCompaction }), [])).toBe(false);
    expect(timelineCopyable(system("s", { toolsAdded: ["read"] }), [])).toBe(false);
    expect(timelineCopyable(system("s", { hookExecution }), [])).toBe(false);
  });

  it("does not copy a subagent call or a delivery notice", () => {
    expect(timelineCopyable(tool("t", { subagent }), [])).toBe(false);
    expect(timelineCopyable(tool("t", { notice: "box" }), [])).toBe(false);
  });

  it("does not copy a tool call its descriptor files under orchestration", () => {
    const tools = [descriptor("read", "filesystem"), descriptor("agent_spawn", "orchestration")];

    expect(timelineCopyable(tool("t", { toolName: "agent_spawn" }), tools)).toBe(false);
    expect(timelineCopyable(tool("t", { toolName: "read" }), tools)).toBe(true);
  });

  it("copies a tool call that has no descriptor at all", () => {
    const tools = [descriptor("agent_spawn", "orchestration")];

    expect(timelineCopyable(tool("t", { toolName: "removed_tool" }), tools)).toBe(true);
    expect(timelineCopyable(tool("t", { toolName: "removed_tool" }), [])).toBe(true);
  });
});

describe("timelineMovable", () => {
  it("refuses only a compaction", () => {
    expect(timelineMovable(system("s", { nativeCompaction }))).toBe(false);
  });

  it("moves everything else", () => {
    for (const context of [
      user("u"),
      assistant("a"),
      system("s"),
      system("s", { toolsAdded: ["read"] }),
      system("s", { hookExecution }),
      reasoning("r"),
      tool("t"),
      tool("t", { subagent })
    ]) {
      expect(timelineMovable(context)).toBe(true);
    }
  });
});

describe("timelineClipboardText", () => {
  it("joins the contents with a blank line", () => {
    expect(timelineClipboardText([user("first"), assistant("second"), system("third")])).toBe(
      "first\n\nsecond\n\nthird"
    );
  });

  it("trims each content", () => {
    expect(timelineClipboardText([user("u", { content: "  hello \n" }), assistant("a", { content: "\nworld  " })])).toBe(
      "hello\n\nworld"
    );
  });

  it("reads a tool as its name and input, then its output", () => {
    const call = tool("t", {
      toolName: "grep",
      input: { pattern: "foo", glob: "*.ts" },
      result: { success: true, output: "a.ts:1: foo", executedAt: AT, durationMs: 1 }
    });

    expect(timelineClipboardText([call])).toBe('grep {"pattern":"foo","glob":"*.ts"}\na.ts:1: foo');
    expect(timelineClipboardText([user("ask"), call])).toBe('ask\n\ngrep {"pattern":"foo","glob":"*.ts"}\na.ts:1: foo');
  });

  it("keeps a tool that printed nothing as its name and input alone", () => {
    const call = tool("t", { result: { success: true, output: "", executedAt: AT, durationMs: 1 } });

    expect(timelineClipboardText([call])).toBe('read {"path":"a.txt"}');
  });

  it("skips what has no content", () => {
    expect(
      timelineClipboardText([
        user("a"),
        user("blank", { content: "" }),
        system("spaces", { content: "   \n " }),
        reasoning("encrypted", { content: undefined }),
        assistant("b")
      ])
    ).toBe("a\n\nb");
    expect(timelineClipboardText([])).toBe("");
    expect(timelineClipboardText([user("blank", { content: "" })])).toBe("");
  });
});

describe("copiesForPaste", () => {
  const clipboard = (contexts: ContextItem[], conversationId = "conv_src") => ({ conversationId, contexts });

  it("gives every copy a new id, keeps the order and leaves the originals alone", () => {
    const contexts: ContextItem[] = [
      user("u"),
      assistant("a", { sources: [{ id: "s1", url: "https://example.com" }] }),
      system("s"),
      reasoning("r", { streaming: false }),
      tool("t", { providerCallId: "call_1", attestation: "proof" })
    ];
    const before = structuredClone(contexts);

    const copies = copiesForPaste(clipboard(contexts), "conv_dst", new Set(), counter());

    expect(copies.skipped).toBe(0);
    expect(idsOf(copies.contexts)).toEqual(["ctx_1", "ctx_2", "ctx_3", "ctx_4", "ctx_5"]);
    expect(copies.contexts.map((context) => context.kind)).toEqual(["user", "assistant", "system", "reasoning", "tool"]);
    expect(copies.contexts.slice(0, 4).map((context) => (context as UserContext).content)).toEqual(["u", "a", "s", "r"]);
    expect(copies.contexts.map((context) => context.createdAt)).toEqual(contexts.map((context) => context.createdAt));
    expect(contexts).toEqual(before);
    for (const [index, copy] of copies.contexts.entries()) expect(copy).not.toBe(contexts[index]);
  });

  it("pastes an empty clipboard as nothing", () => {
    expect(copiesForPaste(clipboard([]), "conv_dst", new Set(), counter())).toEqual({
      contexts: [],
      tools: [],
      skipped: 0
    });
  });

  describe("user messages with images", () => {
    it("numbers an image again when the destination already uses its number, and its placeholder with it", () => {
      const source = user("u", {
        content: "look at [Image #3] closely",
        images: [image("img", 3)]
      });
      const taken = new Set([3, 7]);

      const [copy] = copiesForPaste(clipboard([source]), "conv_dst", taken, counter()).contexts as UserContext[];

      expect(copy.images?.[0].shortId).toBe(8);
      expect(copy.images?.[0].id).toBe("img");
      expect(copy.content).toBe("look at [Image #8] closely");
      expect([...taken].sort((left, right) => left - right)).toEqual([3, 7, 8]);
      // The original keeps its own number.
      expect(source.images?.[0].shortId).toBe(3);
      expect(source.content).toBe("look at [Image #3] closely");
    });

    it("keeps the number when the destination does not use it", () => {
      const source = user("u", { content: "look at [Image #3]", images: [image("img", 3)] });
      const taken = new Set([1, 2]);

      const [copy] = copiesForPaste(clipboard([source]), "conv_dst", taken, counter()).contexts as UserContext[];

      expect(copy.images?.[0].shortId).toBe(3);
      expect(copy.content).toBe("look at [Image #3]");
      expect([...taken].sort((left, right) => left - right)).toEqual([1, 2, 3]);
    });

    it("renumbers only the images that clash and rewrites only their placeholders", () => {
      const source = user("u", {
        content: "[Image #1] versus [Image #2]",
        images: [image("one", 1), image("two", 2)]
      });
      const taken = new Set([1]);

      const [copy] = copiesForPaste(clipboard([source]), "conv_dst", taken, counter()).contexts as UserContext[];

      expect(copy.images?.map((entry) => entry.shortId)).toEqual([3, 2]);
      expect(copy.content).toBe("[Image #3] versus [Image #2]");
      expect([...taken].sort((left, right) => left - right)).toEqual([1, 2, 3]);
    });

    it("hands out numbers across the whole paste, so two copies never share one", () => {
      const contexts = [
        user("u1", { content: "[Image #1]", images: [image("one", 1)] }),
        user("u2", { content: "[Image #1]", images: [image("two", 1)] })
      ];

      const copies = copiesForPaste(clipboard(contexts), "conv_dst", new Set(), counter()).contexts as UserContext[];

      expect(copies.map((copy) => copy.images?.[0].shortId)).toEqual([1, 2]);
      expect(copies.map((copy) => copy.content)).toEqual(["[Image #1]", "[Image #2]"]);
    });

    it("leaves a user message without images as it is", () => {
      const source = user("u", { content: "plain [Image #4] text" });
      const taken = new Set([4]);

      const [copy] = copiesForPaste(clipboard([source]), "conv_dst", taken, counter()).contexts as UserContext[];

      expect(copy.content).toBe("plain [Image #4] text");
      expect(copy.images).toBeUndefined();
      expect([...taken]).toEqual([4]);
    });
  });

  describe("model turns", () => {
    it("drops an assistant's cited sources and gives it a turn id of its own", () => {
      const source = assistant("a", {
        round: 2,
        modelTurnId: "turn-old",
        sources: [{ id: "s1", url: "https://example.com", title: "Example" }]
      });

      const [copy] = copiesForPaste(clipboard([source]), "conv_dst", new Set(), counter()).contexts as AssistantContext[];

      expect(copy).not.toHaveProperty("sources");
      expect(copy.modelTurnId).toBe("turn_1");
      expect(copy.round).toBe(2);
      expect(copy.content).toBe("a");
      expect(source.sources).toHaveLength(1);
      expect(source.modelTurnId).toBe("turn-old");
    });

    it("keeps what one round emitted together under one new turn id, and other rounds apart", () => {
      const contexts: ContextItem[] = [
        reasoning("r1", { modelTurnId: "turn-a" }),
        assistant("a1", { modelTurnId: "turn-a" }),
        tool("t1", { modelTurnId: "turn-a" }),
        reasoning("r2", { modelTurnId: "turn-b" }),
        tool("t2", { modelTurnId: "turn-b" }),
        assistant("a2", { modelTurnId: "turn-c" })
      ];

      const copies = copiesForPaste(clipboard(contexts), "conv_dst", new Set(), counter()).contexts as Array<
        ReasoningContext | AssistantContext | ToolContext
      >;
      const turns = copies.map((copy) => copy.modelTurnId);

      expect(turns[0]).toBeDefined();
      expect(turns[1]).toBe(turns[0]);
      expect(turns[2]).toBe(turns[0]);
      expect(turns[4]).toBe(turns[3]);
      expect(turns[3]).not.toBe(turns[0]);
      expect(turns[5]).not.toBe(turns[0]);
      expect(turns[5]).not.toBe(turns[3]);
      expect(new Set(turns).size).toBe(3);
      for (const turn of turns) expect(["turn-a", "turn-b", "turn-c"]).not.toContain(turn);
    });

    it("gives a context without a turn id none, rather than an undefined one", () => {
      const contexts = [assistant("a"), reasoning("r"), tool("t")];
      const newId = counter();

      const copies = copiesForPaste(clipboard(contexts), "conv_dst", new Set(), newId).contexts;

      for (const copy of copies) expect(copy).not.toHaveProperty("modelTurnId");
      expect(newId("turn")).toBe("turn_1");
    });

    it("starts the turn ids over on every paste", () => {
      const contexts = [assistant("a", { modelTurnId: "turn-a" })];
      const newId = counter();

      const first = copiesForPaste(clipboard(contexts), "conv_dst", new Set(), newId).contexts as AssistantContext[];
      const second = copiesForPaste(clipboard(contexts), "conv_dst", new Set(), newId).contexts as AssistantContext[];

      expect(first[0].modelTurnId).toBe("turn_1");
      expect(second[0].modelTurnId).toBe("turn_2");
    });
  });

  describe("reasoning", () => {
    it("drops the streaming flag and carries the rest over", () => {
      const replay = { model: "gpt", parts: [{ type: "reasoning", id: "rs_1" }] };
      for (const streaming of [true, false]) {
        const source = reasoning("r", {
          content: "weighing it",
          form: "plaintext",
          replay,
          round: 3,
          streaming,
          durationMs: 1200,
          tokens: 40
        });

        const [copy] = copiesForPaste(clipboard([source]), "conv_dst", new Set(), counter()).contexts as ReasoningContext[];

        expect(copy).not.toHaveProperty("streaming");
        expect(copy).toMatchObject({
          id: "ctx_1",
          kind: "reasoning",
          content: "weighing it",
          form: "plaintext",
          round: 3,
          durationMs: 1200,
          tokens: 40,
          createdAt: AT
        });
        expect(copy.replay).toEqual(replay);
      }
    });

    it("carries an encrypted trace without text over too", () => {
      const source = reasoning("r", {
        content: undefined,
        form: "encrypted",
        replay: { model: "gpt", parts: [{ type: "reasoning", encrypted_content: "xxx" }] },
        round: 1
      });

      const [copy] = copiesForPaste(clipboard([source]), "conv_dst", new Set(), counter()).contexts as ReasoningContext[];

      expect(copy.form).toBe("encrypted");
      expect(copy.replay).toEqual(source.replay);
      expect(copy.round).toBe(1);
    });
  });

  describe("tool cards", () => {
    const attested = (id: string, extra: Partial<ToolContext> = {}) =>
      tool(id, {
        toolName: "grep",
        round: 4,
        input: { pattern: "foo" },
        requestedInput: { pattern: "fo+" },
        result: { success: true, output: "a.ts:1: foo", executedAt: AT, durationMs: 9 },
        providerCallId: "call_abc",
        attestation: "proof-of-source",
        streaming: false,
        streamStatus: "completed",
        live: { contexts: [], updates: [] },
        ...extra
      });

    it("leaves behind what tied the original to its place and carries the rest over", () => {
      const source = attested("t");

      const copies = copiesForPaste(clipboard([source]), "conv_dst", new Set(), counter());
      const [copy] = copies.contexts as ToolContext[];

      for (const key of ["providerCallId", "attestation", "streaming", "streamStatus", "live"]) {
        expect(copy).not.toHaveProperty(key);
      }
      expect(copy.id).toBe("ctx_1");
      expect(copy.kind).toBe("tool");
      expect(copy.toolName).toBe("grep");
      expect(copy.input).toEqual({ pattern: "foo" });
      expect(copy.requestedInput).toEqual({ pattern: "fo+" });
      expect(copy.result).toEqual(source.result);
      expect(copy.round).toBe(4);
      expect(copy.createdAt).toBe(AT);
      // The original is untouched.
      expect(source.attestation).toBe("proof-of-source");
      expect(source.providerCallId).toBe("call_abc");
    });

    it("lists each copied tool card with the card it copies, in order", () => {
      const first = attested("t1");
      const second = attested("t2", { toolName: "read" });
      const contexts: ContextItem[] = [user("u"), first, assistant("a"), second];

      const copies = copiesForPaste(clipboard(contexts), "conv_dst", new Set(), counter());

      expect(copies.tools).toHaveLength(2);
      expect(copies.tools[0]).toEqual({ contextId: copies.contexts[1].id, source: first });
      expect(copies.tools[0].source).toBe(first);
      expect(copies.tools[1]).toEqual({ contextId: copies.contexts[3].id, source: second });
      expect(copies.tools[1].source).toBe(second);
      expect(copies.tools.map((entry) => entry.contextId)).toEqual(["ctx_2", "ctx_4"]);
    });

    it("lists no tool cards when none were copied", () => {
      expect(copiesForPaste(clipboard([user("u"), assistant("a")]), "conv_dst", new Set(), counter()).tools).toEqual([]);
    });

    describe("with images in the result", () => {
      const withImages = (id: string, ...shortIds: Array<number | undefined>) =>
        attested(id, {
          result: {
            success: true,
            output: "screenshot",
            images: shortIds.map((shortId, index) => image(`${id}-${index}`, shortId)),
            executedAt: AT,
            durationMs: 3
          }
        });

      it("leaves the card out of another conversation that already uses one of its numbers", () => {
        const clash = withImages("t-clash", 5, 6);
        const other = attested("t-other");
        const taken = new Set([5]);

        const copies = copiesForPaste(
          clipboard([user("u"), clash, other]),
          "conv_dst",
          taken,
          counter()
        );

        expect(copies.skipped).toBe(1);
        expect(copies.contexts.map((context) => context.kind)).toEqual(["user", "tool"]);
        expect(copies.tools).toHaveLength(1);
        expect(copies.tools[0].source).toBe(other);
        expect(copies.tools.some((entry) => entry.source === clash)).toBe(false);
        // A card that was left out reserves nothing.
        expect([...taken]).toEqual([5]);
      });

      it("counts each card it leaves out", () => {
        const copies = copiesForPaste(
          clipboard([withImages("t1", 5), withImages("t2", 5), attested("t3")]),
          "conv_dst",
          new Set([5]),
          counter()
        );

        expect(copies.skipped).toBe(2);
        expect(copies.contexts).toHaveLength(1);
      });

      it("keeps the card in another conversation where its numbers are free, and reserves them", () => {
        const card = withImages("t", 5, 6, undefined);
        const taken = new Set([1, 2]);

        const copies = copiesForPaste(clipboard([card]), "conv_dst", taken, counter());
        const [copy] = copies.contexts as ToolContext[];

        expect(copies.skipped).toBe(0);
        expect(copies.tools).toHaveLength(1);
        expect(copy.result.images?.map((entry) => entry.shortId)).toEqual([5, 6, undefined]);
        expect([...taken].sort((left, right) => left - right)).toEqual([1, 2, 5, 6]);
      });

      it("keeps the card in the conversation it was copied from, where its numbers are the same images", () => {
        const card = withImages("t", 5, 6);
        const taken = new Set([5, 6, 9]);

        const copies = copiesForPaste(clipboard([card], "conv_src"), "conv_src", taken, counter());
        const [copy] = copies.contexts as ToolContext[];

        expect(copies.skipped).toBe(0);
        expect(copies.tools).toHaveLength(1);
        expect(copy.result.images?.map((entry) => entry.shortId)).toEqual([5, 6]);
        expect([...taken].sort((left, right) => left - right)).toEqual([5, 6, 9]);
      });

      it("never renumbers a tool card's images", () => {
        const card = withImages("t", 2);

        const [copy] = copiesForPaste(clipboard([card]), "conv_dst", new Set([1]), counter()).contexts as ToolContext[];

        expect(copy.result.images?.[0].shortId).toBe(2);
      });
    });
  });
});

describe("withCopiedToolProofs", () => {
  it("puts each proof on the tool copy it was issued for and leaves the rest as they are", () => {
    const contexts: ContextItem[] = [
      user("u"),
      tool("t1", { attestation: "old-1" }),
      assistant("a"),
      tool("t2", { toolName: "grep" }),
      reasoning("r")
    ];
    const copies = copiesForPaste({ conversationId: "conv_src", contexts }, "conv_dst", new Set(), counter());

    const proved = withCopiedToolProofs(copies, ["proof-1", "proof-2"]);

    expect(proved).toHaveLength(5);
    expect(proved[1]).toMatchObject({ id: copies.tools[0].contextId, kind: "tool", toolName: "read", attestation: "proof-1" });
    expect(proved[3]).toMatchObject({ id: copies.tools[1].contextId, kind: "tool", toolName: "grep", attestation: "proof-2" });
    for (const index of [0, 2, 4]) expect(proved[index]).toBe(copies.contexts[index]);
    // The copies themselves are not rewritten.
    expect(copies.contexts[1]).not.toHaveProperty("attestation");
    expect(copies.contexts[3]).not.toHaveProperty("attestation");
    expect(idsOf(proved)).toEqual(idsOf(copies.contexts));
  });

  it("matches proofs to tool copies by the order of copies.tools, not by position in the list", () => {
    const contexts: ContextItem[] = [tool("t1"), user("u"), tool("t2")];
    const copies = copiesForPaste({ conversationId: "conv_src", contexts }, "conv_dst", new Set(), counter());

    const proved = withCopiedToolProofs(copies, ["first", "second"]);

    expect((proved[0] as ToolContext).attestation).toBe("first");
    expect((proved[2] as ToolContext).attestation).toBe("second");
    expect(proved[1]).toBe(copies.contexts[1]);
  });

  it("leaves a paste with no tool cards as it is", () => {
    const copies = copiesForPaste(
      { conversationId: "conv_src", contexts: [user("u"), assistant("a")] },
      "conv_dst",
      new Set(),
      counter()
    );

    const proved = withCopiedToolProofs(copies, []);

    expect(proved).toEqual(copies.contexts);
    for (const [index, context] of proved.entries()) expect(context).toBe(copies.contexts[index]);
  });
});
