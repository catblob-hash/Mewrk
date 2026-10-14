import {
  Blocks,
  Bot,
  Box,
  CircleAlert,
  CircleHelp,
  ClipboardCheck,
  Code2,
  Crosshair,
  FilePenLine,
  FileSearch,
  FileText,
  Folder,
  FolderTree,
  GitFork,
  Globe,
  Handshake,
  Image,
  Inbox,
  ListChecks,
  Logs,
  MessageCircleMore,
  MonitorPlay,
  MonitorUp,
  MonitorX,
  MousePointerClick,
  Network,
  NotebookPen,
  NotebookText,
  OctagonX,
  Plug,
  ScanSearch,
  Search,
  Server,
  SquareTerminal,
  TextCursorInput,
  Timer,
  Upload,
  Waypoints,
  Workflow,
  Wrench
} from "lucide-react";
import type { LucideIcon } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import type { CSSProperties, ReactNode } from "react";
import { t as globalT, useI18n } from "../i18n";
import { stripAnsi } from "../lib/ansi";
import { highlightCodeLines } from "../lib/codeHighlight";
import { codeLanguage } from "../lib/fileViewers";
import { backendOfTool, shellBackendLabel } from "../lib/machineShells";
import { parseWaitOutput, questionsFromInput } from "../lib/orchestration";
import { webSourceIcon as siteIcon } from "../lib/runtime";
import { agentTimelineRunStatus, initialMessage, isChildMainMessageContext } from "../lib/subagents";
import type { JsonValue, SubagentUpdate, ToolContext, ToolDescriptor } from "../types";
import { CodeTokens } from "./CodeBlock";
import { DiffOutput, parseUnifiedDiff } from "./DiffOutput";
import { FileIcon } from "./FileIcon";
import { ImageStrip } from "./ImageStrip";
import "./ToolRenderers.css";

export type ToolSurface = "group" | "question" | "workflow";

export type ToolViewFamily =
  | "file-list"
  | "grep"
  | "read"
  | "diff"
  | "terminal"
  | "browser-snapshot"
  | "browser-evaluate"
  | "browser-screenshot"
  | "browser-console"
  | "browser-network"
  | "browser-json"
  | "browser-page"
  | "memory"
  | "question"
  | "agent-run"
  | "agent-wait"
  | "agent-note"
  | "web-search"
  | "raw";

/**
 * The bucket one block row is counted in when the block states what it did.
 *
 * `reasoning` and `hook` have no registry entry — they are not tool calls at
 * all — but they share the bucket vocabulary because they share the block.
 */
export type SummaryKind =
  | "commands"
  | "fileChanges"
  | "agents"
  | "mcp"
  | "browser"
  | "fork"
  | "hook"
  | "skill"
  | "memory"
  | "handoff"
  | "search"
  | "reads"
  | "files"
  | "reasoning"
  | "other";
type Translate = ReturnType<typeof useI18n>["t"];
type PresentationResolver = (item: ToolContext, t: Translate) => string | undefined;
type TitleResolver = (t: Translate) => string;

interface ToolViewConfig {
  surface: ToolSurface;
  family: ToolViewFamily;
  icon: LucideIcon;
  doneTitle: TitleResolver;
  runningTitle?: TitleResolver;
  failedTitle?: TitleResolver;
  /** `undefined` falls back to the phase's static title. */
  resolveTitle?: (item: ToolContext, phase: "done" | "running" | "failed", t: Translate) => string | undefined;
  /**
   * A finished call's title that names what it acted on — the file it read or
   * wrote, the pattern it looked for — in place of the phrase. `undefined`
   * (no path, no pattern) keeps the phrase.
   */
  resolveSubject?: (item: ToolContext, t: Translate) => SubjectTitle | undefined;
  /**
   * A tool whose one wire name carries more than one contract resolves its
   * detail family per call (a saved `send_message` from a child is a note, not
   * a run). Ordinary tools leave it unset and use the static field.
   */
  resolveFamily?: (item: ToolContext) => ToolViewFamily;
  target?: PresentationResolver;
  /** The target is a path, drawn so that it gives way in the middle rather than cut at its end. */
  targetIsPath?: boolean;
  stat?: PresentationResolver;
  /**
   * The arguments worth reading, in the order the card should show them.
   *
   * A call carries whatever plumbing its transport needed; the reader only wants
   * the few that say what it did. Naming them per tool keeps that judgement where
   * every other judgement about the tool already lives. Omitted means "show them
   * all" — the honest answer for a tool nobody has curated, which is why MCP and
   * unknown tools fall through to it.
   */
  keys?: readonly string[];
  summaryKind: SummaryKind;
}

/**
 * A title that names something, in parts: the words around the subject and the
 * subject itself, so a row can set it apart — a file to open, a pattern to read
 * as code. The parts joined are the title.
 */
export interface TitleSubject {
  before: string;
  text: string;
  after: string;
  /** The subject is this file, as the call named it: a click opens it in the file pane. */
  path?: string;
  /** Where in the file the call started reading. */
  line?: number;
  /** The subject is a pattern or a query, set in the code face. */
  code?: boolean;
}

interface SubjectTitle {
  subject: TitleSubject;
  /** Lines the call added and removed. */
  counts?: { additions: number; deletions: number };
  /** What it found: how many files or matches. */
  note?: string;
}

export interface ToolPresentation {
  surface: ToolSurface;
  family: ToolViewFamily;
  icon: LucideIcon;
  title: string;
  /** See `TitleSubject`; absent, the title is a phrase with nothing to set apart. */
  subject?: TitleSubject;
  /**
   * Lines a file change added and removed, from the diff the host made of the
   * file when it wrote it — not Git's: the call's own change, whatever else the
   * working tree holds.
   */
  counts?: { additions: number; deletions: number };
  /** A figure the title carries after its subject: "12 matches". */
  note?: string;
  target?: string;
  /** See `ToolViewConfig.targetIsPath`. */
  targetIsPath?: boolean;
  stat?: string;
  keys?: readonly string[];
}

interface ParsedJson {
  parsed: boolean;
  value: unknown;
}

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function inputString(item: ToolContext, name: string): string | undefined {
  const value = item.input[name];
  return typeof value === "string" && value.trim() ? value.trim() : undefined;
}

