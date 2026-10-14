/**
 * What "mention in the conversation" writes at the end of the composer: a line
 * of a file as `path:line:`, left open for what the reader has to say about it.
 */

/** A line a pane was asked to mention. */
export interface FileMention {
  /** Relative to its workspace's root and `/` separated; absolute when the file is in no workspace. */
  path: string;
  /** The number of the workspace the file is in, or null when it is in none. */
  workspace: number | null;
  line: number;
}

/**
 * The mention as text.
 *
 * A conversation with several workspaces says which one after the path, as
 * `@<number>` — the way a task address carries it (`preview:<serverId>@<number>`).
 * The number is not a folder, so it is never a segment of the path; with one
 * workspace there is nothing to tell apart and the path stands alone.
 */
export function fileMentionText(mention: FileMention, severalWorkspaces: boolean): string {
  const place = severalWorkspaces && mention.workspace !== null
    ? `${mention.path}@${mention.workspace}`
    : mention.path;
  return `${place}:${mention.line}:`;
}

/**
 * The line a pointer event landed on, in a view whose rows carry `data-line`:
 * the nearest one around the event's target, or null when it landed between rows.
 */
export function lineAtTarget(target: EventTarget | null): number | null {
  if (!(target instanceof Element)) return null;
  const line = Number(target.closest("[data-line]")?.getAttribute("data-line"));
  return Number.isSafeInteger(line) && line > 0 ? line : null;
}

/** A draft with a mention on a line of its own at its end. */
export function draftWithMention(draft: string, mention: string): string {
  if (!draft) return mention;
  return `${draft}${draft.endsWith("\n") ? "" : "\n"}${mention}`;
}
