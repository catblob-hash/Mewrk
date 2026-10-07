//! Calls whose result arrives later, on the call itself.
//!
//! The host projects every message it hands the model as a user-role message
//! (Rust `aisdk::project::host_delivery_messages`), the way Claude Code
//! delivers a background task's `<task-notification>` and its reminders, and
//! marks it `providerOptions.mewrk.hostMessage`. Two more marks make the same
//! transcript replay on a model that takes asynchronous tool calls (Rust
//! `async_tools.rs`):
//!
//! - `asyncLaunch` on a tool-result part: the call started a background task
//!   whose result is still to come as the call's own output.
//! - `asyncResult` on a host message: `{ toolCallId, output }`, the task's
//!   result as that call's output.
//!
//! Where the request declares asynchronous tools (`request.asyncTools`, the
//! host's word that the model takes them; only the Responses protocol has
//! them), a launch's receipt gives way to a placeholder the fetch wrapper
//! removes — the call is sent with `async: true` and no output — and a result
//! for a call still waiting becomes that call's `function_call_output`, ahead
//! of the other results it rides with, as OpenAI's guide orders them.
//! Everywhere else both stay what the host projected: a receipt, and a user
//! message. The marks never reach a provider.

import type { ProviderFamily, StepRequest } from "./protocol.js";

type JsonObject = Record<string, unknown>;

function isObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Mirrors Rust `aisdk::project::MARKER_OPTIONS_KEY` and the marker names. */
const MARKER_OPTIONS_KEY = "mewrk";
const HOST_MESSAGE_MARKER = "hostMessage";
const ASYNC_RESULT_MARKER = "asyncResult";
const ASYNC_LAUNCH_MARKER = "asyncLaunch";

/**
 * The output a pending launch carries through the SDK, which would otherwise
 * refuse a call with no result. The fetch wrapper drops the item and marks the
 * call `async`. The NUL keeps it from ever matching text a tool returned.
 */
export const ASYNC_PENDING_SENTINEL = "\u0000mewrk:async-pending";

/** Families with asynchronous tool calls: the Responses protocol's `async: true`. */
function hasAsyncTools(family: ProviderFamily): boolean {
  return family === "openai-responses" || family === "openai-codex" || family === "azure";
}

/** Whether this request declares asynchronous tools. */
export function declaresAsyncTools(request: Pick<StepRequest, "family" | "asyncTools">): boolean {
  return hasAsyncTools(request.family) && (request.asyncTools?.length ?? 0) > 0;
}

/** The host's marks on `holder`, or `null`. */
function marksOf(holder: unknown): JsonObject | null {
  if (!isObject(holder) || !isObject(holder.providerOptions)) return null;
  const marks = holder.providerOptions[MARKER_OPTIONS_KEY];
  return isObject(marks) ? marks : null;
}

/** Removes the host's marks from `holder`, and its `providerOptions` if nothing else is left. */
function stripMarks(holder: JsonObject, keep: readonly string[] = []): void {
  const options = holder.providerOptions;
  if (!isObject(options)) return;
  const marks = options[MARKER_OPTIONS_KEY];
  if (!isObject(marks)) return;
  const kept = Object.fromEntries(Object.entries(marks).filter(([key]) => keep.includes(key)));
  if (Object.keys(kept).length > 0) options[MARKER_OPTIONS_KEY] = kept;
  else delete options[MARKER_OPTIONS_KEY];
  if (Object.keys(options).length === 0) delete holder.providerOptions;
}

/** Whether `message` is a message the host wrote, not the user (`hostMessage`). */
export function isHostMessage(message: unknown): boolean {
  return isObject(message) && message.role === "user" && marksOf(message)?.[HOST_MESSAGE_MARKER] === true;
}

/**
 * Shapes the host's deliveries for this request, in place: on the calls they
 * answer where the request declares asynchronous tools, as the user messages
 * they are everywhere else. The `hostMessage` mark stays only for the Claude
 * Code session, which folds such messages behind a parked round's results.
 */