function inputNumber(item: ToolContext, name: string): number | undefined {
  const value = item.input[name];
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function inputFlag(item: ToolContext, name: string): boolean {
  return item.input[name] === true;
}

/** `web_fetch` titles show the first URL and summarize later URLs as `+N`. */
function firstUrl(item: ToolContext): string | undefined {
  const urls = item.input.urls;
  if (!Array.isArray(urls)) return undefined;
  const first = urls.find((url): url is string => typeof url === "string" && Boolean(url.trim()));
  if (!first) return undefined;
  const rest = urls.length - 1;
  return rest > 0 ? `${first.trim()} +${rest}` : first.trim();
}

function compact(value: string | undefined, limit = 108): string | undefined {
  if (!value) return undefined;
  const normalized = value.replace(/\s+/g, " ").trim();
  if (!normalized) return undefined;
  return normalized.length > limit ? `${normalized.slice(0, limit - 1).trimEnd()}…` : normalized;
}

/** A path target, whole: the surface that draws it shortens it in the middle (`PathText`). */
function pathTarget(value: string | undefined): string | undefined {
  return compact(value, Number.POSITIVE_INFINITY);
}

function targetSelector(item: ToolContext): string | undefined {
  const uid = inputNumber(item, "uid") ?? inputString(item, "uid");
  return compact(uid === undefined ? inputString(item, "selector") ?? inputString(item, "ref") : `uid ${uid}`);
}

/**
 * The one line a call with no registry entry can show about itself. Arguments
 * arrive in the order the schema declares them, so the first scalar is the one
 * the tool's author put first.
 */
function firstArgumentSummary(item: ToolContext): string | undefined {
  for (const value of Object.values(item.input)) {
    if (typeof value === "string") {
      const text = compact(value);
      if (text) return text;
      continue;
    }
    if (typeof value === "number" || typeof value === "boolean") return String(value);
  }
  return undefined;
}

function outputLines(value: string): string[] {
  return value.replace(/\r\n?/g, "\n").split("\n").filter((line, index, lines) => line || index < lines.length - 1);
}

function meaningfulCount(value: string, emptyMessages: string[]): number {
  const trimmed = value.trim();
  if (!trimmed || emptyMessages.includes(trimmed)) return 0;
  return outputLines(trimmed).filter((line) => line.trim() && !line.startsWith("… 已达到")).length; // i18n-audit-ignore: parses a backend truncation sentinel
}

function parseJsonOutput(item: ToolContext): ParsedJson {
  const value = item.result.output.trim();
  if (!value) return { parsed: false, value: null };
  try {
    return { parsed: true, value: JSON.parse(value) as unknown };
  } catch {
    return { parsed: false, value: null };
  }
}

function parsedRecord(item: ToolContext): Record<string, unknown> | null {
  const parsed = parseJsonOutput(item);
  return parsed.parsed ? object(parsed.value) : null;
}

function arrayLength(value: unknown): number | undefined {
  return Array.isArray(value) ? value.length : undefined;
}

function numberValue(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function browserDuration(item: ToolContext): string | undefined {
  return item.result.durationMs > 0 ? `${item.result.durationMs} ms` : undefined;
}

/**
 * What the file tools answer with when they found nothing: the built-in
 * profile's sentences, and the Chinese ones older transcripts carry. They are
 * the tool's words, not an entry it found.
 */
const FILE_LIST_EMPTY_ANSWERS = ["(empty directory)", "No matching files", "（目录为空）", "未找到匹配文件"]; // i18n-audit-ignore: parses backend sentinels
const GREP_EMPTY_ANSWERS = ["No matches found", "未找到匹配内容"]; // i18n-audit-ignore: parses backend sentinels
/** A page asked for past the last entry: the tool's answer, not an entry. */
const GREP_PAST_END = /^No matches at offset \d+; there are \d+\.$/;
/**
 * What `read` says in place of text: an empty file, a start past the last line.
 * They are the tool's sentences, not lines of the file.
 */
const READ_EMPTY_FILE = "(The file exists but is empty)";
const READ_PAST_END = /^\(The file has only \d+ lines?, so the requested start_line is out of range\)$/;
/**
 * The note `read` ends with when it decoded the bytes for the model: lossily, or from
 * UTF-16. It follows the text on a line of its own, like the continuation note.
 */
const READ_DECODE_NOTE = /^\(This file is (?:not valid UTF-8|UTF-16(?:LE|BE) text), .*\)$/;

function fileListStat(item: ToolContext, t: Translate): string {
  const count = meaningfulCount(item.result.output, FILE_LIST_EMPTY_ANSWERS);
  return t("{count} 项", "{count} items", { count });
}

function grepStat(item: ToolContext, t: Translate): string {
  const { mode, count } = grepCount(item);
  return mode === "content"
    ? t("{count} 处匹配", "{count} matches", { count })
    : t("{count} 个文件", "{count} files", { count });
}

/** Past this many lines a `read` is shown uncoloured, as a file is (`CodeBlock.tsx`). */
const READ_HIGHLIGHT_LINES = 8000;

/** A line of `read` output that is the tool talking about the text rather than the text. */
function isReadNote(line: string): boolean {
  return line.startsWith("… ") || READ_DECODE_NOTE.test(line.trim());
}

/**
 * The lines of the file a `read` returned. An empty file and a start past the end
 * come back as a sentence, and the notes trail the text; none of them is a line.
 */
function readTextLines(output: string): string[] {
  const lines = outputLines(output);
  if (lines.length === 1 && (lines[0].trim() === READ_EMPTY_FILE || READ_PAST_END.test(lines[0].trim()))) return [];
  return lines.filter((line) => !isReadNote(line));
}

function readStat(item: ToolContext, t: Translate): string | undefined {
  const images = item.result.images ?? [];
  if (images.length === 1) return `${images[0].width}×${images[0].height}`;
  if (images.length > 1) {
    return t("{count} 张图片", "{count} images", { count: images.length });
  }
  const start = Math.max(1, Math.floor(inputNumber(item, "start_line") ?? 1));
  const requestedEnd = inputNumber(item, "end_line");
  const lineCount = readTextLines(item.result.output).length;
  const end = requestedEnd === undefined ? (lineCount ? start + lineCount - 1 : start) : Math.floor(requestedEnd);
  return lineCount ? t("{start}–{end} 行", "{start}–{end} lines", { start, end: Math.max(start, end) }) : undefined;
}

function diffStat(item: ToolContext): string | undefined {
  if (!item.result.diff) return undefined;
  const parsed = parseUnifiedDiff(item.result.diff);
  return `+${parsed.additions} −${parsed.deletions}`;
}

function writeTitle(item: ToolContext, phase: "done" | "running" | "failed", t: Translate): string {
  if (phase === "running") return t("正在写入文件", "Writing file");
  if (phase === "failed") return t("写入文件失败", "Failed to write file");
  return item.result.diff?.replace(/\r\n?/g, "\n").split("\n").some((line) => line === "--- /dev/null")
    ? t("创建了文件", "Created file")
    : t("写入了文件", "Wrote file");
}

/** Stands where the subject goes in a translated title, so the parts around it can be found. */
const SUBJECT_MARK = "\u0000";

/**
 * `template` is a title translated with `SUBJECT_MARK` for its subject; the
 * subject goes where the translation put it.
 */
function subjectTitle(
  template: string,
  text: string,
  extra: Omit<TitleSubject, "before" | "text" | "after"> = {}
): TitleSubject {
  const cut = template.indexOf(SUBJECT_MARK);
  return cut < 0
    ? { before: template, text, after: "", ...extra }
    : { before: template.slice(0, cut), text, after: template.slice(cut + SUBJECT_MARK.length), ...extra };
}

/** A path's last name: what a row calls the file. */
function baseName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const cut = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return trimmed.slice(cut + 1) || trimmed || path;
}

function diffCounts(item: ToolContext): SubjectTitle["counts"] {
  if (!item.result.diff) return undefined;
  const parsed = parseUnifiedDiff(item.result.diff);
  return { additions: parsed.additions, deletions: parsed.deletions };
}

function readSubject(item: ToolContext, t: Translate): SubjectTitle | undefined {
  const path = inputString(item, "path");
  if (!path) return undefined;
  const start = Math.floor(inputNumber(item, "start_line") ?? 1);
  return {
    subject: subjectTitle(t("已读取：{name}", "Read {name}", { name: SUBJECT_MARK }), baseName(path), {
      path,
      ...(start > 1 ? { line: start } : {})
    })
  };
}

function editSubject(item: ToolContext, t: Translate): SubjectTitle | undefined {
  const path = inputString(item, "path");
  if (!path) return undefined;
  const counts = diffCounts(item);
  return {
    subject: subjectTitle(t("已编辑：{name}", "Edited {name}", { name: SUBJECT_MARK }), baseName(path), { path }),
    ...(counts ? { counts } : {})
  };
}

function writeSubject(item: ToolContext, t: Translate): SubjectTitle | undefined {
  const path = inputString(item, "path");
  if (!path) return undefined;
  const created = item.result.diff?.replace(/\r\n?/g, "\n").split("\n").some((line) => line === "--- /dev/null");
  const template = created
    ? t("已创建：{name}", "Created {name}", { name: SUBJECT_MARK })
    : t("已写入：{name}", "Wrote {name}", { name: SUBJECT_MARK });
  const counts = diffCounts(item);
  return { subject: subjectTitle(template, baseName(path), { path }), ...(counts ? { counts } : {}) };
}

/**
 * The files `find` listed, past its notes. A count the host had to stop short
 * of reads as a floor: the list shows the first of more matches.
 */
function findCount(item: ToolContext): { count: number; more: boolean } {
  const lines = outputLines(item.result.output).map((line) => line.trim()).filter(Boolean);
  const entries = lines.filter((line) => !line.startsWith("(") && !line.startsWith("… ") && !FILE_LIST_EMPTY_ANSWERS.includes(line));
  return { count: entries.length, more: lines.some((line) => /^\(Showing \d+ of \d+/.test(line)) };
}

/**
 * What `grep` returned in the unit its mode counts — files, matching lines, or files with
 * their counts — past its notes, its context lines and the entries it could not read.
 */
function grepCount(item: ToolContext): { mode: GrepMode; count: number; more: boolean } {
  const { mode, count, more } = parseGrepOutput(item);
  return { mode, count, more };
}

function findSubject(item: ToolContext, t: Translate): SubjectTitle | undefined {
  const query = compact(inputString(item, "query"));
  if (!query) return undefined;
  const { count, more } = findCount(item);
  return {
    subject: subjectTitle(t("已查找：{query}", "Found {query}", { query: SUBJECT_MARK }), query, { code: true }),
    note: more
      ? t("{count}+ 个结果", "{count}+ results", { count })
      : t("{count} 个结果", "{count} results", { count })
  };
}

function grepSubject(item: ToolContext, t: Translate): SubjectTitle | undefined {
  const pattern = compact(inputString(item, "pattern"));
  if (!pattern) return undefined;
  const { mode, count, more } = grepCount(item);
  // The default mode answers with files, so a figure of "matches" would count the wrong thing.
  const note = mode === "content"
    ? more
      ? t("{count}+ 处匹配", "{count}+ matches", { count })
      : t("{count} 处匹配", "{count} matches", { count })
    : more
      ? t("{count}+ 个文件", "{count}+ files", { count })
      : t("{count} 个文件", "{count} files", { count });
  return {
    subject: subjectTitle(t("已搜索：{pattern}", "Searched for {pattern}", { pattern: SUBJECT_MARK }), pattern, { code: true }),
    note
  };
}

/**
 * The pattern a search looked for, then what narrowed it: the files it was limited to
 * and a mode other than the default. It is what the card and the waiting line name the call by.
 */
function grepTarget(item: ToolContext, t: Translate): string | undefined {
  const pattern = compact(inputString(item, "pattern"));
  if (!pattern) return undefined;
  const mode = inputString(item, "output_mode");
  return [
    pattern,
    compact(inputString(item, "glob"), 48),
    mode === "content" ? t("内容", "content") : mode === "count" ? t("计数", "count") : undefined
  ].filter(Boolean).join(" · ");
}

/** How much of an error's line a title carries; the row ellipsizes what does not fit. */
const ERROR_EXCERPT_LIMIT = 240;

/**
 * The line of a failed call's output that says what went wrong: its first, or
 * for a command the first after the `Exit code N` it leads with, which says
 * only that it failed.
 */
export function errorExcerpt(output: string): string | undefined {
  const lines = stripAnsi(output).split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  const line = lines.length > 1 && /^Exit code \S+$/.test(lines[0]!) ? lines[1] : lines[0];
  if (!line) return undefined;
  const flat = line.replace(/\s+/g, " ");
  return flat.length > ERROR_EXCERPT_LIMIT ? `${flat.slice(0, ERROR_EXCERPT_LIMIT - 1).trimEnd()}…` : flat;
}

/** A failed call's title: what failed, then why — the local model's reason, or the error's own line. */
function failureTitle(phrase: string, reason: string | undefined, t: Translate): string {
  return reason ? t("{title}：{reason}", "{title}: {reason}", { title: phrase, reason }) : phrase;
}

/** Non-empty output lines, which is how every text-answering preview tool reports its size. */
function outputLineCount(item: ToolContext): number {
  return outputLines(item.result.output).filter((line) => line.trim()).length;
}

/**
 * Text-answering preview tools word their own empty case, and those sentences are
 * the source's — they must not be counted as one entry.
 */
const PREVIEW_EMPTY_ANSWERS = [
  "No console logs.",
  "No network requests recorded.",
  "No failed requests.",
  "No logs yet.",
  "No output yet."
];

function previewEntryStat(item: ToolContext, t: Translate): string | undefined {
  const trimmed = item.result.output.trim();
  if (!trimmed) return browserDuration(item);
  const count = PREVIEW_EMPTY_ANSWERS.includes(trimmed) ? 0 : outputLineCount(item);
  return t("{count} 条", "{count} entries", { count });
}

function screenshotStat(item: ToolContext): string | undefined {
  const result = parsedRecord(item);
  const width = numberValue(result?.width);
  const height = numberValue(result?.height);
  return width !== undefined && height !== undefined ? `${width}×${height}` : browserDuration(item);
}

function uploadImageStat(item: ToolContext): string | undefined {
  const raw = item.input.image_id;
  const reference = typeof raw === "number" ? String(raw) : typeof raw === "string" ? raw.trim() : "";
  const match = reference.match(/^(?:\[Image #|#)?([0-9]{1,9})\]?$/);
  return match ? `#${match[1]}` : browserDuration(item);
}

/** `preview_resize` reports the size it was asked for; a preset carries no numbers. */
function resizeStat(item: ToolContext): string | undefined {
  const width = inputNumber(item, "width");
  const height = inputNumber(item, "height");
  return width !== undefined && height !== undefined ? `${width}×${height}` : browserDuration(item);
}

/**
 * What a host notice card says it is, by the kind its `notice` names. Mirrors
 * Rust `handoff::NOTICE_KIND` and `wire_history::notice_kind`; a delivered
 * background result names no kind and keeps the default title.
 */
const HOST_NOTICE_TITLES: Record<string, (t: Translate) => string> = {
  handoff: (t) => t("上下文已达自动压缩阈值，请求交接", "Context reached the auto-compact threshold; handoff requested"),
  handoff_index: (t) => t("送达了交接文档索引", "Delivered the handoff notes index"),
  plan_mode: (t) => t("送达了计划模式引导", "Delivered the plan mode guidance"),
  plan_mode_exit: (t) => t("送达了计划模式结束说明", "Delivered the plan mode exit note"),
  structured_output: (t) => t("提醒通过 structured_output 返回结果", "Reminded to return the result through structured_output"),
  hook_context: (t) => t("送达了钩子补充的上下文", "Delivered context from a hook"),
  skill_added: (t) => t("送达了新增的技能", "Delivered a newly added skill"),
  diagnostics: (t) => t("送达了语言服务器诊断", "Delivered language-server diagnostics"),
  file_changes: (t) => t("送达了文件变更通知", "Delivered a file-change notice"),
  mcp_unavailable: (t) => t("本轮有 MCP 服务器无法使用", "MCP servers unavailable this turn"),
  instruction_skips: (t) => t("跳过了部分指令文件", "Skipped some instruction files"),
  preview_start_failed: (t) => t("送达了开发服务器启动失败的通知", "Delivered a dev server start failure")
};

/**
 * A card written before cards held the whole message named its kind in its
 * input (`notification.kind`); those keep their titles.
 */
function legacyNoticeKind(item: ToolContext): string | undefined {
  const notification = item.input.notification;
  if (typeof notification !== "object" || notification === null || Array.isArray(notification)) return undefined;
  return typeof notification.kind === "string" ? notification.kind : undefined;
}

function hostNoticeTitle(item: ToolContext, t: Translate): string | undefined {
  const kind = item.notice ?? legacyNoticeKind(item);
  return kind ? HOST_NOTICE_TITLES[kind]?.(t) : undefined;
}

const HOST_MESSAGE_VIEW = {
  surface: "group",
  family: "raw",
  icon: Inbox,
  doneTitle: (t) => t("送达了后台结果", "Delivered a background result"),
  resolveTitle: (item, _phase, t) => hostNoticeTitle(item, t),
  keys: [],
  summaryKind: "agents"
} as const satisfies ToolViewConfig;

/**
 * Memory tools take a plain document name and return plain Markdown, so the
 * card shows only that name. There is no owner, scope, version or byte count
 * to project any more: a memory belongs to a location, not to a model.
 */
function memoryTarget(item: ToolContext): string | undefined {
  return compact(inputString(item, "name"), 160);
}

/**
 * How many task envelopes one `task_wait` call actually drained. A wait that
 * returned nothing is a different event from one that collected six results,
 * and the row is the only place that difference shows without expanding.
 *
 * Counted neutrally rather than as "updates": wait blocks through progress to
 * the terminal result, so a normal return carries the result *plus* however many
 * updates piled up behind it. Calling that pile "updates" would mislabel the
 * one envelope the model was actually waiting on.
 */
function waitStat(item: ToolContext, t: Translate): string | undefined {
  if (isLiveCall(item) || !item.result.success) return undefined;
  const count = parseWaitOutput(item.result.output).envelopes.length;
  return count ? t("收到 {count} 条", "{count} collected", { count }) : undefined;
}

/**
 * One site a web call consulted, as the card shows it.
 *
 * `host` is what the chip reads, because a row of page titles is a wall of text
 * while a row of domains is scannable — and the domain is what tells a reader
 * whether the answer came from vendor documentation or a forum.
 */
interface WebSourceChip {
  host: string;
  url: string;
  title?: string;
}

/**
 * The sites a `web_search` or `web_fetch` result names, deduplicated by host.
 *
 * The two backends answer in different shapes and both are load-bearing: a
 * provider-native search returns `{findings, sources}` because its page text is
 * sealed in the provider's own encrypted payload and only the URLs are
 * host-readable, while a catalog provider and every fetch return
 * `{results:[{title,url,content}]}` with real text. Reading both here is what
 * lets one card serve every backend.
 */
function webSourceChips(item: ToolContext): WebSourceChip[] {
  const parsed = parseJsonOutput(item);
  const record = object(parsed.value);
  const entries = [
    ...(Array.isArray(record?.sources) ? record.sources : []),
    ...(Array.isArray(record?.results) ? record.results : [])
  ];
  const chips: WebSourceChip[] = [];
  const seen = new Set<string>();
  for (const entry of entries) {
    const fields = object(entry);
    const url = typeof fields?.url === "string" ? fields.url.trim() : "";
    if (!url) continue;
    let host = "";
    try {
      const parsedUrl = new URL(url);
      if (parsedUrl.protocol !== "http:" && parsedUrl.protocol !== "https:") continue;
      // `www.` is noise on a chip: it never distinguishes two sources and it
      // costs four characters of a very narrow row.
      host = parsedUrl.hostname.replace(/^www\./, "");
    } catch {
      continue;
    }
    if (!host || seen.has(host)) continue;
    seen.add(host);
    const title = typeof fields?.title === "string" && fields.title.trim()
      ? fields.title.trim()
      : undefined;
    chips.push({ host, url, title });
  }
  return chips;
}

/** The collapsed row of a settled `web_search`: how many sites it read. */
function webSearchTitle(
  item: ToolContext,
  phase: "done" | "running" | "failed",
  t: Translate
): string | undefined {
  if (phase !== "done") return undefined;
  const count = webSourceChips(item).length;
  // A search that reports no sources still ran; falling back to the plain title
  // is more honest than announcing zero sites.
  return count ? t("已搜索 {count} 个网站", "Searched {count} sites", { count }) : undefined;
}

/**
 * Every built-in tool is intentionally named here. Similar tools share a
 * renderer family, but no tool silently falls into a category-wide default.
 */
export const TOOL_VIEW_REGISTRY = {
  ls: { surface: "group", family: "file-list", icon: FolderTree, doneTitle: (t) => t("查看了目录", "Viewed directory"), runningTitle: (t) => t("正在查看目录", "Viewing directory"), failedTitle: (t) => t("查看目录失败", "Failed to view directory"), target: (item) => pathTarget(inputString(item, "path") ?? "."), targetIsPath: true, stat: fileListStat, summaryKind: "files" },
  grep: { surface: "group", family: "grep", icon: Search, doneTitle: (t) => t("搜索了文件内容", "Searched file contents"), resolveSubject: grepSubject, runningTitle: (t) => t("正在搜索文件内容", "Searching file contents"), failedTitle: (t) => t("搜索文件内容失败", "File-content search failed"), target: grepTarget, stat: grepStat, summaryKind: "search" },
  pwsh: { surface: "group", family: "terminal", icon: SquareTerminal, doneTitle: (t) => t("运行了 PowerShell 7 命令", "Ran PowerShell 7 command"), runningTitle: (t) => t("正在运行 PowerShell 7 命令", "Running PowerShell 7 command"), failedTitle: (t) => t("PowerShell 7 命令失败", "PowerShell 7 command failed"), target: (item) => compact(inputString(item, "command")), stat: browserDuration, summaryKind: "commands" },
  powershell: { surface: "group", family: "terminal", icon: SquareTerminal, doneTitle: (t) => t("运行了 Windows PowerShell 命令", "Ran Windows PowerShell command"), runningTitle: (t) => t("正在运行 Windows PowerShell 命令", "Running Windows PowerShell command"), failedTitle: (t) => t("Windows PowerShell 命令失败", "Windows PowerShell command failed"), target: (item) => compact(inputString(item, "command")), stat: browserDuration, summaryKind: "commands" },
  bash: { surface: "group", family: "terminal", icon: SquareTerminal, doneTitle: (t) => t("运行了 Bash 命令", "Ran Bash command"), runningTitle: (t) => t("正在运行 Bash 命令", "Running Bash command"), failedTitle: (t) => t("Bash 命令失败", "Bash command failed"), target: (item) => compact(inputString(item, "command")), stat: browserDuration, summaryKind: "commands" },
  zsh: { surface: "group", family: "terminal", icon: SquareTerminal, doneTitle: (t) => t("运行了 zsh 命令", "Ran zsh command"), runningTitle: (t) => t("正在运行 zsh 命令", "Running zsh command"), failedTitle: (t) => t("zsh 命令失败", "zsh command failed"), target: (item) => compact(inputString(item, "command")), stat: browserDuration, summaryKind: "commands" },
  sh: { surface: "group", family: "terminal", icon: SquareTerminal, doneTitle: (t) => t("运行了 sh 命令", "Ran sh command"), runningTitle: (t) => t("正在运行 sh 命令", "Running sh command"), failedTitle: (t) => t("sh 命令失败", "sh command failed"), target: (item) => compact(inputString(item, "command")), stat: browserDuration, summaryKind: "commands" },
  write: { surface: "group", family: "diff", icon: FilePenLine, doneTitle: (t) => t("写入了文件", "Wrote file"), resolveTitle: writeTitle, resolveSubject: writeSubject, target: (item) => pathTarget(inputString(item, "path")), targetIsPath: true, stat: diffStat, summaryKind: "fileChanges" },
  edit: { surface: "group", family: "diff", icon: FilePenLine, doneTitle: (t) => t("编辑了文件", "Edited file"), resolveSubject: editSubject, runningTitle: (t) => t("正在编辑文件", "Editing file"), failedTitle: (t) => t("编辑文件失败", "Failed to edit file"), target: (item) => pathTarget(inputString(item, "path")), targetIsPath: true, stat: diffStat, summaryKind: "fileChanges" },
  find: { surface: "group", family: "file-list", icon: FileSearch, doneTitle: (t) => t("查找了文件", "Found files"), resolveSubject: findSubject, runningTitle: (t) => t("正在查找文件", "Finding files"), failedTitle: (t) => t("查找文件失败", "Failed to find files"), target: (item) => compact(inputString(item, "query")), stat: fileListStat, summaryKind: "files" },
  read: { surface: "group", family: "read", icon: FileText, doneTitle: (t) => t("读取了文件", "Read file"), resolveSubject: readSubject, runningTitle: (t) => t("正在读取文件", "Reading file"), failedTitle: (t) => t("读取文件失败", "Failed to read file"), target: (item) => pathTarget(inputString(item, "path")), targetIsPath: true, stat: readStat, summaryKind: "reads" },
  lsp: { surface: "group", family: "raw", icon: Waypoints, doneTitle: (t) => t("查询了代码语义", "Queried code semantics"), runningTitle: (t) => t("正在查询代码语义", "Querying code semantics"), failedTitle: (t) => t("查询代码语义失败", "Code semantics query failed"), target: (item) => pathTarget(inputString(item, "filePath") ?? ""), targetIsPath: true, keys: ["operation", "filePath", "line", "character", "query"], summaryKind: "search" },

  workflow: { surface: "workflow", family: "raw", icon: Workflow, doneTitle: (t) => t("完成了工作流", "Completed workflow"), runningTitle: (t) => t("正在运行工作流", "Running workflow"), failedTitle: (t) => t("工作流运行失败", "Workflow failed"), keys: ["name"], summaryKind: "agents" },
  workflow_step: { surface: "group", family: "agent-run", icon: Bot, doneTitle: (t) => t("完成了工作流步骤", "Completed workflow step"), runningTitle: (t) => t("正在运行工作流步骤", "Running workflow step"), failedTitle: (t) => t("工作流步骤失败", "Workflow step failed"), target: (item) => compact(inputString(item, "label") ?? inputString(item, "name")), summaryKind: "agents" },

  // A search names the sites it read, so the card collapses to that count and
  // opens onto one chip per site. Fetching is told what to read, so it keeps the
  // ordinary card: the URLs are already in the call's own arguments.
  web_search: { surface: "group", family: "web-search", icon: Search, doneTitle: (t) => t("完成了联网搜索", "Completed web search"), runningTitle: (t) => t("正在联网搜索", "Searching the web"), failedTitle: (t) => t("联网搜索失败", "Web search failed"), resolveTitle: webSearchTitle, target: (item) => compact(inputString(item, "query")), keys: ["query"], summaryKind: "search" },
  web_fetch: { surface: "group", family: "raw", icon: Globe, doneTitle: (t) => t("抓取了网页", "Fetched pages"), runningTitle: (t) => t("正在抓取网页", "Fetching pages"), failedTitle: (t) => t("抓取网页失败", "Failed to fetch pages"), target: (item) => compact(firstUrl(item)), keys: ["urls"], summaryKind: "search" },

  // Fifteen flat preview tools, each its own wire tool. Thirteen are copied from
  // Claude Code and answer in prose the tool wrote itself, so most of them render
  // that text as-is; only the calls that answer with JSON or pixels get a
  // structured family.
  preview_start: { surface: "group", family: "raw", icon: MonitorPlay, doneTitle: (t) => t("启动了预览服务器", "Started preview server"), runningTitle: (t) => t("正在启动预览服务器", "Starting preview server"), failedTitle: (t) => t("启动预览服务器失败", "Failed to start preview server"), target: (item) => compact(inputString(item, "name")), stat: browserDuration, keys: ["name"], summaryKind: "browser" },
  preview_stop: { surface: "group", family: "raw", icon: MonitorX, doneTitle: (t) => t("停止了预览服务器", "Stopped preview server"), runningTitle: (t) => t("正在停止预览服务器", "Stopping preview server"), failedTitle: (t) => t("停止预览服务器失败", "Failed to stop preview server"), target: (item) => compact(inputString(item, "serverId")), stat: browserDuration, keys: ["serverId"], summaryKind: "browser" },
  preview_list: { surface: "group", family: "browser-json", icon: Server, doneTitle: (t) => t("列出了预览服务器", "Listed preview servers"), runningTitle: (t) => t("正在列出预览服务器", "Listing preview servers"), failedTitle: (t) => t("列出预览服务器失败", "Failed to list preview servers"), keys: [], stat: (item, t) => {
    const count = arrayLength(parseJsonOutput(item).value);
    return count === undefined ? browserDuration(item) : t("{count} 个服务器", "{count} servers", { count });
  }, summaryKind: "browser" },
  preview_logs: { surface: "group", family: "raw", icon: Logs, doneTitle: (t) => t("读取了服务器日志", "Read server logs"), runningTitle: (t) => t("正在读取服务器日志", "Reading server logs"), failedTitle: (t) => t("读取服务器日志失败", "Failed to read server logs"), target: (item) => compact(inputString(item, "search")), stat: previewEntryStat, keys: ["search", "level", "lines"], summaryKind: "browser" },
  preview_console_logs: { surface: "group", family: "browser-console", icon: Logs, doneTitle: (t) => t("读取了 Console 日志", "Read Console logs"), runningTitle: (t) => t("正在读取 Console 日志", "Reading Console logs"), failedTitle: (t) => t("读取 Console 日志失败", "Failed to read Console logs"), target: (item) => compact(inputString(item, "level")), stat: previewEntryStat, summaryKind: "browser" },
  preview_screenshot: { surface: "group", family: "browser-screenshot", icon: Image, doneTitle: (t) => t("截取了页面", "Captured page"), runningTitle: (t) => t("正在截取页面", "Capturing page"), failedTitle: (t) => t("页面截图失败", "Page capture failed"), stat: screenshotStat, summaryKind: "browser" },
  preview_snapshot: { surface: "group", family: "browser-snapshot", icon: ScanSearch, doneTitle: (t) => t("读取了页面快照", "Read page snapshot"), runningTitle: (t) => t("正在读取页面快照", "Reading page snapshot"), failedTitle: (t) => t("读取页面快照失败", "Failed to read page snapshot"), stat: (item, t) => {
    const count = outputLineCount(item);
    return count ? t("{count} 个节点", "{count} nodes", { count }) : browserDuration(item);
  }, summaryKind: "browser" },
  preview_inspect: { surface: "group", family: "browser-json", icon: Crosshair, doneTitle: (t) => t("检查了页面元素", "Inspected page element"), runningTitle: (t) => t("正在检查页面元素", "Inspecting page element"), failedTitle: (t) => t("检查页面元素失败", "Failed to inspect page element"), target: targetSelector, stat: browserDuration, keys: ["selector", "styles"], summaryKind: "browser" },
  preview_click: { surface: "group", family: "raw", icon: MousePointerClick, doneTitle: (t) => t("点击了页面元素", "Clicked page element"), runningTitle: (t) => t("正在点击页面元素", "Clicking page element"), failedTitle: (t) => t("点击页面元素失败", "Failed to click page element"), target: targetSelector, keys: ["selector", "uid", "doubleClick"], stat: (item, t) => (inputFlag(item, "doubleClick") ? t("双击", "Double-click") : browserDuration(item)), summaryKind: "browser" },
  preview_fill: { surface: "group", family: "raw", icon: TextCursorInput, doneTitle: (t) => t("填写了页面输入", "Filled page input"), runningTitle: (t) => t("正在填写页面输入", "Filling page input"), failedTitle: (t) => t("填写页面输入失败", "Failed to fill page input"), target: targetSelector, keys: ["selector", "uid", "value"], stat: (item, t) => {
    const value = inputString(item, "value");
    return value === undefined ? browserDuration(item) : t("{count} 字", "{count} characters", { count: value.length });
  }, summaryKind: "browser" },
  preview_eval: { surface: "group", family: "browser-evaluate", icon: Code2, doneTitle: (t) => t("执行了页面脚本", "Ran page script"), runningTitle: (t) => t("正在执行页面脚本", "Running page script"), failedTitle: (t) => t("页面脚本执行失败", "Page script failed"), target: (item) => compact(inputString(item, "expression")), stat: browserDuration, summaryKind: "browser" },
  preview_network: { surface: "group", family: "browser-network", icon: Network, doneTitle: (t) => t("读取了网络请求", "Read network requests"), runningTitle: (t) => t("正在读取网络请求", "Reading network requests"), failedTitle: (t) => t("读取网络请求失败", "Failed to read network requests"), target: (item) => compact(inputString(item, "requestId") ?? inputString(item, "filter")), stat: previewEntryStat, summaryKind: "browser" },
  preview_resize: { surface: "group", family: "raw", icon: MonitorUp, doneTitle: (t) => t("调整了预览视口", "Resized preview viewport"), runningTitle: (t) => t("正在调整预览视口", "Resizing preview viewport"), failedTitle: (t) => t("调整预览视口失败", "Failed to resize preview viewport"), target: (item) => compact(inputString(item, "preset") ?? inputString(item, "colorScheme")), stat: resizeStat, keys: ["preset", "width", "height", "colorScheme"], summaryKind: "browser" },
  preview_upload_image: { surface: "group", family: "browser-page", icon: Upload, doneTitle: (t) => t("上传了对话图片", "Uploaded conversation image"), runningTitle: (t) => t("正在上传对话图片", "Uploading conversation image"), failedTitle: (t) => t("上传对话图片失败", "Failed to upload conversation image"), target: targetSelector, stat: uploadImageStat, summaryKind: "browser" },
  preview_dialog: { surface: "group", family: "browser-page", icon: CircleAlert, doneTitle: (t) => t("回答了页面对话框", "Answered page dialog"), runningTitle: (t) => t("正在回答页面对话框", "Answering page dialog"), failedTitle: (t) => t("回答页面对话框失败", "Failed to answer page dialog"), target: (item) => compact(inputString(item, "prompt_text")), stat: (item, t) => (
    item.input.accept === false ? t("取消", "Dismiss") : t("接受", "Accept")
  ), summaryKind: "browser" },

  // Agent-protocol tools are ordinary rows inside the tool block: one uniform
  // disclosure per contiguous tool run, chevron → tailored detail. `agent-run`
  // carries a child transcript (instruction, updates, returned text) and is the
  // only family that offers the inline "open subagent" affordance; `agent-wait`
  // splits the drained `[name · status]` envelopes; `agent-note` is a one-shot
  // message whose payload already sits in the row's target.
  agent_spawn: { surface: "group", family: "agent-run", icon: Bot, doneTitle: (t) => t("派生了子代理", "Spawned subagent"), runningTitle: (t) => t("正在派生子代理", "Spawning subagent"), failedTitle: (t) => t("派生子代理失败", "Failed to spawn subagent"), target: (item) => compact(inputString(item, "label") ?? inputString(item, "name")), summaryKind: "agents" },
  // Retired names, kept so saved conversations still render their cards:
  // `agent_send`, and the `send_message` / `followup_task` pair that replaced
  // it. None of them is in the catalog any more; nothing issues them.
  agent_send: { surface: "group", family: "agent-run", icon: MessageCircleMore, doneTitle: (t) => t("向子代理发送了消息", "Sent message to subagent"), runningTitle: (t) => t("正在向子代理发送消息", "Sending message to subagent"), failedTitle: (t) => t("发送子代理消息失败", "Failed to send subagent message"), target: (item) => compact(inputString(item, "agent")), summaryKind: "agents" },
  send_message: {
    surface: "group",
    family: "agent-run",
    icon: MessageCircleMore,
    doneTitle: (t) => t("消息已加入子代理队列", "Queued message for subagent"),
    runningTitle: (t) => t("正在排队子代理消息", "Queueing message for subagent"),
    failedTitle: (t) => t("排队子代理消息失败", "Failed to queue subagent message"),
    // The child form travels upward and opens nothing, so it is a note rather
    // than an agent run: no target to name, no child transcript to disclose.
    resolveFamily: (item) => (isChildMainMessageContext(item) ? "agent-note" : "agent-run"),
    resolveTitle: (item, phase, t) => {
      if (!isChildMainMessageContext(item)) return undefined;
      if (phase === "running") return t("正在向主代理发送消息", "Sending message to main agent");
      if (phase === "failed") return t("向主代理发送消息失败", "Failed to send message to main agent");
      return t("向主代理发送了消息", "Sent message to main agent");
    },
    target: (item) => compact(
      isChildMainMessageContext(item) ? inputString(item, "message") : inputString(item, "target")
    ),
    summaryKind: "agents"
  },
  followup_task: { surface: "group", family: "agent-run", icon: MessageCircleMore, doneTitle: (t) => t("向子代理追加了任务", "Sent follow-up to subagent"), runningTitle: (t) => t("正在追加子代理任务", "Sending subagent follow-up"), failedTitle: (t) => t("追加子代理任务失败", "Failed to follow up with subagent"), target: (item) => compact(inputString(item, "target")), summaryKind: "agents" },
  task_wait: { surface: "group", family: "agent-wait", icon: Timer, doneTitle: (t) => t("等待了任务", "Waited for tasks"), runningTitle: (t) => t("正在等待任务", "Waiting for tasks"), failedTitle: (t) => t("等待任务失败", "Failed to wait for tasks"), stat: waitStat, summaryKind: "agents" },
  task_list: { surface: "group", family: "agent-note", icon: ListChecks, doneTitle: (t) => t("查看了任务列表", "Viewed task list"), runningTitle: (t) => t("正在查看任务列表", "Viewing task list"), failedTitle: (t) => t("查看任务列表失败", "Failed to view task list"), summaryKind: "agents" },
  // Unlike `task_list`, this call names the task it acts on, so it takes the raw
  // card: the address shows as an argument beside the host's receipt, and the
  // row keeps its phrase instead of trading it for a note's own text.
  task_stop: { surface: "group", family: "raw", icon: OctagonX, doneTitle: (t) => t("停止了任务", "Stopped a task"), runningTitle: (t) => t("正在停止任务", "Stopping a task"), failedTitle: (t) => t("停止任务失败", "Failed to stop a task"), target: (item) => compact(inputString(item, "task")), keys: ["task"], summaryKind: "agents" },
  // A message the host handed the model (Rust `wire_history::HOST_CARD_TOOL`):
  // a user-role message on the wire, or a deferred call's output. The host owns
  // the body, so the row only ever settles as done; the message is the result,
  // hence raw detail. A notice names itself in the card's `notice` so the row
  // can say what arrived.
  host_message: HOST_MESSAGE_VIEW,
  // The same message where the conversation's host messages come in `box`:
  // the result of the call the host wrote for it.
  box: HOST_MESSAGE_VIEW,

  // Skills return instruction text, so they use raw detail; the skill name is
  // this call's only input and becomes its target.
  skill: { surface: "group", family: "raw", icon: Box, doneTitle: (t) => t("加载了技能", "Loaded skill"), runningTitle: (t) => t("正在加载技能", "Loading skill"), failedTitle: (t) => t("加载技能失败", "Failed to load skill"), target: (item) => compact(inputString(item, "name")), keys: ["name"], summaryKind: "skill" },
  // Tool discovery returns a `<functions>` block, which is raw text for the same
  // reason a skill body is. Its target is the query, because that is what the
  // reader wants to see the run asked for; the MCP summary groups it with the
  // calls it enables.
  tool_search: { surface: "group", family: "raw", icon: Blocks, doneTitle: (t) => t("取回了工具定义", "Fetched tool definitions"), runningTitle: (t) => t("正在查找工具", "Searching for tools"), failedTitle: (t) => t("查找工具失败", "Tool search failed"), target: (item) => compact(inputString(item, "query")), keys: ["query", "max_results"], summaryKind: "mcp" },
  subagent: { surface: "group", family: "agent-run", icon: Bot, doneTitle: (t) => t("运行了子代理", "Ran subagent"), runningTitle: (t) => t("子代理正在工作", "Subagent is working"), failedTitle: (t) => t("子代理已中断", "Subagent was interrupted"), target: (item) => compact(inputString(item, "label")), summaryKind: "agents" },
  subagent_update: { surface: "group", family: "agent-note", icon: MessageCircleMore, doneTitle: (t) => t("子代理更新了状态", "Updated subagent status"), target: (item) => compact(inputString(item, "message")), summaryKind: "agents" },
  structured_output: { surface: "group", family: "agent-note", icon: MessageCircleMore, doneTitle: (t) => t("子代理交回了结构化结果", "Returned a structured result"), target: () => "", summaryKind: "agents" },
  subagent_activity: { surface: "group", family: "agent-note", icon: Bot, doneTitle: (t) => t("记录了子代理活动", "Recorded subagent activity"), target: (item) => compact(inputString(item, "message")), summaryKind: "agents" },
  update: { surface: "group", family: "agent-note", icon: MessageCircleMore, doneTitle: (t) => t("子代理更新了状态", "Updated subagent status"), target: (item) => compact(inputString(item, "message") ?? inputString(item, "content")), summaryKind: "agents" },

  ask_user: {
    surface: "question",
    family: "question",
    icon: CircleHelp,
    doneTitle: (t) => t("向用户提出了问题", "Asked the user a question"),
    target: (item) => compact(questionsFromInput(item.input)[0]?.question ?? ""),
    summaryKind: "other"
  },
  // A fork call only raises the request and returns; the receipt states that,
  // so the row carries the prompt's first line. An ordinary row — the request
  // has no card of its own.
  fork: {
    surface: "group",
    family: "raw",
    icon: GitFork,
    doneTitle: (t) => t("请求了分叉会话", "Requested a forked conversation"),
    runningTitle: (t) => t("正在请求分叉会话", "Requesting a forked conversation"),
    failedTitle: (t) => t("请求分叉会话失败", "Failed to request a forked conversation"),
    target: (item) => compact(inputString(item, "prompt")?.split("\n", 1)[0]),
    keys: ["prompt"],
    summaryKind: "fork"
  },
  // The plan itself has a page of its own, so these rows only say what happened;
  // the body is never repeated inline.
  plan: {
    surface: "group",
    family: "raw",
    icon: NotebookPen,
    doneTitle: (t) => t("已更新计划", "Updated the plan"),
    resolveTitle: (item, phase, t) => (
      phase === "running"
        ? t("正在撰写计划", "Writing the plan")
        : phase === "failed"
          ? t("计划操作失败", "Plan operation failed")
          : inputString(item, "action") === "read"
            ? t("已读取计划", "Read the plan")
            : t("已更新计划", "Updated the plan")
    ),
    keys: ["action"],
    summaryKind: "other"
  },
  exit_plan_mode: {
    surface: "group",
    family: "raw",
    icon: ClipboardCheck,
    doneTitle: (t) => t("已提交计划", "Submitted the plan"),
    runningTitle: (t) => t("等待批准计划", "Waiting for plan approval"),
    failedTitle: (t) => t("提交计划失败", "Failed to submit the plan"),
    keys: [],
    summaryKind: "other"
  },
  // Handoff notes are the host's own notebook, not memory: the note travels
  // in the call's input, so the row opens onto it.
  read_handoff_note: {
    surface: "group",
    family: "raw",
    icon: NotebookText,
    doneTitle: (t) => t("读取了交接文档", "Read handoff note"),
    runningTitle: (t) => t("正在读取交接文档", "Reading handoff note"),
    failedTitle: (t) => t("读取交接文档失败", "Failed to read handoff note"),
    target: memoryTarget,
    keys: ["name"],
    summaryKind: "handoff"
  },
  create_handoff_note: {
    surface: "group",
    family: "raw",
    icon: NotebookPen,
    doneTitle: (t) => t("写了交接文档", "Wrote handoff note"),
    runningTitle: (t) => t("正在写交接文档", "Writing handoff note"),
    failedTitle: (t) => t("写交接文档失败", "Failed to write handoff note"),
    target: memoryTarget,
    keys: ["name", "description", "content"],
    summaryKind: "handoff"
  },
  edit_handoff_note: {
    surface: "group",
    family: "raw",
    icon: NotebookPen,
    doneTitle: (t) => t("修改了交接文档", "Edited handoff note"),
    runningTitle: (t) => t("正在修改交接文档", "Editing handoff note"),
    failedTitle: (t) => t("修改交接文档失败", "Failed to edit handoff note"),
    target: memoryTarget,
    keys: ["name", "description", "old_text", "new_text"],
    summaryKind: "handoff"
  },
  // The result names the conversation the work went on in.
  handoff: {
    surface: "group",
    family: "raw",
    icon: Handshake,
    doneTitle: (t) => t("已交接到新会话", "Handed off to a new conversation"),
    runningTitle: (t) => t("正在交接", "Handing off"),
    failedTitle: (t) => t("交接未完成", "Handoff not completed"),
    keys: [],
    summaryKind: "handoff"
  },
  read_global_memory: {
    surface: "group",
    family: "memory",
    icon: FileText,
    doneTitle: (t) => t("读取了全局记忆", "Read global memory"),
    runningTitle: (t) => t("正在读取全局记忆", "Reading global memory"),
    failedTitle: (t) => t("读取全局记忆失败", "Failed to read global memory"),
    target: memoryTarget,
    summaryKind: "memory"
  },
  read_project_memory: {
    surface: "group",
    family: "memory",
    icon: FileText,
    doneTitle: (t) => t("读取了项目记忆", "Read project memory"),
    runningTitle: (t) => t("正在读取项目记忆", "Reading project memory"),
    failedTitle: (t) => t("读取项目记忆失败", "Failed to read project memory"),
    target: memoryTarget,
    summaryKind: "memory"
  },
  create_global_memory: {
    surface: "group",
    family: "memory",
    icon: FilePenLine,
    doneTitle: (t) => t("创建了全局记忆", "Created global memory"),
    runningTitle: (t) => t("正在创建全局记忆", "Creating global memory"),
    failedTitle: (t) => t("创建全局记忆失败", "Failed to create global memory"),
    target: memoryTarget,
    summaryKind: "memory"
  },
  create_project_memory: {
    surface: "group",
    family: "memory",
    icon: FilePenLine,
    doneTitle: (t) => t("创建了项目记忆", "Created project memory"),
    runningTitle: (t) => t("正在创建项目记忆", "Creating project memory"),
    failedTitle: (t) => t("创建项目记忆失败", "Failed to create project memory"),
    target: memoryTarget,
    summaryKind: "memory"
  },
  edit_global_memory: {
    surface: "group",
    family: "memory",
    icon: FilePenLine,
    doneTitle: (t) => t("编辑了全局记忆", "Edited global memory"),
    runningTitle: (t) => t("正在编辑全局记忆", "Editing global memory"),
    failedTitle: (t) => t("编辑全局记忆失败", "Failed to edit global memory"),
    target: memoryTarget,
    summaryKind: "memory"
  },
  edit_project_memory: {
    surface: "group",
    family: "memory",
    icon: FilePenLine,
    doneTitle: (t) => t("编辑了项目记忆", "Edited project memory"),
    runningTitle: (t) => t("正在编辑项目记忆", "Editing project memory"),
    failedTitle: (t) => t("编辑项目记忆失败", "Failed to edit project memory"),
    target: memoryTarget,
    summaryKind: "memory"
  }
} as const satisfies Record<string, ToolViewConfig>;

function registryEntry(name: string): ToolViewConfig | undefined {
  return (TOOL_VIEW_REGISTRY as Record<string, ToolViewConfig>)[name];
}

/**
 * The timeline surface a tool renders on.
 *
 * Only two tools leave the ordinary block: `ask_user`, whose history card
 * carries the user's own answer, and `workflow`, which draws a card of its own
 * for the whole life of the run. Everything else — including every
 * agent-protocol and task state call — is an ordinary row inside the one
 * tool block for its contiguous run.
 */
export function toolSurfaceForName(name: string): ToolSurface {
  return registryEntry(name)?.surface ?? "group";
}

/**
 * True while a streamed tool call has not yet reached its terminal status.
 *
 * A persisted context never carries `streaming`, so this is false for every
 * item the timeline reloads from storage — which is the point.
 */
function isLiveCall(item: ToolContext): boolean {
  return item.streaming === true && item.streamStatus !== "completed";
}

/**
 * Whether this call belongs in the ordinary two-level tool disclosure.
 *
 * A `workflow` call never does, running or settled. Its card is built from the
 * agent roster rather than from live stream events, and the roster outlives the
 * model run, so a finished plan keeps the same card it had while it ran instead
 * of collapsing into a row the moment it succeeds. Every filter that assembles
 * the ordinary group must agree, or the call is counted in the summary and then
 * never rendered (or the reverse).
 */
function isOrdinaryToolCall(item: ToolContext): boolean {
  return toolSurfaceForName(item.toolName) === "group";
}

/**
 * Whether the timeline hoists this call onto the end-of-stream waiting
 * indicator instead of drawing a block for it.
 *
 * An ordinary call in flight has nothing a block can show that the one-line
 * activity does not: its result is empty until the host reports it, so the card
 * would be a heading and a spinner occupying a full row, then replaced the
 * moment the real receipt lands. Hoisting it keeps "what is happening right
 * now" in one place — the indicator — and leaves the timeline to settled work.
 *
 * The two non-ordinary surfaces keep their own live representation and hoist
 * only before execution starts, exactly as they always did: `workflow` draws a
 * progress card for the whole run, and `ask_user` is answered in its dock.
 */
export function isHoistedToolCall(item: ToolContext): boolean {
  if (!isLiveCall(item)) return false;
  return isOrdinaryToolCall(item)
    || item.streamStatus === "announced"
    || item.streamStatus === "ready";
}

function executionPhase(item: ToolContext): "done" | "running" | "failed" {
  if (item.streaming && item.streamStatus !== "completed") return "running";
  return item.result.success ? "done" : "failed";
}

/**
 * MCP tools are named by the host, never by this registry:
 * `mcp__<server-slug>__<tool-slug>__<digest>`. The prefix is the whole contract
 * — matching it is what lets a server the user installed today render as an MCP
 * call rather than as an unrecognized one.
 */
export function isMcpToolName(name: string): boolean {
  return name.startsWith("mcp__");
}

/**
 * The server and tool an MCP call names, in the words the user chose.
 *
 * The descriptor's label carries both verbatim (`MCP <server> / <tool>`), so it
 * is preferred; a trailing parenthetical is the host's manual-confirmation
 * notice and belongs in the approval dock, not in a timeline row. Without a
 * descriptor — an archived conversation whose server has since been removed —
 * the wire name still yields readable slugs, and the server slug's trailing
 * digest is dropped because it identifies nothing the reader can act on.
 */
export function mcpToolNaming(item: ToolContext, descriptor?: ToolDescriptor): { server?: string; tool: string } {
  const labelled = /^MCP\s+(.+?)\s+\/\s+(.+)$/.exec(descriptor?.label?.trim() ?? "");
  if (labelled) {
    return { server: labelled[1].trim(), tool: labelled[2].replace(/\s*\([^)]*\)\s*$/, "").trim() };
  }
  const parts = item.toolName.split("__");
  return parts.length >= 3
    ? { server: parts[1].replace(/_[0-9a-f]{10}$/, ""), tool: parts[2] }
    : { tool: item.toolName };
}

/**
 * The bucket this call is counted in by the block heading.
 *
 * MCP is decided by the wire name because those tools are discovered at
 * runtime and have no registry entry to carry a bucket.
 */
export function toolSummaryKind(item: ToolContext): SummaryKind {
  if (isMcpToolName(item.toolName)) return "mcp";
  return registryEntry(item.toolName)?.summaryKind ?? "other";
}

/**
 * What a block row calls the tool: its name, not a sentence about it.
 *
 * The row is one line — icon, name, one line of the call itself — so the
 * localized phrase ("Ran Bash command") would crowd out the argument that says
 * what actually happened. That phrase is not lost: it stays the row's
 * accessible name and its tooltip, and the block heading counts it.
 */
export function toolRowName(item: ToolContext, descriptor?: ToolDescriptor): string {
  if (isMcpToolName(item.toolName)) return mcpToolNaming(item, descriptor).tool;
  return item.toolName;
}

/**
 * `explanation` is the local helper model's one-line description of a shell
 * command; when present it replaces the whole title, prefixed with the shell.
 *
 * A failed call's title says why after what failed: `errorExplanation`, the
 * local helper model's reason, or until there is one (or with that use off)
 * the error's own line.
 */
export function getToolPresentation(
  item: ToolContext,
  descriptor?: ToolDescriptor,
  t: Translate = globalT,
  explanation?: string,
  errorExplanation?: string
): ToolPresentation {
  const config = registryEntry(item.toolName);
  const phase = executionPhase(item);
  const reason = phase === "failed"
    ? errorExplanation?.trim() || errorExcerpt(item.result.output)
    : undefined;
  if (!config) {
    const duration = item.result.durationMs > 0 ? `${item.result.durationMs} ms` : undefined;
    if (isMcpToolName(item.toolName)) {
      const { server, tool } = mcpToolNaming(item, descriptor);
      const target = firstArgumentSummary(item) ?? server;
      return {
        surface: "group",
        family: "raw",
        icon: Plug,
        title: phase === "running"
          ? t("正在调用 MCP 工具 {label}", "Calling MCP tool {label}", { label: tool })
          : phase === "failed"
            ? failureTitle(t("MCP 工具 {label} 调用失败", "MCP tool {label} failed", { label: tool }), reason, t)
            : t("调用了 MCP 工具 {label}", "Called MCP tool {label}", { label: tool }),
        ...(target ? { target } : {}),
        ...(duration ? { stat: duration } : {})
      };
    }
    const label = descriptor?.label?.trim() || item.toolName;
    return {
      surface: "group",
      family: "raw",
      icon: Wrench,
      title: phase === "running"
        ? t("正在使用 {label}", "Using {label}", { label })
        : phase === "failed"
          ? failureTitle(t("{label}执行失败", "{label} failed", { label }), reason, t)
          : t("使用了 {label}", "Used {label}", { label }),
      stat: duration
    };
  }
  const shell = backendOfTool(item.toolName);
  const explained = shell && explanation?.trim()
    ? phase === "failed"
      ? t("{shell}：{text}（失败）", "{shell}: {text} (failed)", { shell: shellBackendLabel(shell), text: explanation.trim() })
      : t("{shell}：{text}", "{shell}: {text}", { shell: shellBackendLabel(shell), text: explanation.trim() })
    : undefined;
  const named = phase === "done" ? config.resolveSubject?.(item, t) : undefined;
  const phrase = config.resolveTitle?.(item, phase, t)
    ?? (phase === "running" ? config.runningTitle : phase === "failed" ? config.failedTitle : config.doneTitle)?.(t)
    ?? config.doneTitle(t);
  const title = reason
    ? failureTitle(phrase, reason, t)
    : explained
      ?? (named ? `${named.subject.before}${named.subject.text}${named.subject.after}` : phrase);
  const target = config.target?.(item, t);
  const stat = config.stat?.(item, t);
  return {
    surface: config.surface,
    family: config.resolveFamily?.(item) ?? config.family,
    icon: config.icon,
    title,
    ...(named && !explained ? { subject: named.subject } : {}),
    ...(named?.counts ? { counts: named.counts } : {}),
    ...(named?.note ? { note: named.note } : {}),
    ...(target ? { target } : {}),
    ...(target && config.targetIsPath ? { targetIsPath: true } : {}),
    ...(stat ? { stat } : {}),
    ...(config.keys ? { keys: config.keys } : {})
  };
}

function summaryPhrase(kind: SummaryKind, count: number, t: Translate): string {
  switch (kind) {
    case "commands": return count === 1
      ? t("运行了 1 个命令", "Ran 1 command")
      : t("运行了 {count} 个命令", "Ran {count} commands", { count });
    case "fileChanges": return count === 1
      ? t("1 次文件操作", "1 file change")
      : t("{count} 次文件操作", "{count} file changes", { count });
    case "agents": return count === 1
      ? t("调用了 1 个子代理", "Called 1 subagent")
      : t("调用了 {count} 个子代理", "Called {count} subagents", { count });
    case "mcp": return count === 1
      ? t("调用了 1 个 MCP 工具", "Called 1 MCP tool")
      : t("调用了 {count} 个 MCP 工具", "Called {count} MCP tools", { count });
    case "browser": return count === 1
      ? t("执行了 1 次浏览器操作", "Performed 1 browser action")
      : t("执行了 {count} 次浏览器操作", "Performed {count} browser actions", { count });
    case "fork": return count === 1
      ? t("请求了 1 次会话分叉", "Requested 1 conversation fork")
      : t("请求了 {count} 次会话分叉", "Requested {count} conversation forks", { count });
    case "hook": return count === 1
      ? t("触发了 1 个 hook", "Triggered 1 hook")
      : t("触发了 {count} 个 hook", "Triggered {count} hooks", { count });
    case "skill": return count === 1
      ? t("读取了 1 个技能", "Loaded 1 skill")
      : t("读取了 {count} 个技能", "Loaded {count} skills", { count });
    case "memory": return count === 1
      ? t("访问了 1 次记忆", "Accessed memory once")
      : t("访问了 {count} 次记忆", "Accessed memory {count} times", { count });
    case "handoff": return count === 1
      ? t("1 次交接操作", "1 handoff action")
      : t("{count} 次交接操作", "{count} handoff actions", { count });
    case "search": return count === 1
      ? t("搜索了 1 次内容", "Performed 1 content search")
      : t("搜索了 {count} 次内容", "Performed {count} content searches", { count });
    case "reads": return count === 1
      ? t("读取了 1 个文件", "Read 1 file")
      : t("读取了 {count} 个文件", "Read {count} files", { count });
    case "files": return count === 1
      ? t("检查了 1 次文件与目录", "Checked files and directories once")
      : t("检查了 {count} 次文件与目录", "Checked files and directories {count} times", { count });
    case "reasoning": return count === 1
      ? t("思考了 1 次", "Thought once")
      : t("思考了 {count} 次", "Thought {count} times", { count });
    default: return count === 1
      ? t("使用了 1 个工具", "Used 1 tool")
      : t("使用了 {count} 个工具", "Used {count} tools", { count });
  }
}

/**
 * Buckets in the order the heading states them: most consequential first.
 *
 * The heading is one line and ellipsizes, so this order decides what survives
 * a narrow window. Work that changed something outside the conversation leads;
 * reads, which are the most numerous and the least consequential, trail.
 */
const SUMMARY_ORDER: readonly SummaryKind[] = [
  "commands",
  "fileChanges",
  "agents",
  "mcp",
  "browser",
  "fork",
  "hook",
  "skill",
  "memory",
  "search",
  "reads",
  "files",
  "reasoning",
  "other"
];

/**
 * What one block did, as a sentence of counted clauses.
 *
 * A block mixes reasoning, hooks and every kind of tool call, so no single
 * clause can describe it; the heading names each bucket it contains and lets
 * {@link SUMMARY_ORDER} decide which ones stay visible when it truncates.
 */
export function summarizeBlockKinds(kinds: SummaryKind[], t: Translate = globalT): string {
  const counts = new Map<SummaryKind, number>();
  for (const kind of kinds) counts.set(kind, (counts.get(kind) ?? 0) + 1);
  return SUMMARY_ORDER
    .flatMap((kind) => {
      const count = counts.get(kind) ?? 0;
      return count ? [summaryPhrase(kind, count, t)] : [];
    })
    .join(t("，", ", "));
}

function safeStringify(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    return String(value);
  }
}

function RawDataDisclosure({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  return (
    <details className="tool-renderer__raw" onToggle={(event) => setOpen(event.currentTarget.open)}>
      <summary>{t("原始数据", "Raw data")}</summary>
      {open && <pre>{safeStringify({
        tool: item.toolName,
        input: item.input,
        result: item.result
      })}</pre>}
    </details>
  );
}

function EmptyResult({ children }: { children?: ReactNode }) {
  const { t } = useI18n();
  return <div className="tool-renderer__lines tool-renderer__note">{children ?? t("没有可显示的结果", "No results to display")}</div>;
}

/**
 * A line the tool wrote about its answer — what it left out, how much of it it
 * showed — rather than one of the entries: `… 12 more`, `(Showing 6 of 14)`.
 */
function isNoteLine(line: string): boolean {
  const text = line.trim();
  return text.startsWith("… ") || /^\(.*\)$/.test(text);
}

/** What the tool sets after an entry's path to say something of it: `target/ (ignored)`. */
const ENTRY_MARK = / \([^()]*\)$/;
/** How a line opens when it is the tool's remark on the list and not an entry of it. */
const ENTRY_REMARK = /^(?:\(|… |\[skipped\] )/; // i18n-audit-ignore: parses backend sentinels

/**
 * The icon ahead of a listed path: its type's, or a folder for a path the tool closed
 * with a slash. A line that is the tool's own remark has none.
 */
function EntryIcon({ line }: { line: string }) {
  const path = line.trim().replace(ENTRY_MARK, "");
  if (!path || ENTRY_REMARK.test(path)) return null;
  return path.endsWith("/")
    ? <Folder className="tool-renderer__entry-icon" size={12} aria-hidden="true" />
    : <FileIcon path={path} size={12} inline />;
}

function FileListView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const lines = outputLines(item.result.output).filter((line) => line.trim());
  const empty = !lines.length || (lines.length === 1 && FILE_LIST_EMPTY_ANSWERS.includes(lines[0].trim()));
  if (empty) return <EmptyResult>{lines[0]?.trim() || t("没有文件", "No files")}</EmptyResult>;
  return (
    <ul className="tool-renderer__lines" aria-label={t("文件结果", "File results")}>
      {lines.map((line, index) => {
        const value = line.trimEnd();
        return (
          <li key={`${index}:${value}`} className={isNoteLine(value) ? "tool-renderer__note" : undefined}>
            <EntryIcon line={value} />
            {value}
          </li>
        );
      })}
    </ul>
  );
}

type GrepMode = "files_with_matches" | "content" | "count";

/**
 * One line of what `grep` printed, read by the mode the call asked for. `location` is
 * where a line was found (`path:12:`, `path-13-` for context, `path:`), set back from
 * `content`, what was found there.
 */
type GrepRow =
  | { kind: "file" | "separator" | "note"; raw: string }
  | { kind: "match" | "context" | "count"; raw: string; location: string; content: string };

const GREP_MATCH_LINE = /^(.+?):(\d+):(.*)$/;
const GREP_CONTEXT_LINE = /^(.+?)-(\d+)-(.*)$/;
const GREP_COUNT_LINE = /^(.+):(\d+)$/;

/**
 * The mode a call asked for. One that named none, or a name this card does not know,
 * got the tool's default: the mode alone says how a line reads, because a path may
 * itself hold `:12:` or `-3-`.
 */
function grepMode(item: ToolContext): GrepMode {
  const requested = inputString(item, "output_mode");
  if (requested === "content" || requested === "count") return requested;
  // A search recorded before `output_mode` existed names no mode and was answered in the old
  // `path:line:text` lines. Only an answer in which every single entry reads that way is taken
  // for one, and only when no mode was named: a list of files whose every path holds `:12:`
  // is not a thing, whereas the old transcripts are.
  if (requested === undefined) {
    const entries = outputLines(item.result.output).filter((line) => line.trim() && !isGrepNote(line));
    if (entries.length > 0 && entries.every((line) => GREP_MATCH_LINE.test(line))) return "content";
  }
  return "files_with_matches";
}

/**
 * A line the tool wrote about its answer rather than an entry of it: how much is left
 * (`… more matches follow`), what it could not read, or that there was nothing to give.
 */
function isGrepNote(line: string): boolean {
  const text = line.trim();
  return line.startsWith("… ")
    || line.startsWith("[skipped] ")
    || GREP_EMPTY_ANSWERS.includes(text)
    || GREP_PAST_END.test(text);
}

function isGrepEmptyAnswer(line: string): boolean {
  const text = line.trim();
  return GREP_EMPTY_ANSWERS.includes(text) || GREP_PAST_END.test(text);
}

/** The line as `path:12:text`, split where the leftmost `:digits:` falls — the text may hold more of them. */
function matchRow(raw: string): GrepRow | undefined {
  const match = GREP_MATCH_LINE.exec(raw);
  return match ? { kind: "match", raw, location: raw.slice(0, raw.length - match[3].length), content: match[3] } : undefined;
}

function contextRow(raw: string): GrepRow | undefined {
  const match = GREP_CONTEXT_LINE.exec(raw);
  return match ? { kind: "context", raw, location: raw.slice(0, raw.length - match[3].length), content: match[3] } : undefined;
}

/** The line as `path<separator>12<separator>text` for a path already known. */
function rowOfPath(raw: string, path: string, separator: ":" | "-"): GrepRow | undefined {
  if (!raw.startsWith(path + separator)) return undefined;
  const number = /^\d+/.exec(raw.slice(path.length + 1))?.[0];
  if (number === undefined || raw[path.length + 1 + number.length] !== separator) return undefined;
  const cut = path.length + number.length + 2;
  return { kind: separator === ":" ? "match" : "context", raw, location: raw.slice(0, cut), content: raw.slice(cut) };
}

/**
 * One group of content lines: the run between two `--` lines, which is one file's.
 *
 * With context on, a matching line and a context line differ only by the character
 * after the path, and a path or a line's own text may hold `:12:` or `-3-` too. The
 * group's file settles it: a matching line cuts at its first `:digits:`, and a context
 * line's text can only make that cut later, never earlier, so the shortest such path
 * in the group is the file. Lines are then read against it.
 */
function grepGroupRows(group: string[], withContext: boolean): GrepRow[] {
  let path: string | undefined;
  if (withContext) {
    for (const raw of group) {
      if (isGrepNote(raw)) continue;
      const found = GREP_MATCH_LINE.exec(raw)?.[1];
      if (found !== undefined && (path === undefined || found.length < path.length)) path = found;
    }
  }
  return group.map((raw): GrepRow => {
    if (isGrepNote(raw)) return { kind: "note", raw };
    return (path !== undefined ? rowOfPath(raw, path, ":") ?? rowOfPath(raw, path, "-") : undefined)
      ?? matchRow(raw)
      ?? (withContext ? contextRow(raw) : undefined)
      ?? { kind: "note", raw };
  });
}

function grepContentRows(lines: string[], withContext: boolean): GrepRow[] {
  const rows: GrepRow[] = [];
  let group: string[] = [];
  const close = () => {
    rows.push(...grepGroupRows(group, withContext));
    group = [];
  };
  for (const raw of lines) {
    if (raw !== "--") {
      group.push(raw);
      continue;
    }
    close();
    rows.push({ kind: "separator", raw });
  }
  close();
  return rows;
}

/**
 * What `grep` printed, row by row, and how many entries of the mode's own unit it holds:
 * files, matching lines (never the context around them) or files with their counts.
 * `more` says the tool left entries for a later page.
 */
function parseGrepOutput(item: ToolContext): { mode: GrepMode; rows: GrepRow[]; count: number; more: boolean } {
  const mode = grepMode(item);
  const lines = outputLines(item.result.output).filter((line) => line.trim());
  let rows: GrepRow[];
  if (mode === "content") {
    // Context was asked for, or the tool's own `--` between groups says it was given.
    const withContext = ["context", "before", "after"].some((name) => (inputNumber(item, name) ?? 0) > 0)
      || lines.includes("--");
    rows = grepContentRows(lines, withContext);
  } else {
    rows = lines.map((raw): GrepRow => {
      if (isGrepNote(raw)) return { kind: "note", raw };
      if (mode === "files_with_matches") return { kind: "file", raw };
      const counted = raw.startsWith("Total: ") ? null : GREP_COUNT_LINE.exec(raw);
      return counted
        ? { kind: "count", raw, location: `${counted[1]}:`, content: counted[2] }
        : { kind: "note", raw };
    });
  }
  return {
    mode,
    rows,
    count: rows.filter((row) => row.kind === "file" || row.kind === "match" || row.kind === "count").length,
    more: rows.some((row) => row.kind === "note" && row.raw.startsWith("… "))
  };
}

/**
 * `grep` as the tool printed it, a line to an entry: the files that matched, or the
 * matching lines with where each was found set back from what was found (and the lines
 * around them, further back still), or a count per file.
 */
function GrepView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const { rows } = parseGrepOutput(item);
  if (!rows.length || (rows.length === 1 && isGrepEmptyAnswer(rows[0].raw))) {
    return <EmptyResult>{rows[0]?.raw.trim() || t("未找到匹配内容", "No matching content found")}</EmptyResult>;
  }
  return (
    <ol className="tool-renderer__lines" aria-label={t("搜索结果", "Search results")}>
      {rows.map((row, index) => {
        const key = `${index}:${row.raw}`;
        switch (row.kind) {
          case "match":
          case "count":
            return (
              <li key={key}>
                <span className="tool-renderer__dim">{row.location}</span>
                {row.content}
              </li>
            );
          case "context":
            return (
              <li key={key} className="tool-renderer__dim">
                <span className="tool-renderer__note">{row.location}</span>
                {row.content}
              </li>
            );
          case "file":
            return (
              <li key={key}>
                <EntryIcon line={row.raw} />
                {row.raw}
              </li>
            );
          default:
            return <li key={key} className="tool-renderer__note">{row.raw}</li>;
        }
      })}
    </ol>
  );
}

function ReadView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const start = Math.max(1, Math.floor(inputNumber(item, "start_line") ?? 1));
  const path = inputString(item, "path");
  const lines = useMemo(() => outputLines(item.result.output), [item.result.output]);
  // The excerpt starts wherever the read did, so what was open above it is not known and
  // it is coloured from clean.
  const tokens = useMemo(
    () => (path && lines.length <= READ_HIGHLIGHT_LINES ? highlightCodeLines(codeLanguage(path), lines) : null),
    [path, lines]
  );
  if ((!lines.length || !item.result.output) && !item.result.images?.length) {
    return <EmptyResult>{t("文件内容为空", "File is empty")}</EmptyResult>;
  }
  if (!item.result.output) return null;
  // An empty file and a start past the end are answered with a sentence, which has no line to number.
  const sentence = lines.length === 1 ? lines[0].trim() : "";
  if (sentence === READ_EMPTY_FILE) return <EmptyResult>{t("文件内容为空", "File is empty")}</EmptyResult>;
  if (READ_PAST_END.test(sentence)) return <EmptyResult>{sentence}</EmptyResult>;
  const gutter = String(start + Math.max(0, lines.length - 1)).length;
  return (
    <div
      className="diff-output"
      role="region"
      aria-label={t("{path} 内容", "{path} contents", { path: path ?? t("文件", "File") })}
      style={{ "--diff-gutter": `${gutter}ch` } as CSSProperties}
    >
      <div className="diff-output__body">
        {lines.map((line, index) => (
          <div key={`${start + index}:${line}`} className={isReadNote(line) ? "diff-output__line diff-output__line--meta" : "diff-output__line diff-output__line--context"}>
            <span className="diff-output__line-number" aria-hidden="true">{start + index}</span>
            <span className="diff-output__content">
              {isReadNote(line) ? line : <CodeTokens tokens={tokens?.[index]} fallback={line || " "} />}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

function DiffView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  if (item.result.diff) {
    return (
      <DiffOutput
        value={item.result.diff}
        path={inputString(item, "path")}
      />
    );
  }
  return (
    <div className="tool-renderer__lines">
      {item.result.output || t("操作成功，没有行级变化", "Operation succeeded with no line-level changes")}
    </div>
  );
}

function TerminalView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const command = inputString(item, "command") ?? "";
  return (
    <div className="tool-renderer__terminal">
      <div className="tool-renderer__command">
        <span aria-hidden="true">›</span>
        <pre>{command}</pre>
      </div>
      <pre className="tool-renderer__terminal-output">{item.result.output || t("命令没有输出", "Command produced no output")}</pre>
    </div>
  );
}

function scalarText(value: unknown, t: Translate = globalT): string {
  if (typeof value === "boolean") return value ? t("是", "Yes") : t("否", "No");
  if (typeof value === "string" || typeof value === "number") return String(value);
  if (value === null) return "null";
  return safeStringify(value);
}

function JsonFallback({ item, parsed }: { item: ToolContext; parsed: ParsedJson }) {
  const { t } = useI18n();
  return (
    <div className="tool-renderer__lines">
      <div className="tool-renderer__note">{parsed.parsed ? t("结果结构无法识别", "Unrecognized result structure") : t("结果不是有效 JSON", "Result is not valid JSON")}</div>
      <div>{item.result.output || t("没有返回内容", "No content returned")}</div>
    </div>
  );
}

/**
 * `preview_snapshot`. The answer is the accessibility tree the tool already
 * formatted — plain text, not JSON — so the card shows it verbatim rather than
 * rebuilding a structure it does not have.
 */
function BrowserSnapshotView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const text = item.result.output.trim();
  if (!text) return <EmptyResult>{t("快照中没有可读内容", "The snapshot has no readable content")}</EmptyResult>;
  return (
    <pre className="tool-renderer__lines" aria-label={t("页面快照", "Page snapshot")}>{text}</pre>
  );
}

/** `preview_eval`. The value is already JSON the tool serialized, or the literal `undefined`. */
function BrowserEvaluateView({ item }: { item: ToolContext }) {
  return (
    <KeyValueCard
      rows={[["JavaScript", inputString(item, "expression")]]}
      result={item.result.output || "undefined"}
    />
  );
}

function BrowserScreenshotView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const parsed = parseJsonOutput(item);
  const value = parsed.parsed ? object(parsed.value) : null;
  if (!value) return <JsonFallback item={item} parsed={parsed} />;
  return (
    <KeyValueCard rows={[
      [t("尺寸", "Dimensions"), value.width !== undefined && value.height !== undefined ? `${scalarText(value.width)}×${scalarText(value.height)}` : undefined]
    ]} />
  );
}

/** `preview_console_logs`, as the tool printed it: one `[level] text` to a line. */
function BrowserConsoleView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const output = item.result.output.trim();
  if (!output || output === "No console logs.") { // i18n-audit-ignore: the tool's own English answer, copied from Claude Code
    return <EmptyResult>{t("没有 Console 日志", "No Console logs")}</EmptyResult>;
  }
  const lines = outputLines(output).filter((line) => line.trim());
  return (
    <ol className="tool-renderer__lines" aria-label={t("Console 日志", "Console logs")}>
      {lines.map((line, index) => {
        const level = /^\[([a-z]+)\]/i.exec(line)?.[1].toLowerCase();
        const tone = isNoteLine(line) ? "tool-renderer__note" : level ? `tool-renderer__log--${level}` : undefined;
        return <li key={`${index}:${line}`} className={tone}>{line}</li>;
      })}
    </ol>
  );
}

const NETWORK_ENTRY = /^\[([^\]]+)]\s+([A-Z]+)\s+(\S+)(?:\s+→\s+(\d+)\s*(.*?))?\s*(\[FAILED:[\s\S]*])?$/;

/**
 * `preview_network`. Without `requestId` the answer is the request ledger, one
 * line each, and stays as the tool printed it — the id a line leads with is what
 * a later call asks for a body by. With one it is that body, which is text.
 */
function BrowserNetworkView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const output = item.result.output.trim();
  if (inputString(item, "requestId")) {
    return output
      ? <pre className="tool-renderer__lines">{output}</pre>
      : <EmptyResult>{t("没有响应正文", "No response body")}</EmptyResult>;
  }
  const lines = outputLines(output).filter((line) => line.trim());
  if (!lines.length || !lines.some((line) => NETWORK_ENTRY.test(line))) {
    return <EmptyResult>{lines[0] ?? t("没有网络记录", "No network records")}</EmptyResult>;
  }
  return (
    <ol className="tool-renderer__lines" aria-label={t("网络请求", "Network requests")}>
      {lines.map((line, index) => {
        const failed = Boolean(NETWORK_ENTRY.exec(line)?.[6]);
        const tone = failed ? "tool-renderer__log--error" : isNoteLine(line) ? "tool-renderer__note" : undefined;
        return <li key={`${index}:${line}`} className={tone}>{line}</li>;
      })}
    </ol>
  );
}

