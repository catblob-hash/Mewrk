/**
 * Word-level alignment of two versions of one line.
 *
 * A line-level diff says a line changed; it does not say what about it changed.
 * Highlighting the differing runs is what turns a wall of red and green into a
 * readable edit. This runs once per paired addition/deletion in a rendered diff,
 * so it refuses long pairs rather than blocking a frame on them.
 */

export interface WordDiffSegment {
  text: string;
  /** True when this run is absent from the other side, or is whitespace joining two such runs. */
  changed: boolean;
}

export interface WordDiffResult {
  before: WordDiffSegment[];
  after: WordDiffSegment[];
  /** True when the pair was too large to align and each side is reported wholly changed. */
  bailed: boolean;
}

export interface WordDiffOptions {
  /** Treat runs of whitespace as equal to each other, so re-indentation is not highlighted. */
  ignoreWhitespace?: boolean;
  /** Above this token count on either side the pair is reported as wholly changed. */
  maxTokens?: number;
}

const DEFAULT_MAX_TOKENS = 400;
/**
 * Ceiling on the alignment table.
 *
 * The table is `before × after` cells, so two lines that each pass the token cap
 * can still multiply into something not worth allocating. 250k cells is a few
 * milliseconds; past that the highlight is not worth what it costs to compute.
 */
const MAX_TABLE_CELLS = 250_000;

const WORD_RUN = /^[\p{L}\p{N}_$]+/u;
const SPACE_RUN = /^\s+/u;

/**
 * Splits a line into the runs the alignment works on.
 *
 * Three kinds: a run of word characters, a run of whitespace, and any other
 * single character on its own. Concatenating the result reproduces the input
 * exactly — nothing is normalised away here, because the segments handed back
 * are the text that gets drawn.
 */
export function tokenizeLine(line: string): string[] {
  const tokens: string[] = [];
  let rest = line;
  while (rest.length > 0) {
    const word = WORD_RUN.exec(rest);
    if (word) {
      tokens.push(word[0]);
      rest = rest.slice(word[0].length);
      continue;
    }
    const space = SPACE_RUN.exec(rest);
    if (space) {
      tokens.push(space[0]);
      rest = rest.slice(space[0].length);
      continue;
    }
    // Take a whole code point, not a UTF-16 unit: half a surrogate pair is not a
    // character, and splitting one would corrupt the text on the way back out.
    const codePoint = String.fromCodePoint(rest.codePointAt(0) ?? 0);
    tokens.push(codePoint);
    rest = rest.slice(codePoint.length);
  }
  return tokens;
}

const WHITESPACE_SENTINEL = "\u0000ws";

function comparisonKey(token: string, ignoreWhitespace: boolean): string {
  if (!ignoreWhitespace) return token;
  return SPACE_RUN.test(token) && token.trim() === "" ? WHITESPACE_SENTINEL : token;
}

function isWhitespace(token: string): boolean {
  return token.trim() === "" && token.length > 0;
}

/** Appends a token, merging into the previous run when it carries the same verdict. */
function push(segments: WordDiffSegment[], text: string, changed: boolean): void {
  if (!text) return;
  const last = segments[segments.length - 1];
  if (last && last.changed === changed) {
    last.text += text;
    return;
  }
  segments.push({ text, changed });
}

function wholeSide(tokens: readonly string[]): WordDiffSegment[] {
  const text = tokens.join("");
  return text ? [{ text, changed: true }] : [];
}

/**
 * Aligns two token arrays by longest common subsequence.
 *
 * LCS rather than Myers: with the table already bounded by the guards above, the
 * quadratic table is affordable, and it has no special cases around an empty side.
 * Returns, for each side, a boolean per token saying whether it is part of the
 * common subsequence.
 */