export function applyAsyncTools(request: Pick<StepRequest, "family" | "asyncTools" | "messages">): void {
  const native = declaresAsyncTools(request);
  const keepHostMark = request.family === "claude-agent" ? [HOST_MESSAGE_MARKER] : [];
  /** Launches still owed their output, by call id, with the tool that made them. */
  const pending = new Map<string, string>();
  /** How many converted results each tool message already starts with. */
  const leading = new Map<JsonObject, number>();
  const out: unknown[] = [];
  for (const message of request.messages) {
    if (!isObject(message)) {
      out.push(message);
      continue;
    }
    if (message.role === "tool" && Array.isArray(message.content)) {
      for (const part of message.content) {
        if (!isObject(part) || part.type !== "tool-result") continue;
        if (marksOf(part)?.[ASYNC_LAUNCH_MARKER] === true && native && typeof part.toolCallId === "string") {
          part.output = { type: "text", value: ASYNC_PENDING_SENTINEL };
          pending.set(part.toolCallId, typeof part.toolName === "string" ? part.toolName : "");
        }
        stripMarks(part);
      }
      out.push(message);
      continue;
    }
    if (message.role === "user") {
      const marks = marksOf(message);
      const result = marks?.[ASYNC_RESULT_MARKER];
      if (native && isObject(result) && typeof result.toolCallId === "string"
        && typeof result.output === "string" && pending.has(result.toolCallId)) {
        const part = {
          type: "tool-result",
          toolCallId: result.toolCallId,
          toolName: pending.get(result.toolCallId) ?? "",
          output: { type: "text", value: result.output },
        };
        pending.delete(result.toolCallId);
        const previous = out[out.length - 1];
        if (isObject(previous) && previous.role === "tool" && Array.isArray(previous.content)) {
          const at = leading.get(previous) ?? 0;
          previous.content.splice(at, 0, part);
          leading.set(previous, at + 1);
        } else {
          const carrier = { role: "tool", content: [part] };
          leading.set(carrier, 1);
          out.push(carrier);
        }
        continue;
      }
      if (marks) stripMarks(message, keepHostMark);
    }
    out.push(message);
  }
  request.messages = out;
}

/** The function tools a Responses body declares, and the calls and outputs in its input. */
function rewriteAsyncBody(body: JsonObject, asyncTools: readonly string[]): boolean {
  let changed = false;
  if (Array.isArray(body.tools)) {
    for (const tool of body.tools) {
      if (isObject(tool) && tool.type === "function" && typeof tool.name === "string"
        && asyncTools.includes(tool.name) && tool.async !== true) {
        tool.async = true;
        changed = true;
      }
    }
  }
  if (Array.isArray(body.input)) {
    const launches = new Set<string>();
    const input = body.input.filter((item) => {
      if (isObject(item) && item.type === "function_call_output" && item.output === ASYNC_PENDING_SENTINEL
        && typeof item.call_id === "string") {
        launches.add(item.call_id);
        return false;
      }
      return true;
    });
    for (const item of input) {
      if (isObject(item) && item.type === "function_call" && typeof item.call_id === "string"
        && launches.has(item.call_id)) {
        item.async = true;
      }
    }
    if (launches.size > 0) {
      body.input = input;
      changed = true;
    }
  }
  return changed;
}

/**
 * Wraps a Responses `fetch` so the tools the host declares asynchronous go out
 * with `async: true`, and so does every launch still waiting for its output,
 * whose placeholder output is removed (`applyAsyncTools`). With no
 * asynchronous tools the request passes through untouched.
 */
export function asyncToolsResponsesFetch(
  inner: typeof globalThis.fetch = globalThis.fetch,
  asyncTools: readonly string[] = [],
): typeof globalThis.fetch {
  if (asyncTools.length === 0) return inner;
  return async (input, init) => {
    const text = init && typeof init.body === "string" ? init.body : null;
    let body: unknown;
    try {
      body = text === null ? null : JSON.parse(text);
    } catch {
      body = null;
    }
    if (!isObject(body) || !rewriteAsyncBody(body, asyncTools)) return inner(input, init);
    return inner(input, { ...init, body: JSON.stringify(body) });
  };
}