/**
 * A preview tool whose whole answer is one JSON document — `preview_list`'s
 * server array, `preview_inspect`'s element. The host serializes those compactly,
 * so the card re-indents rather than printing one very long line.
 */
function BrowserJsonView({ item, presentation }: { item: ToolContext; presentation: ToolPresentation }) {
  const parsed = parseJsonOutput(item);
  if (!parsed.parsed) return <RawView item={item} presentation={presentation} />;
  const names = presentation.keys ?? Object.keys(item.input);
  return (
    <KeyValueCard
      rows={names.map((name) => [name, rawFieldText(item.input[name])])}
      result={safeStringify(parsed.value)}
    />
  );
}

/**
 * Mewrk's own two preview tools answer with a JSON object that carries the page
 * state after the action: where the page ended up, anything that blocked it, the
 * console errors the action produced, and a bounded accessibility tree. Every
 * other key belongs to the tool itself and is shown above them.
 */
const PAGE_REPORT_KEYS = ["page", "snapshot", "notices", "newConsoleErrors", "modalState"];

function BrowserPageView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const parsed = parseJsonOutput(item);
  const value = parsed.parsed ? object(parsed.value) : null;
  if (!value) return <JsonFallback item={item} parsed={parsed} />;
  const page = object(value.page);
  const snapshot = object(value.snapshot);
  const modal = object(value.modalState);
  const notices = Array.isArray(value.notices) ? value.notices : [];
  const consoleErrors = Array.isArray(value.newConsoleErrors) ? value.newConsoleErrors : [];
  const modalDescription = Array.isArray(modal?.description)
    ? modal.description.map((entry) => scalarText(entry)).join("; ")
    : modal
      ? scalarText(modal.description)
      : undefined;
  const own = Object.entries(value).flatMap<KvRow>(([key, entry]) => (
    PAGE_REPORT_KEYS.includes(key) || object(entry) || Array.isArray(entry) ? [] : [[key, entry]]
  ));
  return (
    <KeyValueCard
      rows={[
        ...own,
        ["URL", page?.url],
        [t("标题", "Title"), page?.title],
        [t("加载中", "Loading"), page?.loading],
        [t("模态状态", "Modal state"), modalDescription],
        [t("提示", "Notices"), notices.map((notice) => scalarText(notice)).join("\n")],
        [t("新增 Console 错误", "New Console errors"), consoleErrors.map((entry) => scalarText(entry)).join("\n")]
      ]}
      result={typeof snapshot?.tree === "string" ? snapshot.tree : undefined}
    />
  );
}

