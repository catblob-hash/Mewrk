import type { AppDocument, Conversation } from "../types";

/**
 * Whether the user's Stop holds this conversation's queue: its queued messages
 * wait for the user's next send instead of going out as rounds end. Saved with
 * the conversation (`Conversation.queuePaused`), so a restart does not send what
 * the user held back.
 */
export function queueIsPaused(conversation: Conversation): boolean {
  return conversation.queuePaused === true;
}

/** The document with one conversation's queue paused or resumed, wherever it lives. */
export function withQueuePaused(document: AppDocument, conversationId: string, paused: boolean): AppDocument {
  let changed = false;
  const workspaces = document.workspaces.map((workspace) => {
    if (!workspace.conversations.some((conversation) => conversation.id === conversationId)) return workspace;
    return {
      ...workspace,
      conversations: workspace.conversations.map((conversation) => {
        if (conversation.id !== conversationId || queueIsPaused(conversation) === paused) return conversation;
        changed = true;
        const { queuePaused: _previous, ...rest } = conversation;
        return paused ? { ...rest, queuePaused: true } : rest;
      })
    };
  });
  return changed ? { ...document, workspaces } : document;
}
