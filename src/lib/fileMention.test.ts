import { describe, expect, it } from "vitest";
import { draftWithMention, fileMentionText, lineAtTarget } from "./fileMention";

describe("fileMentionText", () => {
  it("writes the path and the line, each closed by a colon", () => {
    expect(fileMentionText({ path: "src/App.tsx", workspace: 1, line: 42 }, false)).toBe("src/App.tsx:42:");
  });

  it("names the workspace after the path once the conversation has several", () => {
    expect(fileMentionText({ path: "src/App.tsx", workspace: 1, line: 42 }, true)).toBe("src/App.tsx@1:42:");
    expect(fileMentionText({ path: "lib/beta.py", workspace: 2, line: 5 }, true)).toBe("lib/beta.py@2:5:");
  });

  it("leaves a file outside every workspace as the absolute path it is", () => {
    expect(fileMentionText({ path: "C:\\Notes\\todo.md", workspace: null, line: 3 }, true)).toBe("C:\\Notes\\todo.md:3:");
  });
});

describe("lineAtTarget", () => {
  it("answers with the line of the nearest row around a nested target", () => {
    const outer = document.createElement("div");
    outer.setAttribute("data-line", "7");
    const inner = document.createElement("div");
    inner.setAttribute("data-line", "9");
    const leaf = document.createElement("b");
    inner.append(leaf);
    outer.append(inner);

    expect(lineAtTarget(leaf)).toBe(9);
    expect(lineAtTarget(outer)).toBe(7);
  });

  it("is null for an element outside any row", () => {
    expect(lineAtTarget(document.createElement("span"))).toBeNull();
  });

  it("is null for a target that is not an element", () => {
    expect(lineAtTarget(null)).toBeNull();
    expect(lineAtTarget(document)).toBeNull();
  });
});

describe("draftWithMention", () => {
  it("is the whole draft when there was none", () => {
    expect(draftWithMention("", "a.ts:1:")).toBe("a.ts:1:");
  });

  it("goes on a line of its own after what was written", () => {
    expect(draftWithMention("look here", "a.ts:1:")).toBe("look here\na.ts:1:");
  });

  it("does not open a second line when the draft already ends one", () => {
    expect(draftWithMention("look here\n", "a.ts:1:")).toBe("look here\na.ts:1:");
  });
});