/** An argument as the editor would show it: strings verbatim, anything else as JSON. */
function rawFieldText(value: JsonValue | undefined): string {
  if (value === undefined) return "";
  return typeof value === "string" ? value : safeStringify(value);
}

/** One line of a card: what the value is called, and the value. */
type KvRow = [label: string, value: unknown];

/**
 * The one shape a card takes when it has no picture to draw.
 *
 * Everything worth knowing is a line: its name at the left, its value beside
 * it. Whatever the call answered with comes last, under a rule and across the
 * card — the result is the one thing every card has, so it needs no label to
 * say which it is. Families that answer with a diff, a terminal, a listing or
 * pixels keep drawing those; everything else is lines and a result, so a reader
 * who has learned one card has learned all of them.
 */
function KeyValueCard({ rows, result }: { rows: KvRow[]; result?: string }) {
  const visible = rows.flatMap<[string, string]>(([label, value]) => {
    if (value === undefined || value === "") return [];
    return [[label, typeof value === "string" ? value : scalarText(value)]];
  });
  if (!visible.length && !result) return <EmptyResult />;
  return (
    <dl className="tool-kv">
      {visible.map(([label, text], index) => (
        <div className="tool-kv__row" key={`${index}:${label}`}>
          <dt className="tool-kv__key" title={label}>{label}</dt>
          <dd className="tool-kv__value"><pre>{text}</pre></dd>
        </div>
      ))}
      {result ? (
        <div className="tool-kv__row tool-kv__row--result">
          <dd className="tool-kv__value"><pre>{result}</pre></dd>
        </div>
      ) : null}
    </dl>
  );
}

