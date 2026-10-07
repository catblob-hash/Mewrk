import type {
  AgentModelSelection,
  AgentRole,
  AgentRoleResource,
  ApiProvider,
  ConversationWebSearchSettings
} from "../types";
import { NATIVE_FETCH_TOOLS, NATIVE_SEARCH_TOOLS } from "../types";
import {
  DEFAULT_SEARCH_COMPRESSION_CUTOFF,
  DEFAULT_SEARCH_MAX_RESULTS
} from "./searchProviders";

/** Mirrors the Rust `MAX_AGENT_TYPE_CHARS`. */
export const MAX_AGENT_TYPE_CHARS = 64;

export type AgentTypeNameError =
  | "required"
  | "too_long"
  | "characters";

/**
 * Whether a role's bound model resolves against the current providers.
 *
 * Mirrors the Rust `agent_definition_model_is_available`, and the failure cases
 * of `enabled_provider_model` it delegates to: provider missing, provider
 * disabled, model missing under that provider. Presence in the provider's model
 * list is the whole of a model's availability — a model that is listed is usable.
 * `explicit` records the exact `(providerId, modelId)` PAIR because two providers
 * may both carry a model called `gpt-4o` and they are not the same model —
 * matching on the bare ID would silently rebind a role to some other provider's
 * model.
 *
 * `inherit` always resolves: it rides the caller's own provider and model.
 * `unavailable` never does — there is no pair left to re-check: an older build
 * wrote it in place of a binding it had found dead, and the host lists a
 * built-in role whose provider family is missing this way.
 *
 * This matters beyond the editor's own validation. The host hides a role whose
 * model does not resolve from the listing it sends the model, so such a role is
 * uncallable — and the settings list has to say so, or a user reads a normal
 * row as "this works".
 */
export function agentModelSelectionIsAvailable(
  selection: AgentModelSelection,
  providers: readonly ApiProvider[]
): boolean {
  if (selection.kind === "inherit") return true;
  if (selection.kind === "unavailable") return false;
  const provider = providers.find((candidate) => candidate.id === selection.providerId);
  if (!provider?.enabled) return false;
  return provider.models.some((candidate) => candidate.id === selection.modelId);
}

/**
 * The roles a conversation can reach: the global level and the built-ins, plus
 * the conversation's own workspaces. A role of a workspace the conversation does
 * not have is one no run of it can see — the page draws a selection of it as
 * dangling, and the host never offers it. `workspaceKeys` omitted narrows
 * nothing, which is a preset: it points at no workspace in particular.
 */
function reachableAgentRoles(
  agents: readonly AgentRoleResource[],
  workspaceKeys?: readonly string[]
): AgentRoleResource[] {
  const reachable = workspaceKeys ? new Set(workspaceKeys) : null;
  return agents.filter((resource) => (
    !reachable || !resource.workspaceKey || reachable.has(resource.workspaceKey)
  ));
}

/**
 * Whether the conversation offers the model any role it can actually call.
 *
 * The renderer's approximation of `api::available_agent_type_names`: a selected
 * id the catalog lists at a level the conversation reaches (`workspaceKeys`, as
 * for `selectedAgentRoleCount`), whose file could be read, and whose model
 * resolves. It cannot see same-name precedence, so it controls only how the
 * no-role-subagent switch explains itself. That is safe because the host falls
 * back to allowing role-less children whenever its final set is empty; a switch
 * that reads as live a moment too early cannot remove a tool.
 */
export function hasUsableAgentRole(
  agents: readonly AgentRoleResource[],
  agentIds: readonly string[],
  providers: readonly ApiProvider[],
  workspaceKeys?: readonly string[]
): boolean {
  const selected = new Set(agentIds);
  return reachableAgentRoles(agents, workspaceKeys).some((resource) => (
    selected.has(resource.id)
    && resource.available
    && resource.role !== null
    && agentModelSelectionIsAvailable(resource.role.modelSelection, providers)
  ));
}

/**
 * How many of a conversation's selected roles the catalog actually lists.
 *
 * Every surface that COUNTS roles uses this, the same population the roles page
 * divides by: a dangling id is still a row the user can untick, but counting it
 * would announce a role nothing can call. `workspaceKeys` narrows that population
 * the way the page does — the global level and the built-ins, plus the
 * conversation's own workspaces — so a role of a workspace this conversation
 * does not have is dangling here too. Omitted, nothing is narrowed (a preset).
 */
