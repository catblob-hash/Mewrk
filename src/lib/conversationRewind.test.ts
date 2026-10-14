import { describe, expect, it } from "vitest";
import { t } from "../i18n";
import type { ContextItem, Conversation, RewindFileEntry } from "../types";
import {
  rewindPreviewSummary,
  rewindRange,
  rewindResultSummary,
  summarizeRewindFiles
} from "./conversationRewind";

const at = "2026-10-13T00:00:00Z";
const contexts: ContextItem[] = [
  { id: "u1", kind: "user", content: "第一问", createdAt: at },
  { id: "a1", kind: "assistant", content: "第一答", createdAt: at },
  { id: "u2", kind: "user", content: "第二问", createdAt: at },
  { id: "a2", kind: "assistant", content: "第二答", createdAt: at }
];

function conversation(branchForks: string[] = []): Pick<Conversation, "contexts" | "branches"> {
  return {
    contexts,
    branches: branchForks.map((forkContextId, index) => ({
      id: `branch-${index}`,
      forkContextId,
      active: index === 0,
      contexts: [],
      createdAt: at,
      updatedAt: at
    }))
  };
}

const file = (displayPath: string, action: RewindFileEntry["action"], skipped: string | null = null): RewindFileEntry => ({
  path: `/work/${displayPath}`,
  displayPath,
  action,
  skipped
});

describe("rewindRange", () => {
  it("takes the user message and everything after it", () => {
    expect(rewindRange(conversation(), "u2")).toEqual({ ids: ["u2", "a2"], anchored: false });
    expect(rewindRange(conversation(), "u1")?.ids).toEqual(["u1", "a1", "u2", "a2"]);
  });

  it("finds no range for a message that is not a user message on the timeline", () => {
    expect(rewindRange(conversation(), "a1")).toBeNull();
    expect(rewindRange(conversation(), "missing")).toBeNull();
  });

  it("marks a range holding a branch's fork message, wherever in the range it is", () => {
    expect(rewindRange(conversation(["u2"]), "u2")?.anchored).toBe(true);
    expect(rewindRange(conversation(["u2"]), "u1")?.anchored).toBe(true);
    expect(rewindRange(conversation(["u1"]), "u2")?.anchored).toBe(false);
  });
});

describe("rewind file summaries", () => {
  const files = [
    file("src/a.ts", "restore"),
    file("src/b.ts", "restore"),
    file("src/new.ts", "delete"),
    file("src/locked.ts", "restore", "文件已被外部修改"),
    file("src/other.ts", "delete", "不在工作区内")
  ];

  it("counts what takes effect and what is left alone, with the first reason", () => {
    expect(summarizeRewindFiles(files)).toEqual({
      restored: 2,
      deleted: 1,
      skipped: 2,
      firstSkipReason: "文件已被外部修改"
    });
  });

  it("says what a rewind would do before it does it", () => {
    expect(rewindPreviewSummary(summarizeRewindFiles(files), t)).toBe(
      "将恢复 2 个文件，删除 1 个之后新建的文件；2 个会被跳过"
    );
    expect(rewindPreviewSummary(summarizeRewindFiles([file("a", "restore")]), t)).toBe("将恢复 1 个文件");
    expect(rewindPreviewSummary(summarizeRewindFiles([file("a", "restore", "只读")]), t)).toBe(
      "没有文件会被恢复；1 个会被跳过"
    );
  });

  it("reports the outcome with the first reason for a skipped file", () => {
    expect(rewindResultSummary(summarizeRewindFiles(files), t)).toBe(
      "恢复了 2 个文件，删除了 1 个之后新建的文件；跳过 2 个：文件已被外部修改"
    );
    expect(rewindResultSummary(summarizeRewindFiles([file("a", "delete")]), t)).toBe("删除了 1 个之后新建的文件");
    expect(rewindResultSummary(summarizeRewindFiles([]), t)).toBe("没有文件被恢复");
  });
});