function MemoryView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const tier = item.toolName.includes("_global_")
    ? t("全局记忆", "Global memory")
    : t("项目记忆", "Project memory");
  // A memory tool is an ordinary tool: what it wrote is on the card like any
  // other call's arguments — the whole body of a new document, or the passage an
  // edit replaced and what replaced it.
  const rows: KvRow[] = [
    [t("范围", "Tier"), tier],
    [t("文档", "Document"), compact(inputString(item, "name"), 160)],
    [t("索引描述", "Index description"), compact(inputString(item, "description"), 300)],
    [t("内容", "Content"), inputString(item, "content")],
    [t("原文", "Old text"), inputString(item, "old_text")],
    [t("新文", "New text"), inputString(item, "new_text")]
  ];
  return <KeyValueCard rows={rows} result={item.result.output} />;
}

/**
 * A call shown as the arguments worth reading and then what it answered.
 *
 * `presentation.keys` names those arguments and their order. A tool that names
 * none has had none curated, so all of them show rather than none — an
 * uncurated argument is still the reader's only account of the call.
 */
function RawView({ item, presentation }: { item: ToolContext; presentation?: ToolPresentation }) {
  const names = presentation?.keys ?? Object.keys(item.input);
  return (
    <KeyValueCard
      rows={names.map((name) => [name, rawFieldText(item.input[name])])}
      result={item.result.output}
    />
  );
}