function alignByLcs(
  before: readonly string[],
  after: readonly string[],
  ignoreWhitespace: boolean
): { beforeKept: boolean[]; afterKept: boolean[] } {
  const rows = before.length;
  const columns = after.length;
  const beforeKeys = before.map((token) => comparisonKey(token, ignoreWhitespace));
  const afterKeys = after.map((token) => comparisonKey(token, ignoreWhitespace));
  // One flat table, row-major, `(rows + 1) × (columns + 1)`.
  const table = new Uint32Array((rows + 1) * (columns + 1));
  for (let row = rows - 1; row >= 0; row -= 1) {
    for (let column = columns - 1; column >= 0; column -= 1) {
      const index = row * (columns + 1) + column;
      table[index] = beforeKeys[row] === afterKeys[column]
        ? table[index + columns + 2] + 1
        : Math.max(table[index + columns + 1], table[index + 1]);
    }
  }
  const beforeKept = new Array<boolean>(rows).fill(false);
  const afterKept = new Array<boolean>(columns).fill(false);
  let row = 0;
  let column = 0;
  while (row < rows && column < columns) {
    const index = row * (columns + 1) + column;
    if (beforeKeys[row] === afterKeys[column]) {
      beforeKept[row] = true;
      afterKept[column] = true;
      row += 1;
      column += 1;
      continue;
    }
    if (table[index + columns + 1] >= table[index + 1]) row += 1;
    else column += 1;
  }
  return { beforeKept, afterKept };
}

export function wordDiff(
  before: string,
  after: string,
  options?: WordDiffOptions
): WordDiffResult {
  const ignoreWhitespace = options?.ignoreWhitespace ?? false;
  const maxTokens = options?.maxTokens ?? DEFAULT_MAX_TOKENS;
  const beforeTokens = tokenizeLine(before);
  const afterTokens = tokenizeLine(after);

  if (before === after) {
    return {
      before: before ? [{ text: before, changed: false }] : [],
      after: after ? [{ text: after, changed: false }] : [],
      bailed: false
    };
  }

  // The common head and tail are almost always most of the line; stripping them
  // first is what keeps the table small enough to be worth allocating at all.
  let head = 0;
  const shortest = Math.min(beforeTokens.length, afterTokens.length);
  while (
    head < shortest
    && comparisonKey(beforeTokens[head], ignoreWhitespace)
      === comparisonKey(afterTokens[head], ignoreWhitespace)
  ) head += 1;
  let tail = 0;
  while (
    tail < shortest - head
    && comparisonKey(beforeTokens[beforeTokens.length - 1 - tail], ignoreWhitespace)
      === comparisonKey(afterTokens[afterTokens.length - 1 - tail], ignoreWhitespace)
  ) tail += 1;

  const beforeMiddle = beforeTokens.slice(head, beforeTokens.length - tail);
  const afterMiddle = afterTokens.slice(head, afterTokens.length - tail);

  if (
    beforeMiddle.length > maxTokens
    || afterMiddle.length > maxTokens
    || (beforeMiddle.length + 1) * (afterMiddle.length + 1) > MAX_TABLE_CELLS
  ) {
    return { before: wholeSide(beforeTokens), after: wholeSide(afterTokens), bailed: true };
  }

  const { beforeKept, afterKept } = alignByLcs(beforeMiddle, afterMiddle, ignoreWhitespace);

  const buildSide = (
    tokens: readonly string[],
    middle: readonly string[],
    kept: readonly boolean[]
  ): WordDiffSegment[] => {
    // Whitespace never carries a change on its own when it is being ignored:
    // marking it would draw a highlight on the very thing the setting hides.
    const changed = middle.map((token, index) => (
      !kept[index] && !(ignoreWhitespace && isWhitespace(token))
    ));
    // Whitespace between two changed runs joins them. A rewritten phrase still has
    // spaces where the old one did, and they are all the alignment finds in common;
    // left unmarked, they cut one edit into a highlight per word.
    for (let index = 1; index < middle.length - 1; index += 1) {
      if (isWhitespace(middle[index]) && changed[index - 1] && changed[index + 1]) {
        changed[index] = true;
      }
    }
    const segments: WordDiffSegment[] = [];
    for (let index = 0; index < head; index += 1) push(segments, tokens[index], false);
    for (let index = 0; index < middle.length; index += 1) {
      push(segments, middle[index], changed[index]);
    }
    for (let index = tokens.length - tail; index < tokens.length; index += 1) {
      push(segments, tokens[index], false);
    }
    return segments;
  };

  return {
    before: buildSide(beforeTokens, beforeMiddle, beforeKept),
    after: buildSide(afterTokens, afterMiddle, afterKept),
    bailed: false
  };
}