export function selectedAgentRoleCount(
  agents: readonly AgentRoleResource[],
  agentIds: readonly string[],
  workspaceKeys?: readonly string[]
): number {
  const known = new Set(reachableAgentRoles(agents, workspaceKeys).map((resource) => resource.id));
  return new Set(agentIds.filter((id) => known.has(id))).size;
}

/**
 * What a role may be called.
 *
 * Free text, deliberately: a role name is prose the user writes and the model
 * reads back, not an identifier anything constructs a path or an address out
 * of, so "代码审查" and "Code Reviewer" are as legitimate as `reviewer`. The
 * host resolves what the model says to a definition by exact match first and by
 * a case-, whitespace- and separator-insensitive pass second, accepting the
 * loose reading only when it lands on exactly one role. The file a role lives in
 * is named by the host from a slug of it, so nothing here has to be a valid
 * file name either.
 *
 * Three things are still refused, and none of them is about shape:
 * empty (there would be nothing to name), longer than the host stores, and
 * control characters — those cannot survive being echoed back in a listing or
 * an error, so a role carrying one would be unaddressable in practice.
 *
 * Mirrors the Rust `validate_agent_type_name`; leading and trailing whitespace
 * is trimmed by the editor before it gets here rather than rejected, because
 * a trailing space is a slip and not a decision.
 */
export function validateAgentTypeName(value: string): AgentTypeNameError | null {
  const characters = Array.from(value);
  if (characters.length === 0) return "required";
  if (characters.length > MAX_AGENT_TYPE_CHARS) return "too_long";
  // Escaped rather than literal: a control character typed into this source
  // would be invisible in the one place it must be readable.
  if (/[\u0000-\u001f\u007f]/u.test(value)) return "characters";
  return null;
}

/** Sorted and de-duplicated, matching the host's canonicalization so a reorder
 * in the editor is not mistaken for a configuration change. */
export function canonicalAgentToolNames(names: readonly string[]): string[] {
  return [...new Set(names)].sort();
}

/**
 * The web configuration a new role starts with: the same answers the built-in
 * preset ships — both legs native, the basic native tool versions, the shared
 * result shaping, no domain filter — and nothing borrowed from whichever
 * conversation the editor was opened from. A role is a reusable asset, so its
 * opening state cannot depend on where it was created.
 */
export function defaultAgentRoleWebSearch(): ConversationWebSearchSettings {
  return {
    maxSearchesPerCall: 0,
    provider: { kind: "native" },
    fetchProvider: { kind: "native" },
    nativeSearchTool: NATIVE_SEARCH_TOOLS[0],
    nativeFetchTool: NATIVE_FETCH_TOOLS[0],
    maxResults: DEFAULT_SEARCH_MAX_RESULTS,
    compressionCutoff: DEFAULT_SEARCH_COMPRESSION_CUTOFF,
    fetchCompressionCutoff: DEFAULT_SEARCH_COMPRESSION_CUTOFF,
    domainFilter: "off",
    includeDomains: [],
    excludeDomains: []
  };
}

/** Structural equality that ignores object key order. */
function equalValues(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b) || a.length !== b.length) return false;
    return a.every((item, index) => equalValues(item, b[index]));
  }
  if (!a || !b || typeof a !== "object" || typeof b !== "object") return false;
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  const keys = new Set([...Object.keys(left), ...Object.keys(right)]);
  return [...keys].every((key) => equalValues(left[key], right[key]));
}

/**
 * Whether two role bodies say the same thing, field for field.
 *
 * The editor's concurrency check: it remembers the body it opened on, and a
 * save whose file has since been rewritten — by hand, by another window, by
 * the host's own export — is refused rather than written over. Order is
 * meaning in every list here except `tools`, which the editor and the host both
 * keep canonical, so a plain structural comparison is the whole rule.
 */
export function sameAgentRole(a: AgentRole, b: AgentRole): boolean {
  return equalValues(a, b);
}

/** A role body copied deep enough that editing the copy never touches the catalog. */
export function cloneAgentRole(role: AgentRole): AgentRole {
  return structuredClone(role);
}