function runStatusLabel(status: ReturnType<typeof agentTimelineRunStatus>, t: Translate): string {
  switch (status) {
    case "running": return t("运行中", "Running");
    case "completed": return t("已完成", "Completed");
    case "failed": return t("已失败", "Failed");
    case "stopped": return t("已停止", "Stopped");
    case "roundLimit": return t("已达轮次上限", "Round limit reached");
    default: return t("已中断", "Interrupted");
  }
}

/**
 * The updates a child pushed onto this call while it ran.
 *
 * `live.updates` is the streaming feed and `subagent.updates` the persisted
 * record; a call in the handoff window carries both, with the same content in
 * each, so identical text collapses to its latest occurrence.
 */
function runUpdates(item: ToolContext): SubagentUpdate[] {
  const latest = new Map<string, SubagentUpdate>();
  for (const update of [...(item.live?.updates ?? []), ...(item.subagent?.updates ?? [])]) {
    const key = update.content.replace(/\s+/g, " ").trim();
    if (!key) continue;
    const previous = latest.get(key);
    if (!previous || Date.parse(previous.createdAt) <= Date.parse(update.createdAt)) {
      latest.set(key, update);
    }
  }
  return [...latest.values()].sort(
    (left, right) => (Date.parse(left.createdAt) || 0) - (Date.parse(right.createdAt) || 0)
  );
}

