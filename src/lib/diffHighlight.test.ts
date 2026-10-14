import { describe, expect, it } from "vitest";
import type { CodeToken } from "./codeHighlight";
import { highlightDiffLines, markTokens } from "./diffHighlight";
import type { DiffSide } from "./diffHighlight";
import type { WordDiffSegment } from "./wordDiff";

interface HunkLine {
  kind: "addition" | "deletion" | "context";
  text: string;
}

/** Additions are the new file, deletions the old one, and context lines both. */
function sideOf(line: HunkLine): { side: DiffSide; text: string } {
  const side: DiffSide = line.kind === "addition" ? "new" : line.kind === "deletion" ? "old" : "both";
  return { side, text: line.text };
}

/** `kind:value` for each token, short enough to read as an expectation. */
function marked(tokens: readonly CodeToken[] | undefined): string[] {
  return (tokens ?? []).map((token) => `${token.kind}:${token.value}`);
}

describe("highlightDiffLines", () => {
  it("colours nothing when the language is null or has no grammar", () => {
    const hunk: HunkLine[] = [{ kind: "addition", text: "const x = 1;" }];

    expect(highlightDiffLines(null, hunk, sideOf).size).toBe(0);
    expect(highlightDiffLines("klingon", hunk, sideOf).size).toBe(0);
  });

  it("tokenizes the old side alone, so a deleted block comment does not reach an added line", () => {
    const lines: HunkLine[] = [
      { kind: "deletion", text: "/* start" },
      { kind: "addition", text: "const x = 1;" }
    ];
    const tokens = highlightDiffLines("typescript", lines, sideOf);

    expect(marked(tokens.get(lines[0]))).toEqual(["comment:/* start"]);
    expect(marked(tokens.get(lines[1]))).toEqual(["keyword:const", "plain: x = ", "number:1", "plain:;"]);
  });

  it("reads a context line after a deleted block comment as the new file does, where no comment is open", () => {
    const lines: HunkLine[] = [
      { kind: "deletion", text: "/* start" },
      { kind: "context", text: "still open */" }
    ];
    const tokens = highlightDiffLines("typescript", lines, sideOf);

    expect(marked(tokens.get(lines[1]))).toEqual(["plain:still open */"]);
  });

  it("carries a block comment an added line opens onto the context line after it", () => {
    const lines: HunkLine[] = [
      { kind: "addition", text: "/* start" },
      { kind: "context", text: "still open */" }
    ];
    const tokens = highlightDiffLines("typescript", lines, sideOf);

    expect(marked(tokens.get(lines[0]))).toEqual(["comment:/* start"]);
    expect(marked(tokens.get(lines[1]))).toEqual(["comment:still open */"]);
  });
});

describe("markTokens", () => {
  const tokens: CodeToken[] = [
    { kind: "keyword", value: "const" },
    { kind: "plain", value: " a = " },
    { kind: "string", value: "\"x\"" }
  ];

  it("cuts the tokens where the segments begin and end, keeping each piece's kind", () => {
    const segments: WordDiffSegment[] = [
      { text: "const a = \"", changed: false },
      { text: "x", changed: true },
      { text: "\"", changed: false }
    ];
    const runs = markTokens(tokens, segments);

    expect(runs.map((run) => ({ changed: run.changed, tokens: marked(run.tokens) }))).toEqual([
      { changed: false, tokens: ["keyword:const", "plain: a = ", "string:\""] },
      { changed: true, tokens: ["string:x"] },
      { changed: false, tokens: ["string:\""] }
    ]);
  });

  it("concatenates back to the original text across segments that split tokens", () => {
    const segments: WordDiffSegment[] = [
      { text: "con", changed: true },
      { text: "st a = \"x", changed: false },
      { text: "\"", changed: true }
    ];
    const runs = markTokens(tokens, segments);

    expect(runs.map((run) => run.tokens.map((token) => token.value).join("")).join("")).toBe("const a = \"x\"");
  });

  it("produces no run for an empty segment", () => {
    const runs = markTokens(tokens, [
      { text: "", changed: true },
      { text: "const", changed: false },
      { text: "", changed: false }
    ]);

    expect(runs).toEqual([{ changed: false, tokens: [{ kind: "keyword", value: "const" }] }]);
  });
});
