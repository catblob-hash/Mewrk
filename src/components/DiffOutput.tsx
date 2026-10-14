import { type CSSProperties, useMemo } from "react";
import { useI18n } from "../i18n";
import type { CodeToken } from "../lib/codeHighlight";
import { highlightDiffLines } from "../lib/diffHighlight";
import { codeLanguage } from "../lib/fileViewers";
import { wordDiff } from "../lib/wordDiff";
import type { WordDiffSegment } from "../lib/wordDiff";
import { DiffLineTokens } from "./CodeBlock";
import { FileIcon } from "./FileIcon";
import { PathText } from "./PathText";

export type DiffLineKind = "file" | "hunk" | "context" | "addition" | "deletion" | "meta";

export interface DiffOutputProps {
  value: string;
  path?: string;
}

export interface ParsedDiffLine {
  kind: DiffLineKind;
  text: string;
  oldLineNumber: number | null;
  newLineNumber: number | null;
}

export interface ParsedUnifiedDiff {
  lines: ParsedDiffLine[];
  additions: number;
  deletions: number;
  path: string | null;
}

const MAX_RENDERED_LINES = 800;
const HEAD_LINES = 500;
const TAIL_LINES = 250;

function diffPath(header: string): string | null {
  const raw = header.slice(4).split("\t", 1)[0]?.trim();
  if (!raw || raw === "/dev/null") return null;
  return raw.startsWith("a/") || raw.startsWith("b/") ? raw.slice(2) : raw;
}

/** Parses the subset of unified diff syntax needed for a read-only file result view. */
export function parseUnifiedDiff(value: string): ParsedUnifiedDiff {
  const rawLines = value.replace(/\r\n/g, "\n").replace(/\r/g, "\n").split("\n");
  if (rawLines.at(-1) === "") rawLines.pop();

  const lines: ParsedDiffLine[] = [];
  let oldLine = 0;
  let newLine = 0;
  let inHunk = false;
  let additions = 0;
  let deletions = 0;
  let path: string | null = null;

  for (const rawLine of rawLines) {
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$/.exec(rawLine);
    if (hunk) {
      oldLine = Number(hunk[1]);
      newLine = Number(hunk[2]);
      inHunk = true;
      lines.push({ kind: "hunk", text: rawLine, oldLineNumber: null, newLineNumber: null });
      continue;
    }

    if (!inHunk && (rawLine.startsWith("--- ") || rawLine.startsWith("+++ "))) {
      path = diffPath(rawLine) ?? path;
      lines.push({ kind: "file", text: rawLine, oldLineNumber: null, newLineNumber: null });
      continue;
    }

    if (inHunk && rawLine.startsWith("+")) {
      additions += 1;
      lines.push({ kind: "addition", text: rawLine.slice(1), oldLineNumber: null, newLineNumber: newLine });
      newLine += 1;
      continue;
    }

    if (inHunk && rawLine.startsWith("-")) {
      deletions += 1;
      lines.push({ kind: "deletion", text: rawLine.slice(1), oldLineNumber: oldLine, newLineNumber: null });
      oldLine += 1;
      continue;
    }

    if (inHunk && rawLine.startsWith(" ")) {
      lines.push({ kind: "context", text: rawLine.slice(1), oldLineNumber: oldLine, newLineNumber: newLine });
      oldLine += 1;
      newLine += 1;
      continue;
    }

    lines.push({ kind: "meta", text: rawLine, oldLineNumber: null, newLineNumber: null });
  }

  return { lines, additions, deletions, path };
}

function limitLines(lines: ParsedDiffLine[], omittedLabel: (count: number) => string): ParsedDiffLine[] {
  if (lines.length <= MAX_RENDERED_LINES) return lines;
  const omitted = lines.length - HEAD_LINES - TAIL_LINES;
  return [
    ...lines.slice(0, HEAD_LINES),
    {
      kind: "meta",
      text: omittedLabel(omitted),
      oldLineNumber: null,
      newLineNumber: null
    },
    ...lines.slice(-TAIL_LINES)
  ];
}

/** File and hunk headers are diff transport, not content, so the card drops them. */
function isRenderedKind(kind: DiffLineKind): boolean {
  return kind !== "file" && kind !== "hunk";
}

/**
 * The one number a row carries now that both sides share a single column: a
 * deletion exists only in the old file, everything else is addressed in the new
 * one.
 */
function displayedLineNumber(line: ParsedDiffLine): number | null {
  return line.kind === "deletion" ? line.oldLineNumber : line.newLineNumber;
}

/**
 * Reads a replacement as "what was there, then what replaced it": each run of
 * changed lines is regrouped with its deletions first, whatever order the
 * producer emitted them in. Context, hunk, and file rows end a run, so nothing
 * moves across a boundary.
 */
function deletionsBeforeAdditions(lines: ParsedDiffLine[]): ParsedDiffLine[] {
  const ordered: ParsedDiffLine[] = [];
  let run: ParsedDiffLine[] = [];
  const flushRun = () => {
    if (!run.length) return;
    for (const line of run) if (line.kind === "deletion") ordered.push(line);
    for (const line of run) if (line.kind === "addition") ordered.push(line);
    run = [];
  };
  for (const line of lines) {
    if (line.kind === "addition" || line.kind === "deletion") {
      run.push(line);
      continue;
    }
    flushRun();
    ordered.push(line);
  }
  flushRun();
  return ordered;
}