/** A call that owns a child run: what it was asked, what it said, what it returned. */
function AgentRunView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const instruction = initialMessage(item) || item.subagent?.task || "";
  const updates = runUpdates(item);
  return (
    <KeyValueCard
      rows={[
        [t("子代理", "Subagent"), item.subagent?.label || item.subagent?.name || ""],
        [t("状态", "Status"), runStatusLabel(agentTimelineRunStatus(item), t)],
        [t("派发指令", "Instruction"), instruction],
        [t("子代理更新", "Subagent updates"), updates.map((update) => update.content).join("\n")]
      ]}
      result={item.result.output}
    />
  );
}

/**
 * `task_wait` output split back into the `[name · status]` envelopes it
 * drained: who answered and how it stands on a line of its own, over what it
 * said. A name is as long as its author made it, so it is not a column.
 */
function AgentWaitView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const { envelopes, statusLine, notice } = parseWaitOutput(item.result.output);
  if (!envelopes.length) {
    return notice
      ? <KeyValueCard rows={[]} result={item.result.output} />
      : <EmptyResult>{t("等待已结束，没有可显示的更新", "The wait finished with no updates to show")}</EmptyResult>;
  }
  return (
    <div className="tool-renderer__lines">
      {notice && <p className="tool-renderer__note">{notice}</p>}
      {envelopes.map((envelope, index) => (
        <section key={`${index}:${envelope.agent}`}>
          <div className="tool-renderer__dim">{`${envelope.agent} · ${envelope.status}`}</div>
          {envelope.body}
        </section>
      ))}
      {statusLine && <p className="tool-renderer__note">{statusLine}</p>}
    </div>
  );
}

