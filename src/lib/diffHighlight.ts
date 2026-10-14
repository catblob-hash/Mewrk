import { grammarFor, highlightCodeLines } from "./codeHighlight";
import type { CodeToken } from "./codeHighlight";
import type { WordDiffSegment } from "./wordDiff";

/** The file a diff line belongs to: the old one, the new one, or — unchanged — both. */
export type DiffSide = "old" | "new" | "both";

/**
 * Syntax tokens for the lines of one hunk, keyed by line.
 *
 * A hunk is two excerpts of a file interleaved: its unchanged and removed lines are
 * consecutive lines of the old file, its unchanged and added lines of the new one.
 * Each excerpt is tokenized as the run it is, so a block comment or a template
 * literal opened on one line still colours the next, on its own side and no further.
 * An unchanged line keeps the new file's reading. What was open above the hunk is not
 * known, so each side starts clean.
 *
 * The map is empty when the language has no grammar: the caller draws the text plain.
 */
export function highlightDiffLines<Line>(
  language: string | null,
  lines: readonly Line[],
  describe: (line: Line) => { side: DiffSide; text: string }
): Map<Line, CodeToken[]> {
  const tokens = new Map<Line, CodeToken[]>();
  if (grammarFor(language) === null) return tokens;
  const sides: Record<"old" | "new", { line: Line; text: string }[]> = { old: [], new: [] };
  for (const line of lines) {
    const { side, text } = describe(line);
    if (side !== "old") sides.new.push({ line, text });
    if (side !== "new") sides.old.push({ line, text });
  }
  for (const side of [sides.new, sides.old]) {
    const highlighted = highlightCodeLines(language, side.map((entry) => entry.text));
    side.forEach((entry, index) => {
      if (!tokens.has(entry.line)) tokens.set(entry.line, highlighted[index]);
    });
  }
  return tokens;
}

/** A stretch of a line's tokens that a word diff marks, or leaves, as one. */
export interface MarkedTokenRun {
  changed: boolean;
  tokens: CodeToken[];
}

/**
 * Cuts a line's tokens where its word-diff segments begin and end.
 *
 * Both are partitions of the same text, by different rules: a token is a lexical
 * shape, a segment is what changed. A changed run can start inside a string and end
 * past it, so the tokens are split at the segments' edges and regrouped under them —
 * the mark is then one box round everything that changed, with the syntax colours
 * intact inside it.
 */
export function markTokens(
  tokens: readonly CodeToken[],
  segments: readonly WordDiffSegment[]
): MarkedTokenRun[] {
  const runs: MarkedTokenRun[] = [];
  let tokenIndex = 0;
  let tokenOffset = 0;
  for (const segment of segments) {
    const run: MarkedTokenRun = { changed: segment.changed, tokens: [] };
    let remaining = segment.text.length;
    while (remaining > 0 && tokenIndex < tokens.length) {
      const token = tokens[tokenIndex];
      const taken = Math.min(token.value.length - tokenOffset, remaining);
      run.tokens.push({ kind: token.kind, value: token.value.slice(tokenOffset, tokenOffset + taken) });
      remaining -= taken;
      tokenOffset += taken;
      if (tokenOffset === token.value.length) {
        tokenIndex += 1;
        tokenOffset = 0;
      }
    }
    if (run.tokens.length) runs.push(run);
  }
  return runs;
}