/**
 * Width of the number gutter, in characters of the widest number it has to
 * hold and no more, so the numbers sit against the left edge instead of inside
 * a fixed column sized for a file nobody opened.
 */
function gutterCharacters(lines: ParsedDiffLine[]): number {
  let widest = 0;
  for (const line of lines) {
    const number = displayedLineNumber(line);
    if (number !== null && number > widest) widest = number;
  }
  return String(widest).length;
}

/**
 * Every hunk's lines coloured as the code they are (`diffHighlight.ts`). A hunk or file
 * row ends an excerpt, so nothing left open in one hunk colours the next.
 */
function highlightParsedLines(
  language: string | null,
  lines: readonly ParsedDiffLine[]
): Map<ParsedDiffLine, CodeToken[]> {
  const tokens = new Map<ParsedDiffLine, CodeToken[]>();
  if (language === null) return tokens;
  let hunk: ParsedDiffLine[] = [];
  const flush = () => {
    const highlighted = highlightDiffLines(language, hunk, (line) => ({
      side: line.kind === "addition" ? "new" : line.kind === "deletion" ? "old" : "both",
      text: line.text
    }));
    for (const [line, lineTokens] of highlighted) tokens.set(line, lineTokens);
    hunk = [];
  };
  for (const line of lines) {
    if (line.kind === "context" || line.kind === "addition" || line.kind === "deletion") hunk.push(line);
    else flush();
  }
  flush();
  return tokens;
}

/**
 * The words that changed within a replaced line. A run's removals and additions are
 * paired in order, as the review pane pairs them; a line with no counterpart has nothing
 * to be compared with. `lines` are as drawn, each run's removals first.
 */
function changedWords(lines: readonly ParsedDiffLine[]): Map<ParsedDiffLine, WordDiffSegment[]> {
  const segments = new Map<ParsedDiffLine, WordDiffSegment[]>();
  let index = 0;
  while (index < lines.length) {
    if (lines[index].kind !== "deletion") {
      index += 1;
      continue;
    }
    let additions = index;
    while (additions < lines.length && lines[additions].kind === "deletion") additions += 1;
    let end = additions;
    while (end < lines.length && lines[end].kind === "addition") end += 1;
    const pairs = Math.min(additions - index, end - additions);
    for (let pair = 0; pair < pairs; pair += 1) {
      const result = wordDiff(lines[index + pair].text, lines[additions + pair].text);
      if (result.bailed) continue;
      segments.set(lines[index + pair], result.before);
      segments.set(lines[additions + pair], result.after);
    }
    index = end;
  }
  return segments;
}

export function DiffOutput({
  value,
  path
}: DiffOutputProps) {
  const { t } = useI18n();
  const parsed = useMemo(() => parseUnifiedDiff(value), [value]);
  const renderedLines = useMemo(
    () => limitLines(
      deletionsBeforeAdditions(parsed.lines).filter((line) => isRenderedKind(line.kind)),
      (count) => t("… {count} 行差异未显示 …", "… {count} diff lines omitted …", { count })
    ),
    [parsed.lines, t]
  );
  const gutter = useMemo(() => gutterCharacters(renderedLines), [renderedLines]);
  const language = useMemo(() => codeLanguage(path || parsed.path || ""), [path, parsed.path]);
  const tokens = useMemo(() => highlightParsedLines(language, parsed.lines), [language, parsed.lines]);
  const words = useMemo(() => changedWords(renderedLines), [renderedLines]);
  const filePath = path || parsed.path;
  const displayPath = filePath || t("文件", "File");
  const renderLineNumber = (line: ParsedDiffLine) => (
    <span className="diff-output__line-number" aria-hidden="true">{displayedLineNumber(line) ?? ""}</span>
  );

  return (
    <div
      className="diff-output"
      role="region"
      aria-label={t("{path} 文件差异", "{path} file diff", { path: displayPath })}
      style={{ "--diff-gutter": `${gutter}ch` } as CSSProperties}
    >
      <div className="diff-output__header">
        {filePath && <FileIcon path={filePath} size={13} />}
        <PathText className="diff-output__path" path={displayPath} />
        <span className="diff-output__stats" aria-label={t("新增 {additions} 行，删除 {deletions} 行", "{additions} lines added, {deletions} lines deleted", { additions: parsed.additions, deletions: parsed.deletions })}>
          <b className="diff-output__stat diff-output__stat--addition">+{parsed.additions}</b>
          <b className="diff-output__stat diff-output__stat--deletion">−{parsed.deletions}</b>
        </span>
      </div>
      <div className="diff-output__body">
        {renderedLines.length > 0 ? renderedLines.map((line, index) => (
          <div key={`${index}:${line.kind}`} className={`diff-output__line diff-output__line--${line.kind}`}>
            {renderLineNumber(line)}
            <span className="diff-output__content">
              <DiffLineTokens
                text={line.text}
                tokens={tokens.get(line)}
                segments={words.get(line)}
                wordClassName={`diff-output__word diff-output__word--${line.kind}`}
              />
            </span>
          </div>
        )) : (
          <div className="diff-output__empty">{t("没有行级变化", "No line-level changes")}</div>
        )}
      </div>
    </div>
  );
}
