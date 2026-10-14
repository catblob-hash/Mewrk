import type { ContextItem, ToolContext, ToolDescriptor } from "../types";
import { renumberImagesForSend } from "./imageShortIds";

/**
 * Copying contexts off a timeline and pasting them onto one.
 *
 * What is copied is held by the app, not by the system clipboard: a context is
 * more than its text, and what comes back has to be the cards themselves. The
 * text does go to the system clipboard as well, so a copy pastes somewhere
 * else as something readable.
 *
 * A paste never puts the copied contexts themselves down. Each is a new card
 * with a new id, and whatever tied the original to its place is renewed or left
 * behind (see {@link copiesForPaste}); a copied tool card additionally needs
 * the host to prove its source and vouch for the copy, which the caller asks
 * for with what this returns.
 */

/** Contexts copied off a timeline, as they were when copied. */
export interface TimelineClipboard {
  /** The conversation they were copied out of, which a tool card's proof names. */
  conversationId: string;
  contexts: ContextItem[];
}

/**
 * Ids the host mints for cards of its own: a result or a message delivered from
 * an agent, a skill, a hand-over's index, what a hook or the system appended.
 * The id is how the host recognises such a card, so a copy under a new id
 * would be a different thing, and one under the same prefix a forgery.
 */
const HOST_CARD_ID_PREFIXES = [
  "ctx_agent-result_",
  "ctx_agent-message_",
  "ctx_structured-output-nudge_",
  "ctx_system-append_",
  "ctx_handoff_index",
  "ctx_skill_",
  "ctx_hook-continuation_"
] as const;

/**
 * Whether a context can be copied: one whose meaning is its own content, not
 * its place in a run. A compaction stands for everything above it, a subagent
 * call owns a child transcript, an orchestration call is drawn from the run it
 * belongs to, and a hook record is a diagnostic of one execution.
 */
export function timelineCopyable(context: ContextItem, tools: readonly ToolDescriptor[]): boolean {
  if ("streaming" in context && context.streaming) return false;
  if (HOST_CARD_ID_PREFIXES.some((prefix) => context.id.startsWith(prefix))) return false;
  if (context.kind === "system") {
    return !context.nativeCompaction && !context.toolsAdded && !context.hookExecution;
  }
  if (context.kind === "tool") {
    if (context.subagent || context.notice !== undefined) return false;
    return tools.find((tool) => tool.name === context.toolName)?.category !== "orchestration";
  }
  return true;
}

/**
 * Whether a context can be moved. A compaction cannot: every request starts
 * from the last one, so one carried down the timeline would silently drop
 * everything above where it landed.
 */
export function timelineMovable(context: ContextItem): boolean {
  return !(context.kind === "system" && context.nativeCompaction);
}

/** What copied contexts read as anywhere that takes only text. */
export function timelineClipboardText(contexts: readonly ContextItem[]): string {
  return contexts
    .map((context) => (context.kind === "tool"
      ? `${context.toolName} ${JSON.stringify(context.input)}\n${context.result.output}`.trim()
      : (context.content ?? "").trim()))
    .filter(Boolean)
    .join("\n\n");
}

export interface PasteCopies {
  /** The copies, in order. A tool card among them has no proof yet. */
  contexts: ContextItem[];
  /** Each copied tool card with the card it copies, for the host to prove and vouch for. */
  tools: Array<{ contextId: string; source: ToolContext }>;
  /** How many copied contexts could not be pasted here. */
  skipped: number;
}

/**
 * The cards a paste puts down, made from what was copied.
 *
 * - Every card gets a new id. A conversation never holds one id twice.
 * - A user message's images are numbered again against `takenImageIds`, the
 *   numbers the destination already uses, and its `[Image #N]` text with them.
 * - A tool card's image numbers are written into its attested result, so they
 *   cannot change: it is pasted only where they do not clash — always in the
 *   conversation it was copied from, where the same number is the same image.
 * - What one model round emitted stays one group, under a turn id of its own.
 * - The provider's id for a call is not carried over: two calls answering to
 *   one id cannot both go to the provider.
 * - Cited sources, like a fork, are left behind.
 */
export function copiesForPaste(
  clipboard: TimelineClipboard,
  destinationConversationId: string,
  takenImageIds: Set<number>,
  newId: (prefix: "ctx" | "turn") => string
): PasteCopies {
  const turnIds = new Map<string, string>();
  const turnOf = (source: string | undefined): string | undefined => {
    if (source === undefined) return undefined;
    let turn = turnIds.get(source);
    if (!turn) {
      turn = newId("turn");
      turnIds.set(source, turn);
    }
    return turn;
  };
  const sameConversation = clipboard.conversationId === destinationConversationId;
  const copies: PasteCopies = { contexts: [], tools: [], skipped: 0 };
  for (const source of clipboard.contexts) {
    if (source.kind === "user") {
      const numbered = source.images?.length
        ? renumberImagesForSend(takenImageIds, source.content, source.images)
        : null;
      copies.contexts.push({ ...source, ...numbered, id: newId("ctx") });
    } else if (source.kind === "system") {
      copies.contexts.push({ ...source, id: newId("ctx") });
    } else if (source.kind === "assistant") {
      const { sources: _sources, modelTurnId, ...rest } = source;
      const turn = turnOf(modelTurnId);
      copies.contexts.push({ ...rest, id: newId("ctx"), ...(turn ? { modelTurnId: turn } : {}) });
    } else if (source.kind === "reasoning") {
      const { streaming: _streaming, modelTurnId, ...rest } = source;
      const turn = turnOf(modelTurnId);
      copies.contexts.push({ ...rest, id: newId("ctx"), ...(turn ? { modelTurnId: turn } : {}) });
    } else {
      const imageIds = (source.result.images ?? []).flatMap((image) => (
        image.shortId === undefined ? [] : [image.shortId]
      ));
      if (!sameConversation && imageIds.some((id) => takenImageIds.has(id))) {
        copies.skipped += 1;
        continue;
      }
      for (const id of imageIds) takenImageIds.add(id);
      const {
        providerCallId: _providerCallId,
        attestation: _attestation,
        streaming: _streaming,
        streamStatus: _streamStatus,
        live: _live,
        modelTurnId,
        ...rest
      } = source;
      const turn = turnOf(modelTurnId);
      const copy: ToolContext = { ...rest, id: newId("ctx"), ...(turn ? { modelTurnId: turn } : {}) };
      copies.contexts.push(copy);
      copies.tools.push({ contextId: copy.id, source });
    }
  }
  return copies;
}

/** The copies with each tool card carrying the proof the host issued for it. */
export function withCopiedToolProofs(copies: PasteCopies, proofs: readonly string[]): ContextItem[] {
  const proofById = new Map(copies.tools.map((tool, index) => [tool.contextId, proofs[index]]));
  return copies.contexts.map((context) => {
    const attestation = proofById.get(context.id);
    return context.kind === "tool" && attestation ? { ...context, attestation } : context;
  });
}
