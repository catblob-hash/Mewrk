import type { TranslationFunction } from "../i18n";
import type { Conversation, RewindFileEntry } from "../types";
import { isConversationBranchFork } from "./conversationBranches";

/**
 * What rewinding a conversation to before one of its user messages takes off the
 * timeline: that message and everything after it.
 */
export interface RewindRange {
  /** The ids that leave the timeline, in timeline order, the message itself first. */
  ids: string[];
  /**
   * Whether a branch was taken from one of them. A branch's fork message anchors
   * it, so the timeline cannot give such a range up (the same rule as a delete).
   */
  anchored: boolean;
}

/** The range rewinding to before user message `contextId` removes, or null when the timeline has no such message. */
export function rewindRange(
  conversation: Pick<Conversation, "contexts" | "branches">,
  contextId: string
): RewindRange | null {
  const start = conversation.contexts.findIndex((context) => context.id === contextId && context.kind === "user");
  if (start < 0) return null;
  const ids = conversation.contexts.slice(start).map((context) => context.id);
  return {
    ids,
    anchored: ids.some((id) => isConversationBranchFork(conversation as Conversation, id))
  };
}

/** A file the backend would (or did) leave alone has a reason; every other entry takes effect. */
export function isRewindable(entry: RewindFileEntry): boolean {
  return !entry.skipped;
}

export interface RewindFilesSummary {
  /** Files written back as they were. */
  restored: number;
  /** Files that did not exist then, removed. */
  deleted: number;
  skipped: number;
  /** The reason given for the first file left alone, if any. */
  firstSkipReason: string | null;
}

export function summarizeRewindFiles(files: readonly RewindFileEntry[]): RewindFilesSummary {
  let restored = 0;
  let deleted = 0;
  let skipped = 0;
  let firstSkipReason: string | null = null;
  for (const entry of files) {
    if (entry.skipped) {
      skipped += 1;
      firstSkipReason ??= entry.skipped;
    } else if (entry.action === "delete") {
      deleted += 1;
    } else {
      restored += 1;
    }
  }
  return { restored, deleted, skipped, firstSkipReason };
}

function restoreClause(count: number, t: TranslationFunction, done: boolean): string {
  if (done) {
    return count === 1
      ? t("恢复了 1 个文件", "restored 1 file")
      : t("恢复了 {count} 个文件", "restored {count} files", { count });
  }
  return count === 1
    ? t("恢复 1 个文件", "restore 1 file")
    : t("恢复 {count} 个文件", "restore {count} files", { count });
}

function deleteClause(count: number, t: TranslationFunction, done: boolean): string {
  if (done) {
    return count === 1
      ? t("删除了 1 个之后新建的文件", "deleted 1 file created since then")
      : t("删除了 {count} 个之后新建的文件", "deleted {count} files created since then", { count });
  }
  return count === 1
    ? t("删除 1 个之后新建的文件", "delete 1 file created since then")
    : t("删除 {count} 个之后新建的文件", "delete {count} files created since then", { count });
}

/** The one line the dialog says over its file list, before anything is touched. */
export function rewindPreviewSummary(summary: RewindFilesSummary, t: TranslationFunction): string {
  const parts = [
    ...(summary.restored ? [restoreClause(summary.restored, t, false)] : []),
    ...(summary.deleted ? [deleteClause(summary.deleted, t, false)] : [])
  ];
  const lead = parts.length
    ? t("将{parts}", "Will {parts}", { parts: parts.join(t("，", ", ")) })
    : t("没有文件会被恢复", "No file will be restored");
  return summary.skipped
    ? t("{lead}；{count} 个会被跳过", "{lead}; {count} will be skipped", { lead, count: summary.skipped })
    : lead;
}

/** What a restore did, for the notice: the counts, and the first reason a file was left alone. */
export function rewindResultSummary(summary: RewindFilesSummary, t: TranslationFunction): string {
  const parts = [
    ...(summary.restored ? [restoreClause(summary.restored, t, true)] : []),
    ...(summary.deleted ? [deleteClause(summary.deleted, t, true)] : [])
  ];
  const done = parts.length ? parts.join(t("，", ", ")) : t("没有文件被恢复", "no file was restored");
  if (!summary.skipped) return done;
  return t("{done}；跳过 {count} 个：{reason}", "{done}; skipped {count}: {reason}", {
    done,
    count: summary.skipped,
    reason: summary.firstSkipReason ?? ""
  });
}