/**
 * A one-shot agent message (`subagent_update`, `structured_output`,
 * `task_list`, …). Its payload is already the row's target, so the detail only
 * has to show the full untruncated text and the receipt the host wrote back.
 */
function AgentNoteView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  const message = inputString(item, "message")
    ?? inputString(item, "content")
    ?? inputString(item, "status");
  return <KeyValueCard rows={[[t("内容", "Message"), message]]} result={item.result.output} />;
}

/**
 * One site chip: its icon, or the first letter of its domain while the icon is
 * still being fetched or when the site has none.
 *
 * The host does the fetching (`webSourceIcon`), because the application's CSP
 * allows `img-src 'self' data:` and nothing outbound. The placeholder is not a
 * loading state worth animating — most chips resolve in one paint from cache,
 * and a spinner on each of six chips would be the loudest thing on the card.
 */
function WebSourceIcon({ chip }: { chip: WebSourceChip }) {
  const [source, setSource] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    siteIcon(chip.url).then((icon) => {
      if (active) setSource(icon);
    });
    return () => { active = false; };
  }, [chip.url]);
  if (!source) {
    return <span className="tool-renderer__source-letter" aria-hidden="true">
      {chip.host.slice(0, 1).toUpperCase()}
    </span>;
  }
  return <img
    className="tool-renderer__source-icon"
    src={source}
    alt=""
    aria-hidden="true"
    loading="lazy"
    decoding="async"
  />;
}

/**
 * A settled `web_search` card: the sites it consulted, then what it found.
 *
 * The chips come first because they are the part a reader can check. The prose
 * under them is a model's summary for a native backend and the providers' own
 * snippets for a catalog backend; either way the domains say where it came from,
 * which is the question a search result actually has to answer.
 */
function WebSearchView({ item, presentation }: { item: ToolContext; presentation: ToolPresentation }) {
  const { t } = useI18n();
  const chips = webSourceChips(item);
  const parsed = parseJsonOutput(item);
  const envelope = object(parsed.value);
  const findings = typeof envelope?.findings === "string" ? envelope.findings.trim() : "";
  const results = Array.isArray(envelope?.results) ? envelope.results : [];
  if (!chips.length && !findings && !results.length) {
    return <RawView item={item} presentation={presentation} />;
  }
  return (
    <div className="tool-renderer__web-search">
      {chips.length > 0 && (
        <nav className="tool-renderer__sources" aria-label={t("搜索到的网站", "Sites searched")}>
          {chips.map((chip) => (
            <a
              key={chip.host}
              className="tool-renderer__source"
              href={chip.url}
              // The title carries the page title and full URL: the chip itself
              // shows only the domain, and a reader who wants the rest should
              // not have to open the page to see it.
              title={chip.title ? `${chip.title}\n${chip.url}` : chip.url}
              target="_blank"
              rel="noreferrer noopener"
            >
              <WebSourceIcon chip={chip} />
              <span className="tool-renderer__source-host">{chip.host}</span>
            </a>
          ))}
        </nav>
      )}
      {findings && <div className="tool-renderer__findings">{findings}</div>}
      {results.length > 0 && (
        <ol className="tool-renderer__result-list">
          {results.map((entry, index) => {
            const fields = object(entry);
            const title = typeof fields?.title === "string" ? fields.title.trim() : "";
            const url = typeof fields?.url === "string" ? fields.url.trim() : "";
            const content = typeof fields?.content === "string" ? fields.content.trim() : "";
            return (
              <li key={`${index}:${url}`}>
                <strong>{title || url || t("无标题", "Untitled")}</strong>
                {content && <p>{content}</p>}
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}

function RunningView({ item }: { item: ToolContext }) {
  const { t } = useI18n();
  return (
    <div className="tool-renderer__running" role="status">
      <Timer size={14} aria-hidden="true" />
      <span>{runningLabel(item, t)}</span>
    </div>
  );
}

function runningLabel(item: ToolContext, t: Translate): string {
  if (item.streamStatus === "announced") return t("正在准备参数", "Preparing arguments");
  if (item.streamStatus === "ready") return t("等待执行", "Waiting to run");
  return t("正在执行", "Running");
}

/**
 * The families whose card is a picture of what a call that worked returned: a
 * file's numbered lines, a diff, a listing, a parsed report. An error message
 * is none of those — drawn as one it would be a file one line long, or a
 * directory holding a sentence — so a failed call of these opens to the shared
 * card instead: what it was asked, then what it answered.
 */
const RESULT_PICTURE_FAMILIES: ReadonlySet<ToolViewFamily> = new Set<ToolViewFamily>([
  "file-list",
  "grep",
  "read",
  "diff",
  "browser-snapshot",
  "browser-screenshot",
  "browser-console",
  "browser-network",
  "browser-page",
  "agent-wait"
]);

function detailBody(
  item: ToolContext,
  presentation: ToolPresentation
): ReactNode {
  switch (presentation.family) {
    case "file-list": return <FileListView item={item} />;
    case "grep": return <GrepView item={item} />;
    case "read": return <ReadView item={item} />;
    case "diff": return <DiffView item={item} />;
    case "terminal": return <TerminalView item={item} />;
    case "browser-snapshot": return <BrowserSnapshotView item={item} />;
    case "browser-evaluate": return <BrowserEvaluateView item={item} />;
    case "browser-screenshot": return <BrowserScreenshotView item={item} />;
    case "browser-console": return <BrowserConsoleView item={item} />;
    case "browser-network": return <BrowserNetworkView item={item} />;
    case "browser-json": return <BrowserJsonView item={item} presentation={presentation} />;
    case "browser-page": return <BrowserPageView item={item} />;
    case "memory": return <MemoryView item={item} />;
    case "agent-run": return <AgentRunView item={item} />;
    case "agent-wait": return <AgentWaitView item={item} />;
    case "agent-note": return <AgentNoteView item={item} />;
    case "web-search": return <WebSearchView item={item} presentation={presentation} />;
    default: return <RawView item={item} presentation={presentation} />;
  }
}

export function ToolDetailRenderer({
  item,
  descriptor
}: {
  item: ToolContext;
  descriptor?: ToolDescriptor;
}) {
  const { t } = useI18n();
  const presentation = getToolPresentation(item, descriptor, t);
  const phase = executionPhase(item);
  // A result carrying images is the image: its receipt text and screenshot
  // metadata only repeat the thumbnail and collapsed summary. Render only the
  // thumbnail and let its viewer carry detail. Failures retain their messages.
  const imageOnly = phase === "done" && (item.result.images?.length ?? 0) > 0;
  // A failed call opens to an ordinary card, not to an alert: the row's title
  // is what says it failed, and the card is the account of it — a command and
  // what it printed, the arguments and the error they met.
  const answered = phase === "failed" && RESULT_PICTURE_FAMILIES.has(presentation.family)
    ? <RawView item={item} presentation={presentation} />
    : detailBody(item, presentation);
  const label = `${presentation.title}${presentation.target ? `：${presentation.target}` : ""}`;
  return (
    <section
      className={`tool-renderer tool-renderer--${presentation.family}`}
      aria-label={label}
      data-tool-name={item.toolName}
      data-tool-family={presentation.family}
    >
      <div className="tool-renderer__body">
        {phase !== "running" && (
          <ImageStrip images={item.result.images} className="tool-renderer__images" />
        )}
        {phase === "running"
          ? <RunningView item={item} />
          : imageOnly
            ? null
            : answered}
      </div>
      <RawDataDisclosure item={item} />
    </section>
  );
}
