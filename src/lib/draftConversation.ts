import type {
  AttachedWorkspace,
  ContextItem,
  Conversation,
  ConversationSettings,
  RunTarget
} from "../types";
import { conversationHasNoContexts } from "./conversationBodies";
import { captureConversationPresetSettings, sameConversationPresetSettings } from "./conversationPresets";

/**
 * Prefix of the renderer-owned draft conversation ids. A draft id is never
 * persisted or sent to the host; the prefix makes draft detection a string test.
 *
 * Every project has a draft of its own — its next new task — so a draft's id is
 * the prefix plus the project's id ({@link draftConversationId}). Picking another
 * project for a new task goes to that project's draft; nothing moves between them.
 */
export const DRAFT_CONVERSATION_ID = "__draft__";

const DRAFT_ID_PREFIX = `${DRAFT_CONVERSATION_ID}:`;

/** The id of `workspaceId`'s draft, which everything the renderer keys by conversation uses. */
export function draftConversationId(workspaceId: string): string {
  return `${DRAFT_ID_PREFIX}${workspaceId}`;
}

/** The project whose draft `conversationId` names, or null when it names no draft. */
export function draftWorkspaceIdOf(conversationId: string | null | undefined): string | null {
  return conversationId?.startsWith(DRAFT_ID_PREFIX)
    ? conversationId.slice(DRAFT_ID_PREFIX.length) || null
    : null;
}

/** Renderer-owned draft state, one per project. See `drafts` in App. */
export interface DraftConversationState {
  /** The project the draft is the next new task of, and materializes into. */
  workspaceId: string;
  /**
   * The conversation id the draft will materialize as, minted up front.
   *
   * What the draft opens at the host before then — its terminals and its preview page — is
   * owned by this id from the start, so becoming a real conversation hands them over without
   * moving anything. A draft never changes project, so neither does what it opened.
   */
  materializesAs: string;
  settings: ConversationSettings;
  createdAt: string;
  /**
   * The project workspaces (1-based) the user ticked the worktree box for.
   *
   * This is intent only: worktrees belong to persisted conversation IDs. They
   * are created after the conversation materializes and before the first
   * message, when all tool calls must already target the isolated checkouts.
   */
  worktreeMembers: number[];
  /** The draft's run location, persisted directly in `Conversation.runTarget` when materialized. */
  runTarget: RunTarget | null;
  /**
   * Workspaces the draft may work in besides its primary one, persisted directly
   * in `Conversation.attachedWorkspaces` when materialized.
   *
   * Unlike `worktreeMembers` this is not just intent: the host authorized each
   * one when its picker returned it, so the list is already the real grant.
   */
  attachedWorkspaces: AttachedWorkspace[];
  /** Preset the draft's settings came from; empty means an unnamed draft. */
  presetId: string;
  /** Conversation template whose message queue this draft is showing; empty
   * means none. Same trace semantics as `presetId`. */
  templateId: string;
  /**
   * Content the user wrote by hand before sending anything, such as a
   * right-click inserted message. It travels into the conversation at
   * materialization, ahead of the first sent message.
   */
  contexts: ContextItem[];
  /**
   * The opening messages its preset's template laid down, by identity: while
   * `contexts` is still this very array nobody has touched them, so a change of
   * the project's default preset may lay its own in their place.
   */
  templateContexts?: readonly ContextItem[];
  /**
   * The settings its project resolved it to when it was opened, or when the
   * project's default preset last changed under it. Absent for a draft brought
   * back from the document after a restart: only settings the user chose are
   * kept there, so those are the user's ({@link draftFollowsProject}).
   */
  resolvedSettings?: ConversationSettings;
}

/**
 * Whether the draft still holds what its project resolved it to — nobody has
 * changed a preset-owned setting since — so a change of the project's default
 * preset carries it along. Lists compare as sets, as the preset trace does.
 */
export function draftFollowsProject(draft: DraftConversationState): boolean {
  return draft.resolvedSettings !== undefined && sameConversationPresetSettings(
    captureConversationPresetSettings(draft.settings),
    captureConversationPresetSettings(draft.resolvedSettings)
  );
}

/**
 * Whether the draft's settings are worth keeping across a restart: the user
 * chose some, or switched plan mode on. One that only follows its project is
 * rebuilt from the project instead, so it follows a default changed meanwhile.
 */
export function draftHoldsOwnSettings(draft: DraftConversationState): boolean {
  return !draftFollowsProject(draft) || draft.settings.planModeEnabled === true;
}

export function isDraftConversationId(conversationId: string | null | undefined): boolean {
  return draftWorkspaceIdOf(conversationId) !== null;
}

/**
 * Whether a persisted conversation holds nothing yet. New tasks start as the renderer draft, so
 * this is the rare real conversation that is still empty — one that materialized for a tool call
 * the host then refused, or one left from when every project kept an empty slot of its own. The
 * sidebar withholds it until it holds something.
 *
 * Nesting counts as content because hiding a parent would orphan its children in the tree.
 */
export function isUnsentConversation(conversation: Conversation, hasChildren = false): boolean {
  return !hasChildren
    // A body that is not loaded is not an empty one (`conversationBodies.ts`).
    && conversationHasNoContexts(conversation)
    && conversation.queuedMessages.length === 0;
}

/** The workspace's conversations minus the ones that are still empty. */
export function visibleConversations(conversations: Conversation[]): Conversation[] {
  const parentIds = new Set(
    conversations.map((conversation) => conversation.parentConversationId).filter(Boolean)
  );
  return conversations.filter(
    (conversation) => !isUnsentConversation(conversation, parentIds.has(conversation.id))
  );
}

/**
 * Project a draft as a `Conversation` so the normal timeline, composer,
 * workspace, model, reasoning, and security controls render unchanged. A draft
 * can already hold hand-written content; it materializes on the first request,
 * or on the first tool the user runs or records in it, not on the first message.
 */
export function draftAsConversation(
  draft: DraftConversationState,
  title: string
): Conversation {
  return {
    id: draftConversationId(draft.workspaceId),
    title,
    createdAt: draft.createdAt,
    updatedAt: draft.createdAt,
    settings: draft.settings,
    contexts: draft.contexts,
    queuedMessages: [],
    branches: [],
    userAbortedTasks: [],
    worktrees: [],
    runTarget: draft.runTarget,
    attachedWorkspaces: draft.attachedWorkspaces,
    parentConversationId: null,
    presetId: draft.presetId,
    templateId: draft.templateId
  };
}
