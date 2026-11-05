//! `read`, `write`, `edit`, `ls`, `find`, `grep`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use agent_rt::Cancel;
use globset::{Glob, GlobMatcher};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{WalkBuilder, WalkState};
use memchr::{memchr, memchr_iter, memrchr};
use parking_lot::Mutex;
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

use super::{
    MAX_PATH_CHARS, Tool, ToolCtx, ToolFuture, ToolResult, allow_outside_read, blocking, candidate_path, canonical_workspace, display_path, fit, future, modified_ms,
    optional_bool, optional_string, optional_u64, required_string, resolve_existing, truncate_chars, workspace_root,
};
use crate::model::ToolMeta;
use crate::prompt::{render, text};
use crate::tasks::{FileStamp, Session};

pub(super) const MAX_TEXT_FILE: u64 = 2 * 1024 * 1024;
const MAX_WRITE_BYTES: usize = 2 * 1024 * 1024;
const MAX_DIFF_OUTPUT: usize = 64 * 1024;

/// Claude Code's two read-gate refusals, byte for byte.
const FILE_NOT_READ: &str = "File has not been read yet. Read it first before writing to it.";
const FILE_MODIFIED_SINCE_READ: &str = "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.";

const VCS_DIRECTORIES: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];
/// What stands in for an ignore file outside a Git work tree.
const DEPENDENCY_DIRECTORIES: &[&str] = &[
    "node_modules",
    "bower_components",
    "vendor",
    "venv",
    ".venv",
    ".tox",
    "__pycache__",
    "target",
    "build",
    "_build",
    ".build",
    "dist",
    "dist-newstyle",
    "obj",
    "deps",
    ".gradle",
    ".dart_tool",
    ".pub-cache",
    ".deno",
    ".next",
    ".nuxt",
    ".svelte-kit",
];
const LS_BUDGET_CHARS: usize = 40_000;
const FIND_LIMIT: usize = 100;
const FIND_SCAN_LIMIT: usize = 200_000;
const GREP_DEFAULT_LIMIT: usize = 250;
const GREP_MAX_LIMIT: usize = 1_000;
const GREP_MAX_OFFSET: usize = 100_000;
const GREP_MAX_CONTEXT: usize = 20;
const GREP_LINE_CHARS: usize = 500;
const GREP_INLINE_CHARS: usize = 20_000;
const SKIPPED_SHOWN: usize = 20;
/// Smaller files are read: below this a mapping costs more than the copy it saves.
const MMAP_AT: u64 = 256 << 10;

pub(super) fn register(v: &mut Vec<Arc<dyn Tool>>) {
    v.push(Arc::new(ReadTool));
    v.push(Arc::new(WriteTool));
    v.push(Arc::new(EditTool));
    v.push(Arc::new(LsTool));
    v.push(Arc::new(FindTool));
    v.push(Arc::new(GrepTool));
}

fn path_summary(args: &Value) -> String {
    args.get("path").and_then(Value::as_str).unwrap_or(".").to_owned()
}

fn stamp_of(meta: &fs::Metadata) -> FileStamp {
    FileStamp { mtime_ms: modified_ms(meta), len: meta.len() }
}

// ---------------------------------------------------------------- read

struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn dangerous(&self, _: &Value) -> bool {
        false
    }
    fn parallel_safe(&self, _: &Value) -> bool {
        true
    }
    fn summary(&self, args: &Value) -> String {
        path_summary(args)
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            let path = required_string(&args, "path", MAX_PATH_CHARS, false)?;
            let (start, end) = parse_read_range(&args)?;
            let workspace = workspace_root(&ctx, &args)?;
            let requested = path.clone();
            let target = blocking(move || canonical_workspace(&workspace).and_then(|w| resolve_existing(&w, &requested))).await??;
            allow_outside_read(&ctx, "read", &args, &target).await?;
            let session = ctx.session.clone();
            let model = ctx.spec.model.id.clone();
            blocking(move || read_file(&session, &target, &path, start, end, &model)).await?
        })
    }
}

fn parse_read_range(args: &Value) -> Result<(u64, u64), String> {
    let start = optional_u64(args, "start_line")?.unwrap_or(1).max(1);
    let end = optional_u64(args, "end_line")?.filter(|e| *e > 0).unwrap_or(u64::MAX);
    if end < start {
        return Err("end_line cannot be less than start_line".to_owned());
    }
    Ok((start, end))
}

fn is_supported_image(head: &[u8]) -> bool {
    head.starts_with(b"\x89PNG\r\n\x1a\n")
        || head.starts_with(&[0xff, 0xd8, 0xff])
        || head.starts_with(b"GIF87a")
        || head.starts_with(b"GIF89a")
        || (head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP")
}

fn read_file(session: &Session, target: &Path, path: &str, start: u64, end: u64, model: &str) -> Result<ToolResult, String> {
    // Before the open, which on a FIFO would wait for a writer that may never come.
    let meta = fs::metadata(target).map_err(|e| format!("Failed to read file metadata: {e}"))?;
    if !meta.is_file() {
        return Err(format!("read target is not a file: {path}"));
    }
    let mut file = File::open(target).map_err(|e| format!("Could not open file {}: {e}", target.display()))?;
    let mut head = Vec::with_capacity(12);
    Read::by_ref(&mut file).take(12).read_to_end(&mut head).map_err(|e| format!("Failed to inspect file {}: {e}", target.display()))?;
    if is_supported_image(&head) {
        return Err(format!(
            "Tool read ran and returned 1 images, but the current model {model} does not support image input; the images were not added to context. Switch to a vision-capable model and retry."
        ));
    }
    // The stamp is the metadata from before the bytes were read: if the file changes in between, the
    // next edit reads as stale, which is the safe side.
    file.rewind().map_err(|e| format!("Failed to rewind {}: {e}", target.display()))?;
    let output = read_lines(file, meta.len(), path, start, end)?;
    session.reads.lock().insert(target.to_owned(), stamp_of(&meta));
    Ok(ToolResult::ok(output))
}

/// Lines a `read` without `end_line` returns.
const READ_DEFAULT_LINES: u64 = 2_000;
const READ_MAX_LINES: u64 = 5_001;
const READ_MAX_BYTES: usize = 60 * 1024;
/// How much of the start of a file is looked at for a NUL.
const SNIFF_BYTES: usize = 8 * 1024;
/// Up to this size a read that stops short goes on to count the rest, to name the total.
const COUNT_SCAN_LIMIT: u64 = 64 * 1024 * 1024;
/// A range deeper than this is what `tail` and `sed` are for.
const SKIP_SCAN_LIMIT: u64 = 1024 * 1024 * 1024;
const CUT_MARKER_RESERVE: usize = 128;
const IO_CHUNK: usize = 64 * 1024;

/// A window of lines out of a file of any size: only what is shown is ever held.
fn read_lines<R: Read>(source: R, size: u64, path: &str, start: u64, end: u64) -> Result<String, String> {
    let io_error = |e: io::Error| format!("Failed to read text file {path}: {e}");
    let (mut reader, encoding) = open_text(source, path)?;
    let reader: &mut dyn BufRead = &mut *reader;
    let unbounded = end == u64::MAX;
    let before = start - 1;
    let mut scanned = 0u64;
    let (skipped, dangling) = skip_lines(reader, before, &mut scanned).map_err(io_error)?;
    if scanned > SKIP_SCAN_LIMIT {
        return Err(format!(
            "Line {start} is more than {} GiB into the file. Use a shell command such as tail or sed to reach it.",
            SKIP_SCAN_LIMIT / 1024 / 1024 / 1024
        ));
    }
    if skipped < before {
        return Ok(nothing(skipped + u64::from(dangling)));
    }
    let wanted = if unbounded { READ_DEFAULT_LINES } else { (end - before).min(READ_MAX_LINES) };
    let mut shown: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut lossy = false;
    let mut at_eof = false;
    let mut unfit = 0u64;
    while (shown.len() as u64) < wanted {
        let Some(line) = read_line(reader, READ_MAX_BYTES).map_err(io_error)? else {
            at_eof = true;
            break;
        };
        let content = match std::str::from_utf8(&line.bytes) {
            Ok(s) => Cow::Borrowed(s),
            Err(_) => {
                lossy = true;
                String::from_utf8_lossy(&line.bytes)
            }
        };
        if line.dropped_bytes == 0 && content.len() < READ_MAX_BYTES {
            let cost = content.len() + 1;
            if used + cost > READ_MAX_BYTES {
                unfit = 1;
                break;
            }
            used += cost;
            shown.push(content.into_owned());
        } else if shown.is_empty() {
            // Longer than a whole read can return: cut it and say how much was left, rather than refuse the file for one line.
            let mut cut = (READ_MAX_BYTES - 1 - CUT_MARKER_RESERVE).min(content.len());
            while !content.is_char_boundary(cut) {
                cut -= 1;
            }
            let omitted = content[cut..].chars().count() as u64 + line.dropped_chars;
            let mut kept = content[..cut].to_owned();
            kept.push_str(&render("tool.read_line_truncated", &[("omitted", &omitted.to_string())]));
            shown.push(kept);
            used = READ_MAX_BYTES;
        } else {
            unfit = 1;
            break;
        }
    }
    if shown.is_empty() {
        return Ok(nothing(before));
    }
    let emitted = shown.len() as u64;
    let reached_end = !unbounded && emitted == end - before;
    if !at_eof && unfit == 0 && !reached_end {
        at_eof = reader.fill_buf().map_err(io_error)?.is_empty();
    }
    let stopped_early = !at_eof && !reached_end;
    let mut output = shown.join("\n");
    if lossy {
        output.push_str(text("tool.read_lossy_note"));
    }
    if encoding != Encoding::Utf8 {
        output.push_str(&render("tool.read_utf16_note", &[("encoding", encoding.label())]));
    }
    if stopped_early {
        let last = before + emitted;
        let (from, to, next) = (start.to_string(), last.to_string(), (last + 1).to_string());
        if size <= COUNT_SCAN_LIMIT {
            let total = last + unfit + count_lines(reader).map_err(io_error)?;
            output.push_str(&render("tool.read_limit", &[("from", &from), ("to", &to), ("total", &total.to_string()), ("next", &next)]));
        } else {
            output.push_str(&render("tool.read_limit_uncounted", &[("from", &from), ("to", &to), ("next", &next)]));
        }
    }
    Ok(output)
}

fn nothing(total: u64) -> String {
    if total == 0 { text("tool.read_empty_file").to_owned() } else { render("tool.read_range_out_of_bounds", &[("total", &total.to_string())]) }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
        }
    }
}

fn open_text<'a, R: Read + 'a>(mut source: R, path: &str) -> Result<(Box<dyn BufRead + 'a>, Encoding), String> {
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    (&mut source).take(SNIFF_BYTES as u64).read_to_end(&mut head).map_err(|e| format!("Failed to read text file {path}: {e}"))?;
    let (encoding, mark) = match head.as_slice() {
        [0xff, 0xfe, ..] => (Encoding::Utf16Le, 2),
        [0xfe, 0xff, ..] => (Encoding::Utf16Be, 2),
        _ => (Encoding::Utf8, 0),
    };
    // UTF-16 text is full of NULs; the mark is what says it is text.
    if encoding == Encoding::Utf8 && memchr(0, &head).is_some() {
        return Err(render("tool.read_binary", &[("path", path)]));
    }
    let mut head = Cursor::new(head);
    head.set_position(mark);
    let joined = head.chain(source);
    Ok(match encoding {
        Encoding::Utf8 => (Box::new(BufReader::with_capacity(IO_CHUNK, joined)), encoding),
        _ => (Box::new(BufReader::with_capacity(IO_CHUNK, Utf16Decoder::new(joined, encoding == Encoding::Utf16Be))), encoding),
    })
}

struct Line {
    bytes: Vec<u8>,
    dropped_bytes: u64,
    dropped_chars: u64,
}

/// The next line without its terminator, or `None` at the end. Memory is bounded by `cap` whatever the line's length.
fn read_line(reader: &mut dyn BufRead, cap: usize) -> io::Result<Option<Line>> {
    let mut line = Line { bytes: Vec::new(), dropped_bytes: 0, dropped_chars: 0 };
    let mut any = false;
    let mut ended = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        any = true;
        let (take, found) = match memchr(b'\n', chunk) {
            Some(i) => (i, true),
            None => (chunk.len(), false),
        };
        let keep = cap.saturating_sub(line.bytes.len()).min(take);
        line.bytes.extend_from_slice(&chunk[..keep]);
        line.dropped_bytes += (take - keep) as u64;
        line.dropped_chars += chunk[keep..take].iter().filter(|b| (**b & 0xc0) != 0x80).count() as u64;
        reader.consume(take + usize::from(found));
        if found {
            ended = true;
            break;
        }
    }
    if !any {
        return Ok(None);
    }
    if line.dropped_bytes > 0 {
        drop_partial_character(&mut line);
    } else if ended && line.bytes.last() == Some(&b'\r') {
        line.bytes.pop();
    }
    Ok(Some(line))
}

/// A cut line must not end halfway through a character: that half would read as U+FFFD and make a valid file look lossy.
fn drop_partial_character(line: &mut Line) {
    let length = line.bytes.len();
    if length == 0 {
        return;
    }
    let mut start = length;
    while start > 0 && length - start < 4 {
        start -= 1;
        if (line.bytes[start] & 0xc0) != 0x80 {
            break;
        }
    }
    let needs = match line.bytes.get(start).copied() {
        Some(0xc0..=0xdf) => 2,
        Some(0xe0..=0xef) => 3,
        Some(0xf0..=0xf7) => 4,
        _ => 1,
    };
    if length - start < needs {
        line.dropped_bytes += (length - start) as u64;
        line.dropped_chars += 1;
        line.bytes.truncate(start);
    }
}

fn skip_lines(reader: &mut dyn BufRead, count: u64, scanned: &mut u64) -> io::Result<(u64, bool)> {
    let mut skipped = 0u64;
    let mut dangling = false;
    while skipped < count {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let mut used = 0;
        while skipped < count && used < chunk.len() {
            match memchr(b'\n', &chunk[used..]) {
                Some(i) => {
                    used += i + 1;
                    skipped += 1;
                    dangling = false;
                }
                None => {
                    used = chunk.len();
                    dangling = true;
                }
            }
        }
        reader.consume(used);
        *scanned += used as u64;
        if *scanned > SKIP_SCAN_LIMIT {
            break;
        }
    }
    Ok((skipped, dangling))
}

fn count_lines(reader: &mut dyn BufRead) -> io::Result<u64> {
    let mut lines = 0u64;
    let mut last = b'\n';
    loop {
        let chunk = reader.fill_buf()?;
        let Some(final_byte) = chunk.last().copied() else { break };
        lines += memchr_iter(b'\n', chunk).count() as u64;
        last = final_byte;
        let length = chunk.len();
        reader.consume(length);
    }
    Ok(lines + u64::from(last != b'\n'))
}

/// UTF-16 read as UTF-8, a chunk at a time. A lone surrogate or a stray last byte becomes U+FFFD.
struct Utf16Decoder<R: Read> {
    inner: R,
    big_endian: bool,
    odd_byte: Option<u8>,
    high_surrogate: Option<u16>,
    decoded: Vec<u8>,
    position: usize,
    finished: bool,
}

impl<R: Read> Utf16Decoder<R> {
    fn new(inner: R, big_endian: bool) -> Self {
        Self { inner, big_endian, odd_byte: None, high_surrogate: None, decoded: Vec::new(), position: 0, finished: false }
    }

    fn push(&mut self, c: char) {
        let mut buffer = [0u8; 4];
        self.decoded.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
    }

    fn push_unit(&mut self, unit: u16) {
        match self.high_surrogate.take() {
            Some(high) if (0xdc00..=0xdfff).contains(&unit) => {
                let scalar = 0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00);
                self.push(char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            Some(_) => {
                self.push(char::REPLACEMENT_CHARACTER);
                self.push_unit(unit);
            }
            None => match unit {
                0xd800..=0xdbff => self.high_surrogate = Some(unit),
                0xdc00..=0xdfff => self.push(char::REPLACEMENT_CHARACTER),
                _ => self.push(char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER)),
            },
        }
    }

    fn refill(&mut self) -> io::Result<()> {
        self.decoded.clear();
        self.position = 0;
        let mut chunk = [0u8; 8192];
        let read = self.inner.read(&mut chunk)?;
        if read == 0 {
            self.finished = true;
            if self.high_surrogate.take().is_some() || self.odd_byte.take().is_some() {
                self.push(char::REPLACEMENT_CHARACTER);
            }
            return Ok(());
        }
        for &byte in &chunk[..read] {
            match self.odd_byte.take() {
                None => self.odd_byte = Some(byte),
                Some(first) => {
                    let unit = if self.big_endian { u16::from_be_bytes([first, byte]) } else { u16::from_le_bytes([first, byte]) };
                    self.push_unit(unit);
                }
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for Utf16Decoder<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        while self.position >= self.decoded.len() {
            if self.finished {
                return Ok(0);
            }
            self.refill()?;
        }
        let available = &self.decoded[self.position..];
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.position += count;
        Ok(count)
    }
}

// ---------------------------------------------------------------- the read gate

/// Whether an existing file may be overwritten, given what this conversation last saw of it.
fn check_write_gate(session: &Session, path: &Path) -> Result<(), String> {
    let Some(stamp) = session.reads.lock().get(path).copied() else {
        return Err(FILE_NOT_READ.to_owned());
    };
    let Ok(meta) = fs::metadata(path) else {
        return Ok(());
    };
    if stamp_of(&meta) != stamp {
        return Err(FILE_MODIFIED_SINCE_READ.to_owned());
    }
    Ok(())
}

fn record_written(session: &Session, path: &Path) {
    // A file that cannot be stat'ed after its own write gets no record: the next edit asks for a read.
    match fs::metadata(path) {
        Ok(meta) => {
            session.reads.lock().insert(path.to_owned(), stamp_of(&meta));
        }
        Err(_) => {
            session.reads.lock().remove(path);
        }
    }
}

// ---------------------------------------------------------------- write

fn read_text_file(path: &Path) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|e| format!("Failed to read file metadata: {e}"))?;
    if meta.len() > MAX_TEXT_FILE {
        return Err(format!("Text file exceeds the {} MiB limit", MAX_TEXT_FILE / 1024 / 1024));
    }
    fs::read_to_string(path).map_err(|e| format!("Failed to read text file as UTF-8: {e}"))
}

/// Temp file, sync, rename, keeping the mode of what it replaces: an agent's edit must not take a script's executable bit.
pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("The path has no parent directory")?;
    let name = path.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy());
    let temp = parent.join(format!(".{name}.tmp-{}-{}", std::process::id(), crate::model::new_id()));
    let mode = fs::metadata(path).ok().map(|m| m.permissions());
    let write = || -> io::Result<()> {
        let mut file = File::options().write(true).create_new(true).open(&temp)?;
        file.write_all(bytes)?;
        if let Some(mode) = mode.clone() {
            file.set_permissions(mode)?;
        }
        file.sync_all()?;
        fs::rename(&temp, path)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&temp);
        format!("Failed to write file: {e}")
    })
}

/// Where a write lands: the real file when it exists, or the new name inside its real directory, spelled like a read's record of it.
fn resolve_for_write(workspace: &Path, requested: &str) -> Result<PathBuf, String> {
    let candidate = candidate_path(workspace, requested)?;
    if let Ok(existing) = fs::canonicalize(&candidate) {
        return Ok(existing);
    }
    let mut missing = Vec::new();
    let mut walk = candidate.as_path();
    loop {
        let Some(name) = walk.file_name() else { return Ok(candidate.clone()) };
        missing.push(name.to_owned());
        let Some(parent) = walk.parent() else { return Ok(candidate.clone()) };
        if let Ok(mut real) = fs::canonicalize(parent) {
            real.extend(missing.iter().rev());
            return Ok(real);
        }
        walk = parent;
    }
}

fn truncate_diff(diff: String) -> String {
    if diff.len() <= MAX_DIFF_OUTPUT {
        return diff;
    }
    let marker = format!("\n{}", text("tool.diff_truncated"));
    let mut end = MAX_DIFF_OUTPUT.saturating_sub(marker.len());
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &diff[..end])
}

fn change_meta(path: &str, before: &str, after: &str, created: bool) -> ToolMeta {
    let mut meta = ToolMeta { file: Some(path.to_owned()), ..Default::default() };
    if before == after {
        return meta;
    }
    // A file of pathological churn gets a coarser diff rather than a stalled tool.
    let diff = TextDiff::configure().timeout(Duration::from_secs(1)).diff_lines(before, after);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => meta.added += 1,
            ChangeTag::Delete => meta.removed += 1,
            ChangeTag::Equal => {}
        }
    }
    // Header values go in verbatim, so a filename must not be able to inject diff control lines.
    let shown = path.replace('\\', "/").replace(['\r', '\n', '\t'], " ");
    let old = if created { "/dev/null" } else { &shown };
    meta.diff = Some(truncate_diff(diff.unified_diff().context_radius(3).header(old, &shown).to_string()));
    meta
}

struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn dangerous(&self, _: &Value) -> bool {
        true
    }
    fn blocked_in_plan_mode(&self, _: &Value) -> bool {
        true
    }
    fn summary(&self, args: &Value) -> String {
        path_summary(args)
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            let path = required_string(&args, "path", MAX_PATH_CHARS, false)?;
            let content = required_string(&args, "content", MAX_WRITE_BYTES, true)?;
            if content.len() > MAX_WRITE_BYTES {
                return Err(format!("Write content exceeds the {} MiB limit", MAX_WRITE_BYTES / 1024 / 1024));
            }
            let workspace = workspace_root(&ctx, &args)?;
            let session = ctx.session.clone();
            blocking(move || run_write(&session, &workspace, &path, &content)).await?
        })
    }
}

fn run_write(session: &Session, workspace: &Path, path: &str, content: &str) -> Result<ToolResult, String> {
    let workspace = canonical_workspace(workspace)?;
    let target = resolve_for_write(&workspace, path)?;
    // Diff metadata is best-effort: overwriting a large or non-UTF-8 file stays valid.
    let before = match fs::metadata(&target) {
        Ok(meta) if meta.is_file() => read_text_file(&target).ok().map(|c| (c, false)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some((String::new(), true)),
        _ => None,
    };
    if !matches!(before, Some((_, true))) {
        check_write_gate(session, &target)?;
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create parent directory: {e}"))?;
    }
    atomic_write(&target, content.as_bytes())?;
    record_written(session, &target);
    let meta = match &before {
        Some((old, created)) => change_meta(path, old, content, *created),
        None => ToolMeta { file: Some(path.to_owned()), ..Default::default() },
    };
    Ok(ToolResult::ok(format!("{}{}", text("tool.write_done"), text("tool.file_state_current"))).with_meta(meta))
}

// ---------------------------------------------------------------- edit

const EDIT_FIND_EQUALS_REPLACE: &str = "No changes to make: find and replace are exactly the same.";
const EDIT_LEAVES_FILE_UNCHANGED: &str = "No changes to make: the edit leaves the file unchanged.";
const EDIT_NOT_FOUND: &str = "The exact text to replace was not found";

struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn dangerous(&self, _: &Value) -> bool {
        true
    }
    fn blocked_in_plan_mode(&self, _: &Value) -> bool {
        true
    }
    fn summary(&self, args: &Value) -> String {
        path_summary(args)
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            let path = required_string(&args, "path", MAX_PATH_CHARS, false)?;
            let find = required_string(&args, "find", MAX_WRITE_BYTES, true)?;
            if find.is_empty() {
                return Err("Parameter find cannot be empty".to_owned());
            }
            let replace = required_string(&args, "replace", MAX_WRITE_BYTES, true)?;
            // Claude Code takes the string forms for its own `replace_all`, so a model may send either.
            let replace_all = match args.get("replace_all") {
                Some(Value::String(s)) if s == "true" => true,
                Some(Value::String(s)) if s == "false" => false,
                _ => optional_bool(&args, "replace_all", false)?,
            };
            if find == replace {
                return Err(EDIT_FIND_EQUALS_REPLACE.to_owned());
            }
            let workspace = workspace_root(&ctx, &args)?;
            let session = ctx.session.clone();
            blocking(move || run_edit(&session, &workspace, &path, &find, &replace, replace_all)).await?
        })
    }
}

fn run_edit(session: &Session, workspace: &Path, path: &str, find: &str, replace: &str, replace_all: bool) -> Result<ToolResult, String> {
    let workspace = canonical_workspace(workspace)?;
    let target = resolve_existing(&workspace, path)?;
    if !target.is_file() {
        return Err(format!("edit target is not a file: {}", target.display()));
    }
    check_write_gate(session, &target)?;
    let content = read_text_file(&target)?;
    let applied = apply_edit(&content, find, replace, replace_all)?;
    if applied.text.len() > MAX_WRITE_BYTES {
        return Err(format!("Edited file exceeds the {} MiB limit", MAX_WRITE_BYTES / 1024 / 1024));
    }
    atomic_write(&target, applied.text.as_bytes())?;
    record_written(session, &target);
    let meta = change_meta(path, &content, &applied.text, false);
    let receipt = if replace_all {
        render("tool.edit_done_all", &[("path", path), ("count", &applied.replacements.to_string())])
    } else {
        render("tool.edit_done", &[("path", path)])
    };
    Ok(ToolResult::ok(format!("{receipt}{}", text("tool.file_state_current"))).with_meta(meta))
}

#[derive(Debug, PartialEq, Eq)]
struct EditApplied {
    text: String,
    replacements: usize,
}

fn normalize_text(raw: &str) -> Cow<'_, str> {
    let stripped = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    if stripped.contains('\r') { Cow::Owned(stripped.replace("\r\n", "\n")) } else { Cow::Borrowed(stripped) }
}

/// Replaces `find` in `content`: exactly one occurrence, or every one under `replace_all`. The search
/// runs on the file as `read` showed it (no byte-order mark, `\r\n` as `\n`), first on the exact
/// text and then with curly quotes folded, and each hit is spliced into the raw file, which stays
/// byte for byte as it was outside the hits.
fn apply_edit(content: &str, find: &str, replace: &str, replace_all: bool) -> Result<EditApplied, String> {
    if find.is_empty() {
        return Err("Parameter find cannot be empty".to_owned());
    }
    if find == replace {
        return Err(EDIT_FIND_EQUALS_REPLACE.to_owned());
    }
    let view = EditView::of(content);
    let find = normalize_text(find);
    let replace = normalize_text(replace);
    if find.is_empty() {
        return Err(EDIT_NOT_FOUND.to_owned());
    }
    let mut hits: Vec<Range<usize>> = view.text.match_indices(find.as_ref()).map(|(at, found)| at..at + found.len()).collect();
    let mut folded = false;
    if hits.is_empty() && (has_curly_quote(&find) || (has_curly_quote(&view.text) && find.contains(['"', '\'']))) {
        hits = folded_hits(&view.text, &find);
        folded = true;
    }
    if hits.is_empty() {
        return Err(EDIT_NOT_FOUND.to_owned());
    }
    if hits.len() > 1 && !replace_all {
        return Err(ambiguous_edit_error(&view.text, &hits));
    }
    let spans: Vec<(Range<usize>, String)> = hits
        .iter()
        .map(|hit| {
            let with = if folded { restyle_quotes(&find, &view.text[hit.clone()], &replace) } else { replace.to_string() };
            let raw = view.raw_offset(hit.start)..view.raw_offset(hit.end);
            let with = if with.contains('\n') && crlf_at(content, &raw) { with.replace('\n', "\r\n") } else { with };
            (raw, with)
        })
        .collect();
    let edited = splice_edit(content, &spans);
    if edited == content {
        return Err(EDIT_LEAVES_FILE_UNCHANGED.to_owned());
    }
    Ok(EditApplied { text: edited, replacements: hits.len() })
}

struct EditView {
    text: String,
    bom: usize,
    /// The view offset of every `\n` whose `\r` was dropped, ascending.
    dropped: Vec<usize>,
}

impl EditView {
    fn of(raw: &str) -> Self {
        let bom = if raw.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
        let body = &raw[bom..];
        let mut text = String::with_capacity(body.len());
        let mut dropped = Vec::new();
        let mut copied = 0;
        for (at, _) in body.match_indices("\r\n") {
            text.push_str(&body[copied..at]);
            dropped.push(text.len());
            copied = at + 1;
        }
        text.push_str(&body[copied..]);
        Self { text, bom, dropped }
    }

    /// A dropped `\r` sits just before its `\n`: a span starting at that `\n` starts at the `\r`, one ending right after it takes the whole `\r\n`.
    fn raw_offset(&self, at: usize) -> usize {
        at + self.bom + self.dropped.partition_point(|&line_break| line_break < at)
    }
}

fn crlf_dominant(content: &str) -> bool {
    let sample: String = content.chars().take(4096).collect();
    let crlf = sample.matches("\r\n").count();
    let lf = sample.matches('\n').count() - crlf;
    crlf > lf
}

fn crlf_at(content: &str, hit: &Range<usize>) -> bool {
    let line_break = content[hit.clone()].find('\n').map(|at| hit.start + at).or_else(|| content[hit.end..].find('\n').map(|at| hit.end + at));
    match line_break {
        Some(at) => at > 0 && content.as_bytes()[at - 1] == b'\r',
        None => crlf_dominant(content),
    }
}

const LEFT_SINGLE_QUOTE: char = '\u{2018}';
const RIGHT_SINGLE_QUOTE: char = '\u{2019}';
const LEFT_DOUBLE_QUOTE: char = '\u{201c}';
const RIGHT_DOUBLE_QUOTE: char = '\u{201d}';

fn has_curly_quote(text: &str) -> bool {
    text.contains([LEFT_SINGLE_QUOTE, RIGHT_SINGLE_QUOTE, LEFT_DOUBLE_QUOTE, RIGHT_DOUBLE_QUOTE])
}

fn fold_quotes(text: &str) -> (String, Vec<usize>) {
    let mut folded = String::with_capacity(text.len());
    let mut at = Vec::new();
    for c in text.chars() {
        let straight = match c {
            LEFT_SINGLE_QUOTE | RIGHT_SINGLE_QUOTE => '\'',
            LEFT_DOUBLE_QUOTE | RIGHT_DOUBLE_QUOTE => '"',
            other => other,
        };
        if straight != c {
            at.push(folded.len());
        }
        folded.push(straight);
    }
    (folded, at)
}

fn folded_hits(text: &str, find: &str) -> Vec<Range<usize>> {
    let (folded, folded_at) = fold_quotes(text);
    let (find, _) = fold_quotes(find);
    if find.is_empty() {
        return Vec::new();
    }
    // Each curly quote is three bytes and folds to one, so an offset lies two bytes further on for every quote before it.
    let original = |offset: usize| offset + 2 * folded_at.partition_point(|&quote| quote < offset);
    folded.match_indices(find.as_str()).map(|(at, found)| original(at)..original(at + found.len())).collect()
}

fn splice_edit(text: &str, spans: &[(Range<usize>, String)]) -> String {
    let mut edited = String::with_capacity(text.len());
    let mut copied = 0;
    for (index, (hit, with)) in spans.iter().enumerate() {
        let mut end = hit.end;
        if with.is_empty() {
            let next = spans.get(index + 1).map_or(text.len(), |(next, _)| next.start);
            let line_break = deleted_line_break(text, hit);
            if end + line_break <= next {
                end += line_break;
            }
        }
        edited.push_str(&text[copied..hit.start]);
        edited.push_str(with);
        copied = end;
    }
    edited.push_str(&text[copied..]);
    edited
}

/// A deletion that removes a whole line's text takes its line break with it; one in the middle of a line never joins two lines.
fn deleted_line_break(text: &str, hit: &Range<usize>) -> usize {
    let before = &text[..hit.start];
    let starts_line = before.is_empty() || before == "\u{feff}" || before.ends_with('\n');
    if !starts_line || text[hit.clone()].ends_with('\n') {
        return 0;
    }
    let after = &text[hit.end..];
    if after.starts_with('\n') {
        1
    } else if after.starts_with("\r\n") {
        2
    } else {
        0
    }
}

fn ambiguous_edit_error(text: &str, hits: &[Range<usize>]) -> String {
    const LISTED: usize = 10;
    const CHARS_BEFORE: usize = 60;
    const CHARS_AFTER: usize = 100;
    let mut message = format!(
        "The search text occurs {} times; edit requires exactly one match. Include more surrounding lines in find to make it unique, or set replace_all to true to replace every occurrence.",
        hits.len()
    );
    let mut line = 1;
    let mut counted = 0;
    for hit in hits.iter().take(LISTED) {
        line += text[counted..hit.start].matches('\n').count();
        counted = hit.start;
        let start = text[..hit.start].rfind('\n').map_or(0, |at| at + 1);
        let end = text[hit.start..].find('\n').map_or(text.len(), |at| hit.start + at);
        let before = &text[start..hit.start];
        let column = before.chars().count() + 1;
        let skipped = (column - 1).saturating_sub(CHARS_BEFORE);
        let mut excerpt = String::new();
        if skipped > 0 {
            excerpt.push('…');
        }
        excerpt.extend(before.chars().skip(skipped));
        let mut after = text[hit.start..end].chars();
        excerpt.extend(after.by_ref().take(CHARS_AFTER));
        if after.next().is_some() {
            excerpt.push('…');
        }
        message.push_str(&format!("\n  line {line}, col {column}: {excerpt}"));
    }
    if hits.len() > LISTED {
        message.push_str(&format!("\n  … and {} more", hits.len() - LISTED));
    }
    message
}

/// `replace` restyled to the quotes of `actual`, the file's own text for one hit found with quotes
/// folded: what it keeps of `find` takes the file's glyph, and the rest follows the replaced span.
fn restyle_quotes(find: &str, actual: &str, replace: &str) -> String {
    let find: Vec<char> = find.chars().collect();
    let actual: Vec<char> = actual.chars().collect();
    let replace: Vec<char> = replace.chars().collect();
    if find.len() != actual.len() {
        return replace.into_iter().collect();
    }
    let fold = |c: char| QuoteKind::of(c).map_or(c, QuoteKind::straight);
    let prefix = find.iter().zip(&replace).take_while(|&(&a, &b)| fold(a) == fold(b)).count();
    let room = find.len().min(replace.len()) - prefix;
    let suffix = find.iter().rev().zip(replace.iter().rev()).take(room).take_while(|&(&a, &b)| fold(a) == fold(b)).count();
    let mut styled = replace.clone();
    for index in 0..prefix {
        if QuoteKind::of(replace[index]).is_some() {
            styled[index] = actual[index];
        }
    }
    for offset in 1..=suffix {
        let at = replace.len() - offset;
        if QuoteKind::of(replace[at]).is_some() {
            styled[at] = actual[actual.len() - offset];
        }
    }
    let middle = prefix..replace.len() - suffix;
    let replaced = &actual[prefix..actual.len() - suffix];
    for kind in [QuoteKind::Double, QuoteKind::Single] {
        let of_kind = |c: &&char| QuoteKind::of(**c) == Some(kind);
        let curly = if replaced.iter().any(|c| of_kind(&c)) {
            Some(replaced.iter().any(|&c| of_kind(&&c) && c != kind.straight()))
        } else {
            actual[..prefix].iter().rev().find(of_kind).or_else(|| actual[actual.len() - suffix..].iter().find(of_kind)).map(|&c| c != kind.straight())
        };
        match curly {
            None => {}
            Some(false) => {
                for index in middle.clone() {
                    if QuoteKind::of(replace[index]) == Some(kind) {
                        styled[index] = kind.straight();
                    }
                }
            }
            Some(true) => {
                let mut inside = quotation_open_at(kind, &actual, prefix);
                for index in middle.clone() {
                    if QuoteKind::of(replace[index]) != Some(kind) {
                        continue;
                    }
                    let after_word = index > 0 && is_word_char(replace[index - 1]);
                    styled[index] = if kind == QuoteKind::Single && after_word {
                        if !between_words(&replace, index) {
                            inside = false;
                        }
                        kind.closing()
                    } else if inside {
                        inside = false;
                        kind.closing()
                    } else {
                        inside = true;
                        kind.opening()
                    };
                }
            }
        }
    }
    styled.into_iter().collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuoteKind {
    Double,
    Single,
}

impl QuoteKind {
    fn of(c: char) -> Option<Self> {
        match c {
            '"' | LEFT_DOUBLE_QUOTE | RIGHT_DOUBLE_QUOTE => Some(Self::Double),
            '\'' | LEFT_SINGLE_QUOTE | RIGHT_SINGLE_QUOTE => Some(Self::Single),
            _ => None,
        }
    }

    fn straight(self) -> char {
        match self {
            Self::Double => '"',
            Self::Single => '\'',
        }
    }

    fn opening(self) -> char {
        match self {
            Self::Double => LEFT_DOUBLE_QUOTE,
            Self::Single => LEFT_SINGLE_QUOTE,
        }
    }

    fn closing(self) -> char {
        match self {
            Self::Double => RIGHT_DOUBLE_QUOTE,
            Self::Single => RIGHT_SINGLE_QUOTE,
        }
    }
}

fn quotation_open_at(kind: QuoteKind, actual: &[char], at: usize) -> bool {
    let mark = |index: usize| {
        let c = actual[index];
        let curly = c == kind.opening() || c == kind.closing();
        let apostrophe = kind == QuoteKind::Single && between_words(actual, index);
        (curly && !apostrophe).then_some(c == kind.opening())
    };
    let before: Vec<bool> = (0..at).filter_map(mark).collect();
    if !before.is_empty() {
        let opening = before.iter().filter(|&&opens| opens).count();
        return opening > before.len() - opening;
    }
    (at..actual.len()).find_map(mark).is_some_and(|opens| !opens)
}

fn between_words(chars: &[char], index: usize) -> bool {
    index > 0 && is_word_char(chars[index - 1]) && chars.get(index + 1).is_some_and(|&next| is_word_char(next))
}

/// A letter or digit an apostrophe can join to the next: not CJK, which sits directly against an opening quote as well as a closing one.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() && !matches!(u32::from(c), 0x2e80..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff | 0xff00..=0xffef | 0x20000..=0x3ffff)
}

// ---------------------------------------------------------------- what the walks leave out

fn is_vcs(name: &str) -> bool {
    VCS_DIRECTORIES.contains(&name)
}

/// Git's ignore rules for one work tree, each directory's file read once it is asked about.
struct GitRules {
    top: PathBuf,
    matchers: HashMap<PathBuf, Gitignore>,
    exclude: Gitignore,
    global: Gitignore,
}

impl GitRules {
    fn new(top: &Path) -> Self {
        let mut exclude = GitignoreBuilder::new(top);
        let _ = exclude.add(top.join(".git/info/exclude"));
        Self { top: top.to_owned(), matchers: HashMap::new(), exclude: exclude.build().unwrap_or_else(|_| Gitignore::empty()), global: Gitignore::global().0 }
    }

    fn matcher(&mut self, directory: &Path) -> &Gitignore {
        self.matchers.entry(directory.to_owned()).or_insert_with(|| {
            let mut builder = GitignoreBuilder::new(directory);
            let _ = builder.add(directory.join(".gitignore"));
            builder.build().unwrap_or_else(|_| Gitignore::empty())
        })
    }

    /// The nearest file with an opinion on this one path decides, as in Git.
    fn ignores_itself(&mut self, path: &Path, is_dir: bool) -> bool {
        let top = self.top.clone();
        for directory in path.ancestors().skip(1).take_while(|d| d.starts_with(&top)) {
            let found = self.matcher(directory).matched(path, is_dir);
            if found.is_ignore() {
                return true;
            }
            if found.is_whitelist() {
                return false;
            }
        }
        let found = self.exclude.matched(path, is_dir);
        if !found.is_none() {
            return found.is_ignore();
        }
        self.global.matched(path, is_dir).is_ignore()
    }

    /// Git covers everything below an ignored directory, and no pattern can bring it back.
    fn ignores(&mut self, path: &Path, is_dir: bool) -> bool {
        let top = self.top.clone();
        let Ok(relative) = path.strip_prefix(&top) else { return false };
        let mut walk = top.clone();
        let parts: Vec<_> = relative.components().collect();
        for (i, part) in parts.iter().enumerate() {
            walk.push(part);
            let last = i + 1 == parts.len();
            if self.ignores_itself(&walk, if last { is_dir } else { true }) {
                return true;
            }
        }
        false
    }
}

/// Which rule decides what is hidden below the directory a call named.
enum Rules {
    Git(Box<GitRules>),
    /// The directory was named although it is itself ignored: the model asked for it, so nothing below it is hidden.
    NamedRoot,
    /// No work tree: [`DEPENDENCY_DIRECTORIES`] stand in.
    DependencyNames,
}

impl Rules {
    fn for_root(root: &Path) -> Self {
        let Some(top) = root.ancestors().find(|a| fs::symlink_metadata(a.join(".git")).is_ok()) else {
            return Self::DependencyNames;
        };
        let mut git = GitRules::new(top);
        if root != top && git.ignores(root, true) {
            return Self::NamedRoot;
        }
        Self::Git(Box::new(git))
    }

    fn ignores(&mut self, root: &Path, path: &Path, is_dir: bool) -> bool {
        match self {
            Self::Git(git) => git.ignores(path, is_dir),
            Self::NamedRoot => false,
            Self::DependencyNames => {
                let relative = path.strip_prefix(root).unwrap_or(path);
                let mut parts: Vec<_> = relative.components().filter_map(|c| c.as_os_str().to_str()).collect();
                if !is_dir {
                    parts.pop();
                }
                parts.iter().any(|p| DEPENDENCY_DIRECTORIES.contains(p))
            }
        }
    }

    fn collapses(&mut self, root: &Path, path: &Path) -> bool {
        path.file_name().and_then(|n| n.to_str()).is_some_and(is_vcs) || self.ignores(root, path, true)
    }

    fn git_applies(&self) -> bool {
        matches!(self, Self::Git(_))
    }
}

fn skipped_lines(skipped: Vec<String>) -> Vec<String> {
    let more = skipped.len().saturating_sub(SKIPPED_SHOWN);
    let mut lines: Vec<String> = skipped.into_iter().take(SKIPPED_SHOWN).map(|e| render("tool.grep_skipped", &[("error", &e)])).collect();
    if more > 0 {
        lines.push(render("tool.grep_skipped", &[("error", &format!("… and {more} more"))]));
    }
    lines
}

/// Worker threads for a search: a walk is I/O and scanning is memory bandwidth, and neither gains much past this.
fn search_threads() -> usize {
    std::thread::available_parallelism().map_or(4, usize::from).min(16)
}

// ---------------------------------------------------------------- ls

struct LsTool;

impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }
    fn dangerous(&self, _: &Value) -> bool {
        false
    }
    fn parallel_safe(&self, _: &Value) -> bool {
        true
    }
    fn summary(&self, args: &Value) -> String {
        path_summary(args)
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            let path = optional_string(&args, "path", ".", MAX_PATH_CHARS)?;
            let depth = optional_u64(&args, "depth")?.unwrap_or(1).min(8) as usize;
            let workspace = workspace_root(&ctx, &args)?;
            let requested = path.clone();
            let (workspace, root) = blocking(move || -> Result<_, String> {
                let workspace = canonical_workspace(&workspace)?;
                let root = resolve_existing(&workspace, &requested)?;
                Ok((workspace, root))
            })
            .await??;
            allow_outside_read(&ctx, "ls", &args, &root).await?;
            blocking(move || run_ls(&workspace, &root, &path, depth).map(ToolResult::ok)).await?
        })
    }
}

fn run_ls(workspace: &Path, root: &Path, path: &str, depth: usize) -> Result<String, String> {
    if !root.is_dir() {
        return Err(format!("ls target is not a directory: {path}"));
    }
    let mut rules = Rules::for_root(root);
    // Breadth-first, each directory in name order, so the budget cuts the deepest level reached and a big subtree cannot push its siblings out.
    let mut listing = Listing::default();
    let mut frontier = vec![root.to_owned()];
    'levels: for level in 1..=depth + 1 {
        let mut next = Vec::new();
        for directory in &frontier {
            let mut children = match fs::read_dir(directory) {
                Ok(children) => children.filter_map(Result::ok).collect::<Vec<_>>(),
                Err(e) if directory.as_path() == root => return Err(format!("Failed to list directory {path}: {e}")),
                Err(e) => {
                    listing.skipped.push(format!("{}: {e}", display_path(workspace, directory)));
                    continue;
                }
            };
            children.sort_by_key(|c| c.file_name());
            for child in children {
                let child_path = child.path();
                // A link to a directory is listed, not followed, and so not marked as one.
                let is_dir = child.file_type().is_ok_and(|t| t.is_dir());
                let collapsed = is_dir && rules.collapses(root, &child_path);
                if !listing.push(level, &display_path(workspace, &child_path), is_dir, collapsed) {
                    break 'levels;
                }
                if is_dir && !collapsed {
                    next.push(child_path);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(listing.render())
}

#[derive(Default)]
struct Listing {
    used: usize,
    entries: Vec<(String, String)>,
    cut_at: Option<usize>,
    collapsed: usize,
    skipped: Vec<String>,
}

impl Listing {
    fn push(&mut self, level: usize, display: &str, is_dir: bool, collapsed: bool) -> bool {
        if self.cut_at.is_some() {
            return false;
        }
        let key = if is_dir { format!("{display}/") } else { display.to_owned() };
        let line = if collapsed { render("tool.ignored_entry", &[("path", &key)]) } else { key.clone() };
        let cost = line.chars().count() + 1;
        if self.used + cost > LS_BUDGET_CHARS {
            self.cut_at = Some(level);
            return false;
        }
        self.used += cost;
        if collapsed {
            self.collapsed += 1;
        }
        self.entries.push((key, line));
        true
    }

    fn render(mut self) -> String {
        self.entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut lines: Vec<String> = self.entries.into_iter().map(|(_, line)| line).collect();
        if lines.is_empty() && self.cut_at.is_none() && self.skipped.is_empty() {
            return text("tool.ls_empty").to_owned();
        }
        let limit = LS_BUDGET_CHARS.to_string();
        match self.cut_at {
            Some(level) if level > 1 => lines.push(render("tool.ls_limit", &[("limit", &limit), ("depth", &(level - 2).to_string())])),
            Some(_) => lines.push(render("tool.ls_limit_partial", &[("limit", &limit)])),
            None => {}
        }
        if self.collapsed > 0 {
            lines.push(text("tool.ls_ignored_note").to_owned());
        }
        lines.extend(skipped_lines(self.skipped));
        lines.join("\n")
    }
}

// ---------------------------------------------------------------- find

struct FindTool;

impl Tool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }
    fn dangerous(&self, _: &Value) -> bool {
        false
    }
    fn parallel_safe(&self, _: &Value) -> bool {
        true
    }
    fn summary(&self, args: &Value) -> String {
        args.get("query").and_then(Value::as_str).unwrap_or("").to_owned()
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            let query = required_string(&args, "query", 1024, false)?;
            let path = optional_string(&args, "path", ".", MAX_PATH_CHARS)?;
            let matcher = Glob::new(&query).map_err(|e| format!("Invalid glob pattern: {e}"))?.compile_matcher();
            let workspace = workspace_root(&ctx, &args)?;
            let (workspace, root) = blocking(move || -> Result<_, String> {
                let workspace = canonical_workspace(&workspace)?;
                let root = resolve_existing(&workspace, &path)?;
                Ok((workspace, root))
            })
            .await??;
            allow_outside_read(&ctx, "find", &args, &root).await?;
            blocking(move || run_find(&workspace, &root, &matcher).map(ToolResult::ok)).await?
        })
    }
}

/// Everything a thread of a parallel walk found, handed over when the thread ends.
struct Flush<'a, T> {
    local: Vec<T>,
    shared: &'a Mutex<Vec<T>>,
}

impl<T> Drop for Flush<'_, T> {
    fn drop(&mut self) {
        if !self.local.is_empty() {
            self.shared.lock().append(&mut self.local);
        }
    }
}

fn run_find(workspace: &Path, root: &Path, matcher: &GlobMatcher) -> Result<String, String> {
    let scanned = AtomicUsize::new(0);
    let scan_cut = AtomicBool::new(false);
    let found: Mutex<Vec<(PathBuf, bool)>> = Mutex::new(Vec::new());
    // `find` hides nothing: a name lookup is often after exactly the ignored file.
    WalkBuilder::new(root).standard_filters(false).follow_links(false).threads(search_threads()).build_parallel().run(|| {
        let mut flush = Flush { local: Vec::new(), shared: &found };
        let (scanned, scan_cut) = (&scanned, &scan_cut);
        Box::new(move |entry| {
            let Ok(entry) = entry else { return WalkState::Continue };
            if entry.depth() == 0 {
                return WalkState::Continue;
            }
            if scanned.fetch_add(1, Ordering::Relaxed) >= FIND_SCAN_LIMIT {
                scan_cut.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
            if matcher.is_match(relative) || matcher.is_match(Path::new(entry.file_name())) {
                flush.local.push((entry.path().to_owned(), is_dir));
            }
            // Version-control data can be found by name but is not walked.
            if is_dir && entry.file_name().to_str().is_some_and(is_vcs) {
                return WalkState::Skip;
            }
            WalkState::Continue
        })
    });
    // Only for ranking, and only the matches, so the rules cost nothing for the entries that did not match.
    let mut rules = Rules::for_root(root);
    let mut visible = Vec::new();
    let mut ignored = Vec::new();
    for (path, is_dir) in found.into_inner() {
        let mut shown = display_path(workspace, &path);
        if is_dir {
            shown.push('/');
        }
        if rules.ignores(root, &path, is_dir) { ignored.push(shown) } else { visible.push(shown) }
    }
    visible.sort_unstable();
    ignored.sort_unstable();
    let total = visible.len() + ignored.len();
    let ignored_total = ignored.len();
    let mut lines: Vec<String> = visible.into_iter().take(FIND_LIMIT).collect();
    let room = FIND_LIMIT - lines.len();
    lines.extend(ignored.into_iter().take(room).map(|p| render("tool.ignored_entry", &[("path", &p)])));
    let shown = lines.len();
    if lines.is_empty() {
        lines.push(text("tool.find_no_match").to_owned());
    }
    if total > shown {
        lines.push(render("tool.find_limit", &[("shown", &shown.to_string()), ("total", &total.to_string())]));
    }
    if ignored_total > 0 {
        lines.push(render("tool.find_ignored_note", &[("count", &ignored_total.to_string())]));
    }
    if scan_cut.load(Ordering::Relaxed) {
        lines.push(render("tool.find_scan_limit", &[("limit", &FIND_SCAN_LIMIT.to_string())]));
    }
    Ok(lines.join("\n"))
}

// ---------------------------------------------------------------- grep

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum GrepMode {
    #[default]
    FilesWithMatches,
    Content,
    Count,
}

struct GrepQuery {
    mode: GrepMode,
    before: usize,
    after: usize,
    glob: Option<GlobMatcher>,
    offset: usize,
    limit: usize,
}

fn optional_count(args: &Value, key: &str) -> Result<Option<usize>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(v) => v.as_u64().map(|n| Some(n.min(usize::MAX as u64) as usize)).ok_or_else(|| format!("Parameter {key} must be a non-negative integer")),
    }
}

impl GrepQuery {
    fn parse(args: &Value) -> Result<Self, String> {
        let mode = match args.get("output_mode") {
            None | Some(Value::Null) => GrepMode::default(),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "" | "files_with_matches" => GrepMode::FilesWithMatches,
                "content" => GrepMode::Content,
                "count" => GrepMode::Count,
                _ => return Err("Parameter output_mode must be files_with_matches, content or count".to_owned()),
            },
            Some(_) => return Err("Parameter output_mode must be a string".to_owned()),
        };
        let context = optional_count(args, "context")?;
        let before = optional_count(args, "before")?.or(context).unwrap_or(0).min(GREP_MAX_CONTEXT);
        let after = optional_count(args, "after")?.or(context).unwrap_or(0).min(GREP_MAX_CONTEXT);
        let glob_source = optional_string(args, "glob", "", 1024)?;
        let glob = if glob_source.is_empty() { None } else { Some(Glob::new(glob_source.trim()).map_err(|e| format!("Invalid glob pattern: {e}"))?.compile_matcher()) };
        let limit = match optional_u64(args, "limit").map_err(|_| "Parameter limit must be a positive integer".to_owned())? {
            None | Some(0) => GREP_DEFAULT_LIMIT,
            Some(n) => (n as usize).min(GREP_MAX_LIMIT),
        };
        let offset = optional_u64(args, "offset").map_err(|_| "Parameter offset must be a non-negative integer".to_owned())?.unwrap_or(0);
        if offset > GREP_MAX_OFFSET as u64 {
            return Err(format!("Parameter offset cannot exceed {GREP_MAX_OFFSET}"));
        }
        Ok(Self { mode, before, after, glob, offset: offset as usize, limit })
    }

    fn wanted(&self) -> usize {
        self.offset + self.limit + 1
    }

    fn allows(&self, relative: &str) -> bool {
        self.glob.as_ref().is_none_or(|g| g.is_match(relative) || g.is_match(relative.rsplit('/').next().unwrap_or(relative)))
    }

    fn finish(&self, mut lines: Vec<String>, found: usize, footer: Option<String>, skipped: Vec<String>) -> String {
        let more = found > self.offset + self.limit;
        if lines.is_empty() {
            lines.push(if self.offset > 0 && found > 0 {
                render("tool.grep_no_match_at_offset", &[("offset", &self.offset.to_string()), ("count", &found.to_string())])
            } else {
                text("tool.grep_no_match").to_owned()
            });
        } else {
            if more {
                let to = self.offset + self.limit;
                lines.push(render("tool.grep_limit", &[("from", &(self.offset + 1).to_string()), ("to", &to.to_string()), ("next", &to.to_string())]));
            }
            lines.extend(footer);
        }
        lines.extend(skipped_lines(skipped));
        lines.join("\n")
    }
}

/// One matching line, with the lines around it, already cut to [`GREP_LINE_CHARS`].
#[derive(Debug)]
struct ContentMatch {
    path: String,
    line: u64,
    text: String,
    before: Vec<String>,
    after: Vec<String>,
}

/// In ripgrep's spelling: a match is `path:line:text`, a context line `path-line-text`, and groups
/// that do not touch are told apart by `--`. A line that is one of the page's own matches prints as one, once.
fn content_lines(matches: &[ContentMatch], grouped: bool) -> Vec<String> {
    let on_page: HashSet<(&str, u64)> = matches.iter().map(|m| (m.path.as_str(), m.line)).collect();
    let mut lines = Vec::new();
    let mut last: Option<(&str, u64)> = None;
    for hit in matches {
        let first = hit.line - hit.before.len() as u64;
        let window = hit
            .before
            .iter()
            .enumerate()
            .map(|(i, t)| (first + i as u64, t))
            .chain(std::iter::once((hit.line, &hit.text)))
            .chain(hit.after.iter().enumerate().map(|(i, t)| (hit.line + 1 + i as u64, t)));
        for (number, line) in window {
            if last.is_some_and(|(path, done)| path == hit.path && number <= done) {
                continue;
            }
            let touching = last.is_some_and(|(path, done)| path == hit.path && number == done + 1);
            if grouped && last.is_some() && !touching {
                lines.push("--".to_owned());
            }
            let separator = if on_page.contains(&(hit.path.as_str(), number)) { ':' } else { '-' };
            lines.push(format!("{}{separator}{number}{separator}{line}", hit.path));
            last = Some((hit.path.as_str(), number));
        }
    }
    lines
}

/// The pattern twice: once as asked, line by line, and once over the whole buffer to jump between candidates.
struct LineMatcher {
    line: Regex,
    /// Multi-line, so a match in a line is a match here too and the lines between candidates cost nothing. `None` where that does not hold.
    whole: Option<Regex>,
}

impl LineMatcher {
    fn new(pattern: &str, case_sensitive: bool) -> Result<Self, String> {
        let line = RegexBuilder::new(pattern).case_insensitive(!case_sensitive).build().map_err(|e| format!("Invalid regular expression: {e}"))?;
        // `\A` and `\z` mean each line's ends to the model and the buffer's to this regex, and `(?-m)` undoes the mode.
        let whole = if ["\\A", "\\z", "(?-", "(?i-", "(?s-", "(?x-", "(?U-"].iter().any(|s| pattern.contains(s)) {
            None
        } else {
            RegexBuilder::new(pattern).case_insensitive(!case_sensitive).multi_line(true).build().ok()
        };
        Ok(Self { line, whole })
    }

    /// Calls `visit` with each matching line's number and span, until it answers `false`.
    fn for_each(&self, text: &str, mut visit: impl FnMut(u64, Range<usize>) -> bool) {
        let bytes = text.as_bytes();
        let len = bytes.len();
        // A `\r` makes a line end differently for the two regexes, so those files go line by line.
        let jump = self.whole.as_ref().filter(|_| memchr(b'\r', bytes).is_none());
        let mut pos = 0;
        let mut number = 1u64;
        while pos < len {
            if let Some(whole) = jump {
                let Some(found) = whole.find_at(text, pos) else { break };
                let start = memrchr(b'\n', &bytes[pos..found.start()]).map_or(pos, |i| pos + i + 1);
                if start >= len {
                    break;
                }
                number += memchr_iter(b'\n', &bytes[pos..start]).count() as u64;
                pos = start;
            }
            let end = memchr(b'\n', &bytes[pos..]).map_or(len, |i| pos + i);
            let shown_end = if end > pos && end < len && bytes[end - 1] == b'\r' { end - 1 } else { end };
            if self.line.is_match(&text[pos..shown_end]) && !visit(number, pos..shown_end) {
                return;
            }
            pos = end + 1;
            number += 1;
        }
    }
}

enum FileResult {
    /// A file with a match, and when it last changed.
    Hit(u64),
    Count(u64),
    Content(Vec<ContentMatch>),
    Nothing,
}

enum Buffer {
    Owned(Vec<u8>),
    Mapped(memmap2::Mmap),
}

impl Buffer {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Owned(v) => v,
            Self::Mapped(m) => m,
        }
    }
}

fn load(path: &Path) -> io::Result<Option<(Buffer, fs::Metadata)>> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Ok(None);
    }
    let mut file = File::open(path)?;
    let buffer = if meta.len() >= MMAP_AT {
        // SAFETY: the mapping is read-only and lives inside this call's search. A file truncated by
        // someone else underneath it is the one risk, and ripgrep runs on the same bargain.
        Buffer::Mapped(unsafe { memmap2::Mmap::map(&file)? })
    } else {
        let mut v = Vec::with_capacity(meta.len() as usize);
        file.read_to_end(&mut v)?;
        Buffer::Owned(v)
    };
    Ok(Some((buffer, meta)))
}

fn context_lines_before(text: &str, start: usize, count: usize) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut lines = Vec::with_capacity(count);
    let mut end = start;
    for _ in 0..count {
        if end == 0 {
            break;
        }
        let line_end = end - 1;
        let line_start = memrchr(b'\n', &bytes[..line_end]).map_or(0, |i| i + 1);
        lines.push(truncate_chars(text[line_start..line_end].trim_end_matches('\r'), GREP_LINE_CHARS));
        end = line_start;
    }
    lines.reverse();
    lines
}

fn context_lines_after(text: &str, end: usize, count: usize) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut lines = Vec::with_capacity(count);
    let mut pos = memchr(b'\n', &bytes[end..]).map_or(bytes.len(), |i| end + i + 1);
    for _ in 0..count {
        if pos >= bytes.len() {
            break;
        }
        let line_end = memchr(b'\n', &bytes[pos..]).map_or(bytes.len(), |i| pos + i);
        lines.push(truncate_chars(text[pos..line_end].trim_end_matches('\r'), GREP_LINE_CHARS));
        pos = line_end + 1;
    }
    lines
}

/// One file's answer. A link and a binary file (a NUL in its first 8 KiB) are passed over.
fn search_file(path: &Path, shown: &str, matcher: &LineMatcher, query: &GrepQuery) -> io::Result<FileResult> {
    let Some((buffer, meta)) = load(path)? else { return Ok(FileResult::Nothing) };
    let bytes = buffer.bytes();
    if memchr(0, &bytes[..bytes.len().min(SNIFF_BYTES)]).is_some() {
        return Ok(FileResult::Nothing);
    }
    let decoded = String::from_utf8_lossy(bytes);
    let content: &str = &decoded;
    Ok(match query.mode {
        GrepMode::FilesWithMatches => {
            let mut found = false;
            matcher.for_each(content, |_, _| {
                found = true;
                false
            });
            if found { FileResult::Hit(modified_ms(&meta)) } else { FileResult::Nothing }
        }
        GrepMode::Count => {
            let mut count = 0;
            matcher.for_each(content, |_, _| {
                count += 1;
                true
            });
            if count > 0 { FileResult::Count(count) } else { FileResult::Nothing }
        }
        GrepMode::Content => {
            let mut matches = Vec::new();
            let wanted = query.wanted();
            matcher.for_each(content, |line, span| {
                matches.push(ContentMatch {
                    path: shown.to_owned(),
                    line,
                    before: context_lines_before(content, span.start, query.before),
                    after: context_lines_after(content, span.end, query.after),
                    text: truncate_chars(&content[span], GREP_LINE_CHARS),
                });
                matches.len() < wanted
            });
            if matches.is_empty() { FileResult::Nothing } else { FileResult::Content(matches) }
        }
    })
}

/// The files `grep` looks at below a directory: what Git would show, or with no Git, everything outside the dependency directories.
fn grep_files(root: &Path, query: &GrepQuery) -> Vec<PathBuf> {
    let rules = Rules::for_root(root);
    let in_git = rules.git_applies();
    let names_apply = matches!(rules, Rules::DependencyNames);
    let mut walk = WalkBuilder::new(root);
    walk.standard_filters(false)
        .git_ignore(in_git)
        .git_exclude(in_git)
        .git_global(in_git)
        .parents(in_git)
        .require_git(false)
        .follow_links(false)
        .threads(search_threads());
    walk.filter_entry(move |e| {
        let is_dir = e.file_type().is_some_and(|t| t.is_dir());
        !is_dir || e.depth() == 0 || e.file_name().to_str().is_none_or(|n| !is_vcs(n) && !(names_apply && DEPENDENCY_DIRECTORIES.contains(&n)))
    });
    let found: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    walk.build_parallel().run(|| {
        let mut flush = Flush { local: Vec::new(), shared: &found };
        Box::new(move |entry| {
            let Ok(entry) = entry else { return WalkState::Continue };
            if entry.file_type().is_some_and(|t| t.is_file()) {
                let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
                if query.allows(&relative.to_string_lossy()) {
                    flush.local.push(entry.into_path());
                }
            }
            WalkState::Continue
        })
    });
    let mut files = found.into_inner();
    // The walk's order is the threads'; the page must not be.
    files.sort_unstable();
    files
}

/// A file as shown to the model, and what searching it came to.
type FileOutcome = (String, io::Result<FileResult>);

#[derive(Default)]
struct Settled {
    slots: HashMap<usize, FileOutcome>,
    /// Every file before this one is done.
    frontier: usize,
    /// The matches those files hold.
    total: usize,
}

/// Searches `files` in order on several threads. A worker claims the next file, so everything before
/// the first unfinished one is settled, and once that settled prefix holds the page nothing later can change it.
fn search_all(files: &[PathBuf], workspace: &Path, matcher: &LineMatcher, query: &GrepQuery, cancel: &Cancel) -> Vec<FileOutcome> {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let wanted = query.wanted();
    let settled: Mutex<Settled> = Mutex::new(Settled::default());
    let work = || {
        loop {
            if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                return;
            }
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(path) = files.get(index) else { return };
            let shown = display_path(workspace, path);
            let result = search_file(path, &shown, matcher, query);
            let mut done = settled.lock();
            done.slots.insert(index, (shown, result));
            if query.mode == GrepMode::Content {
                while let Some((_, result)) = done.slots.get(&done.frontier) {
                    let found = match result {
                        Ok(FileResult::Content(found)) => found.len(),
                        _ => 0,
                    };
                    done.total += found;
                    done.frontier += 1;
                }
                if done.total >= wanted {
                    stop.store(true, Ordering::Relaxed);
                }
            }
        }
    };
    let threads = search_threads().min(files.len().max(1));
    if threads <= 1 {
        work();
    } else {
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(work);
            }
        });
    }
    let done = settled.into_inner();
    let mut out: Vec<(usize, FileOutcome)> = done.slots.into_iter().collect();
    out.sort_unstable_by_key(|(i, _)| *i);
    if query.mode == GrepMode::Content && stop.load(Ordering::Relaxed) {
        // Whatever finished past the settled prefix is no part of a page that stops inside it.
        out.retain(|(i, _)| *i < done.frontier);
    }
    out.into_iter().map(|(_, outcome)| outcome).collect()
}

fn run_grep(workspace: &Path, root: &Path, pattern: &str, case_sensitive: bool, query: &GrepQuery, cancel: &Cancel) -> Result<String, String> {
    let matcher = LineMatcher::new(pattern, case_sensitive)?;
    // A file named outright is searched whatever the ignore rules or the glob say.
    let files = if root.is_dir() { grep_files(root, query) } else { vec![root.to_owned()] };
    let mut skipped = Vec::new();
    let mut hits: Vec<(String, u64)> = Vec::new();
    let mut counts: Vec<(String, u64)> = Vec::new();
    let mut matches: Vec<ContentMatch> = Vec::new();
    for (shown, result) in search_all(&files, workspace, &matcher, query, cancel) {
        match result {
            Err(e) => skipped.push(format!("{shown}: {e}")),
            Ok(FileResult::Nothing) => {}
            Ok(FileResult::Hit(modified)) => hits.push((shown, modified)),
            Ok(FileResult::Count(n)) => counts.push((shown, n)),
            Ok(FileResult::Content(mut found)) => matches.append(&mut found),
        }
    }
    Ok(match query.mode {
        GrepMode::FilesWithMatches => {
            // Newest first, the same second in path order, so a page is the same page each time.
            hits.sort_by(|a, b| (b.1 / 1000).cmp(&(a.1 / 1000)).then_with(|| a.0.cmp(&b.0)));
            let found = hits.len();
            let lines: Vec<String> = hits.into_iter().skip(query.offset).take(query.limit).map(|(p, _)| p).collect();
            query.finish(lines, found, None, skipped)
        }
        GrepMode::Count => {
            counts.sort_by(|a, b| a.0.cmp(&b.0));
            let found = counts.len();
            let total: u64 = counts.iter().map(|(_, n)| n).sum();
            let lines: Vec<String> = counts.into_iter().skip(query.offset).take(query.limit).map(|(p, n)| format!("{p}:{n}")).collect();
            let footer = render("tool.grep_count_total", &[("matches", &total.to_string()), ("files", &found.to_string())]);
            query.finish(lines, found, Some(footer), skipped)
        }
        GrepMode::Content => {
            let found = matches.len();
            let page: Vec<ContentMatch> = matches.into_iter().skip(query.offset).take(query.limit).collect();
            let lines = content_lines(&page, query.before + query.after > 0);
            query.finish(lines, found, None, skipped)
        }
    })
}

struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn dangerous(&self, _: &Value) -> bool {
        false
    }
    fn parallel_safe(&self, _: &Value) -> bool {
        true
    }
    fn summary(&self, args: &Value) -> String {
        let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
        match args.get("path").and_then(Value::as_str) {
            Some(path) => format!("{pattern} · {path}"),
            None => pattern.to_owned(),
        }
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            let pattern = required_string(&args, "pattern", 4096, false)?;
            let path = optional_string(&args, "path", ".", MAX_PATH_CHARS)?;
            let query = GrepQuery::parse(&args)?;
            let case_sensitive = optional_bool(&args, "case_sensitive", true)?;
            // Before any walk: a bad pattern should cost nothing.
            RegexBuilder::new(&pattern).case_insensitive(!case_sensitive).build().map_err(|e| format!("Invalid regular expression: {e}"))?;
            let workspace = workspace_root(&ctx, &args)?;
            let (workspace, root) = blocking(move || -> Result<_, String> {
                let workspace = canonical_workspace(&workspace)?;
                let root = resolve_existing(&workspace, &path)?;
                Ok((workspace, root))
            })
            .await??;
            allow_outside_read(&ctx, "grep", &args, &root).await?;
            let cancel = ctx.run.cancel.clone();
            let spill = super::output_dir(&ctx);
            blocking(move || {
                let output = run_grep(&workspace, &root, &pattern, case_sensitive, &query, &cancel)?;
                Ok(ToolResult::ok(fit(output, GREP_INLINE_CHARS, &spill, "grep")))
            })
            .await?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{call, fixture};
    use super::*;
    use crate::model::PermissionMode;
    use serde_json::json;

    fn run_git(dir: &Path, args: &[&str]) {
        let status =
            std::process::Command::new("git").args(args).current_dir(dir).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn edit_requires_exactly_one_match() {
        let err = apply_edit("a\nb\na\n", "a", "x", false).unwrap_err();
        assert!(err.contains("occurs 2 times"), "{err}");
        assert!(err.contains("line 1, col 1: a"), "{err}");
        assert!(err.contains("line 3, col 1: a"), "{err}");
        let done = apply_edit("a\nb\na\n", "a", "x", true).unwrap();
        assert_eq!(done, EditApplied { text: "x\nb\nx\n".into(), replacements: 2 });
        assert_eq!(apply_edit("a\nb\n", "b", "c", false).unwrap().text, "a\nc\n");
        assert_eq!(apply_edit("a", "zzz", "c", false).unwrap_err(), EDIT_NOT_FOUND);
        assert_eq!(apply_edit("a", "a", "a", false).unwrap_err(), EDIT_FIND_EQUALS_REPLACE);
        assert_eq!(apply_edit("a", "", "b", false).unwrap_err(), "Parameter find cannot be empty");
    }

    #[test]
    fn edit_keeps_line_endings_and_folds_quotes() {
        assert_eq!(apply_edit("one\r\ntwo\r\n", "one\ntwo", "1\n2", false).unwrap().text, "1\r\n2\r\n");
        assert_eq!(apply_edit("\u{feff}a\nb\n", "a", "c", false).unwrap().text, "\u{feff}c\nb\n");
        // A deletion of a whole line takes its break, one inside a line does not.
        assert_eq!(apply_edit("keep\ndrop\nkeep2\n", "drop", "", false).unwrap().text, "keep\nkeep2\n");
        assert_eq!(apply_edit("a drop b\n", " drop", "", false).unwrap().text, "a b\n");
        let curly = apply_edit("say \u{201c}hi\u{201d} now\n", "\"hi\"", "\"bye\"", false).unwrap();
        assert_eq!(curly.text, "say \u{201c}bye\u{201d} now\n");
        let apostrophe = apply_edit("it\u{2019}s fine\n", "it's", "it's ok", false).unwrap();
        assert_eq!(apostrophe.text, "it\u{2019}s ok fine\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_read_gate_holds() {
        let f = fixture(PermissionMode::FullAccess);
        let path = f.dir.join("work/a.txt");
        fs::write(&path, "one\ntwo\n").unwrap();
        // Never read: both refuse, and the file is untouched.
        let r = call(&f.ctx, "write", json!({"path": "a.txt", "content": "x"})).await;
        assert!(r.is_error);
        assert_eq!(r.output, FILE_NOT_READ);
        let r = call(&f.ctx, "edit", json!({"path": "a.txt", "find": "one", "replace": "1"})).await;
        assert_eq!(r.output, FILE_NOT_READ);
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");

        // Read: the edit goes through, says so, and describes itself.
        let r = call(&f.ctx, "read", json!({"path": "a.txt"})).await;
        assert_eq!(r.output, "one\ntwo");
        let r = call(&f.ctx, "edit", json!({"path": "a.txt", "find": "one", "replace": "1"})).await;
        assert!(!r.is_error, "{}", r.output);
        assert_eq!(r.output, format!("ok{}", text("tool.file_state_current")));
        assert_eq!(r.meta.file.as_deref(), Some("a.txt"));
        assert_eq!((r.meta.added, r.meta.removed), (1, 1));
        let diff = r.meta.diff.unwrap();
        assert!(diff.contains("-one") && diff.contains("+1") && diff.contains("--- a.txt"), "{diff}");

        // Its own write does not make the file stale.
        let r = call(&f.ctx, "edit", json!({"path": "a.txt", "find": "two", "replace": "2"})).await;
        assert!(!r.is_error, "{}", r.output);

        // Someone else's does, even when the size stays.
        fs::write(&path, "XXXXX\n").unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let mtime = fs::metadata(&path).unwrap().modified().unwrap() + Duration::from_secs(5);
        File::options().write(true).open(&path).unwrap().set_modified(mtime).unwrap();
        let r = call(&f.ctx, "write", json!({"path": "a.txt", "content": "y"})).await;
        assert_eq!(r.output, FILE_MODIFIED_SINCE_READ);
        let r = call(&f.ctx, "edit", json!({"path": "a.txt", "find": "XXXXX", "replace": "y"})).await;
        assert_eq!(r.output, FILE_MODIFIED_SINCE_READ);

        // A partial read counts, since the model has seen the file.
        call(&f.ctx, "read", json!({"path": "a.txt", "start_line": 1, "end_line": 1})).await;
        let r = call(&f.ctx, "write", json!({"path": "a.txt", "content": "y"})).await;
        assert!(!r.is_error, "{}", r.output);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_file_needs_no_read_and_creates_its_parents() {
        let f = fixture(PermissionMode::FullAccess);
        let r = call(&f.ctx, "write", json!({"path": "deep/er/new.txt", "content": "hi\n"})).await;
        assert!(!r.is_error, "{}", r.output);
        assert_eq!(fs::read_to_string(f.dir.join("work/deep/er/new.txt")).unwrap(), "hi\n");
        assert_eq!(r.meta.added, 1);
        assert!(r.meta.diff.unwrap().contains("--- /dev/null"));
        // And what it wrote it has seen.
        let r = call(&f.ctx, "write", json!({"path": "deep/er/new.txt", "content": "bye\n"})).await;
        assert!(!r.is_error, "{}", r.output);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_keeps_the_mode_and_goes_through_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture(PermissionMode::FullAccess);
        let real = f.dir.join("work/run.sh");
        fs::write(&real, "old\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&real, f.dir.join("work/link.sh")).unwrap();
        call(&f.ctx, "read", json!({"path": "link.sh"})).await;
        let r = call(&f.ctx, "write", json!({"path": "link.sh", "content": "new\n"})).await;
        assert!(!r.is_error, "{}", r.output);
        assert_eq!(fs::read_to_string(&real).unwrap(), "new\n");
        assert_eq!(fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(fs::symlink_metadata(f.dir.join("work/link.sh")).unwrap().file_type().is_symlink());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_windows_and_refusals() {
        let f = fixture(PermissionMode::FullAccess);
        let body: String = (1..=3000).map(|i| format!("line {i}\n")).collect();
        fs::write(f.dir.join("work/long.txt"), &body).unwrap();
        let r = call(&f.ctx, "read", json!({"path": "long.txt"})).await;
        assert!(r.output.ends_with("… showing lines 1–2000 of 3000. Continue with start_line=2001."), "{}", &r.output[r.output.len() - 100..]);
        assert_eq!(call(&f.ctx, "read", json!({"path": "long.txt", "start_line": 2, "end_line": 3})).await.output, "line 2\nline 3");
        let r = call(&f.ctx, "read", json!({"path": "long.txt", "start_line": 5000})).await;
        assert_eq!(r.output, "(The file has only 3000 lines, so the requested start_line is out of range)");
        assert!(call(&f.ctx, "read", json!({"path": "long.txt", "start_line": 5, "end_line": 2})).await.is_error);

        fs::write(f.dir.join("work/empty.txt"), "").unwrap();
        assert_eq!(call(&f.ctx, "read", json!({"path": "empty.txt"})).await.output, "(The file exists but is empty)");

        fs::write(f.dir.join("work/bin"), b"abc\0def").unwrap();
        let r = call(&f.ctx, "read", json!({"path": "bin"})).await;
        assert!(r.is_error && r.output.contains("cannot read binary files: bin"), "{}", r.output);

        fs::write(f.dir.join("work/lossy.txt"), b"ok \xff here\n").unwrap();
        let r = call(&f.ctx, "read", json!({"path": "lossy.txt"})).await;
        assert!(r.output.contains('\u{fffd}') && r.output.contains("`edit` cannot modify it"), "{}", r.output);

        let utf16: Vec<u8> = [0xff, 0xfe].into_iter().chain("hi\n世\n".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        fs::write(f.dir.join("work/u16.txt"), utf16).unwrap();
        let r = call(&f.ctx, "read", json!({"path": "u16.txt"})).await;
        assert!(r.output.starts_with("hi\n世") && r.output.contains("UTF-16LE"), "{}", r.output);

        fs::write(f.dir.join("work/pic.png"), b"\x89PNG\r\n\x1a\nrest").unwrap();
        let r = call(&f.ctx, "read", json!({"path": "pic.png"})).await;
        assert!(r.is_error && r.output.contains("test-model does not support image input"), "{}", r.output);

        let wide = "世".repeat(40_000);
        fs::write(f.dir.join("work/wide.txt"), &wide).unwrap();
        let r = call(&f.ctx, "read", json!({"path": "wide.txt"})).await;
        assert!(r.output.contains("[line cut:") && !r.output.contains('\u{fffd}'), "{}", &r.output[r.output.len() - 80..]);

        assert!(call(&f.ctx, "read", json!({"path": "missing"})).await.output.starts_with("Could not access path missing"));
    }

    fn tree() -> super::super::testing::Fixture {
        let f = fixture(PermissionMode::FullAccess);
        let work = f.dir.join("work");
        fs::create_dir_all(work.join("src/nested")).unwrap();
        fs::create_dir_all(work.join("target/debug")).unwrap();
        fs::write(work.join("src/a.rs"), "fn alpha() {}\n// TODO one\nlet x = 1;\nlet y = 2;\n// TODO two\n").unwrap();
        fs::write(work.join("src/nested/b.rs"), "// TODO three\n").unwrap();
        fs::write(work.join("src/notes.md"), "TODO in markdown\n").unwrap();
        fs::write(work.join("target/debug/out.rs"), "TODO hidden\n").unwrap();
        fs::write(work.join("blob.bin"), b"TODO\0binary").unwrap();
        f
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_modes() {
        let f = tree();
        // No Git here, so the dependency names stand in: `target` is skipped, and so is the binary.
        let r = call(&f.ctx, "grep", json!({"pattern": "TODO"})).await;
        let mut lines: Vec<&str> = r.output.lines().collect();
        lines.sort();
        assert_eq!(lines, ["src/a.rs", "src/nested/b.rs", "src/notes.md"]);

        let r = call(&f.ctx, "grep", json!({"pattern": "TODO", "output_mode": "count"})).await;
        assert_eq!(r.output, "src/a.rs:2\nsrc/nested/b.rs:1\nsrc/notes.md:1\nTotal: 4 matching line(s) in 3 file(s).");

        let r = call(&f.ctx, "grep", json!({"pattern": "TODO", "output_mode": "content", "glob": "*.rs"})).await;
        assert_eq!(r.output, "src/a.rs:2:// TODO one\nsrc/a.rs:5:// TODO two\nsrc/nested/b.rs:1:// TODO three");

        let r = call(&f.ctx, "grep", json!({"pattern": "todo", "output_mode": "content", "case_sensitive": false, "glob": "src/nested/**"})).await;
        assert_eq!(r.output, "src/nested/b.rs:1:// TODO three");

        let r = call(&f.ctx, "grep", json!({"pattern": "absent"})).await;
        assert_eq!(r.output, "No matches found");

        // Naming a directory or file inside one hides nothing.
        let r = call(&f.ctx, "grep", json!({"pattern": "TODO", "path": "target"})).await;
        assert_eq!(r.output, "target/debug/out.rs");
        let r = call(&f.ctx, "grep", json!({"pattern": "hidden", "path": "target/debug/out.rs", "output_mode": "content"})).await;
        assert_eq!(r.output, "target/debug/out.rs:1:TODO hidden");

        assert!(call(&f.ctx, "grep", json!({"pattern": "("})).await.output.starts_with("Invalid regular expression"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_context_and_paging() {
        let f = tree();
        let r = call(&f.ctx, "grep", json!({"pattern": "TODO", "output_mode": "content", "context": 1, "path": "src/a.rs"})).await;
        assert_eq!(
            r.output, "src/a.rs-1-fn alpha() {}\nsrc/a.rs:2:// TODO one\nsrc/a.rs-3-let x = 1;\nsrc/a.rs-4-let y = 2;\nsrc/a.rs:5:// TODO two",
            "windows that touch print as one"
        );
        let r = call(&f.ctx, "grep", json!({"pattern": "TODO", "output_mode": "content", "after": 1, "path": "src/a.rs"})).await;
        assert_eq!(r.output, "src/a.rs:2:// TODO one\nsrc/a.rs-3-let x = 1;\n--\nsrc/a.rs:5:// TODO two");
        let r = call(&f.ctx, "grep", json!({"pattern": "TODO", "output_mode": "content", "before": 0, "after": 2, "path": "src/a.rs", "limit": 1})).await;
        assert!(
            r.output.starts_with("src/a.rs:2:// TODO one\nsrc/a.rs-3-let x = 1;\nsrc/a.rs-4-let y = 2;\n… more matches follow. Showing 1–1; pass offset=1"),
            "{}",
            r.output
        );
        let r = call(&f.ctx, "grep", json!({"pattern": "TODO", "output_mode": "content", "glob": "*.rs", "offset": 1, "limit": 1})).await;
        assert!(r.output.starts_with("src/a.rs:5:// TODO two\n… more matches follow. Showing 2–2; pass offset=2"), "{}", r.output);
        let r = call(&f.ctx, "grep", json!({"pattern": "TODO", "output_mode": "content", "offset": 50, "path": "src/nested"})).await;
        assert_eq!(r.output, "No matches at offset 50; there are 1.");
    }

    #[test]
    fn line_matching_agrees_with_and_without_the_jump() {
        let cases = [
            ("^b", "a\nb\nab\nb"),
            ("a$", "a\nba\nb"),
            ("x\\s+y", "x\n y\nx y\n"),
            ("^$", "\n\na\n\n"),
            ("", "a\nb\n"),
            ("(?i)TODO", "todo\nno\nToDo"),
            ("\\Afoo", "foo\nbar\nfoo\n"),
            ("(?-m)^foo", "foo\nbar\nfoo\n"),
        ];
        for (pattern, content) in cases {
            let with = LineMatcher::new(pattern, true).unwrap();
            let without = LineMatcher { line: with.line.clone(), whole: None };
            let collect = |m: &LineMatcher| {
                let mut out = Vec::new();
                m.for_each(content, |n, span| {
                    out.push((n, content[span].to_owned()));
                    true
                });
                out
            };
            assert_eq!(collect(&with), collect(&without), "{pattern:?} on {content:?}");
        }
        let crlf = LineMatcher::new("b$", true).unwrap();
        let mut seen = Vec::new();
        crlf.for_each("a\r\nb\r\n", |n, span| {
            seen.push((n, "a\r\nb\r\n"[span].to_owned()));
            true
        });
        assert_eq!(seen, [(2, "b".to_owned())], "a line ends before its \\r");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_page_is_the_same_page_every_time() {
        let f = fixture(PermissionMode::FullAccess);
        let work = f.dir.join("work");
        for i in 0..200 {
            fs::write(work.join(format!("f{i:03}.txt")), "needle\nneedle\n").unwrap();
        }
        let args = json!({"pattern": "needle", "output_mode": "content", "limit": 7, "offset": 3});
        let first = call(&f.ctx, "grep", args.clone()).await.output;
        assert!(first.starts_with("f001.txt:2:needle\nf002.txt:1:needle"), "{first}");
        for _ in 0..5 {
            assert_eq!(call(&f.ctx, "grep", args.clone()).await.output, first);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn git_decides_inside_a_work_tree() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let f = fixture(PermissionMode::FullAccess);
        let work = f.dir.join("work");
        run_git(&work, &["init", "-q"]);
        fs::write(work.join(".gitignore"), "out/\n*.log\n!keep.log\n").unwrap();
        fs::create_dir_all(work.join("out/deep")).unwrap();
        fs::create_dir_all(work.join("build")).unwrap();
        fs::write(work.join("out/deep/gen.txt"), "needle\n").unwrap();
        fs::write(work.join("noise.log"), "needle\n").unwrap();
        fs::write(work.join("keep.log"), "needle\n").unwrap();
        // Only a name on the fallback list, and Git does not ignore it, so it is searched here.
        fs::write(work.join("build/real.txt"), "needle\n").unwrap();

        let r = call(&f.ctx, "grep", json!({"pattern": "needle"})).await;
        let mut lines: Vec<&str> = r.output.lines().collect();
        lines.sort();
        assert_eq!(lines, ["build/real.txt", "keep.log"]);

        let r = call(&f.ctx, "grep", json!({"pattern": "needle", "path": "out"})).await;
        assert_eq!(r.output, "out/deep/gen.txt", "a named ignored root hides nothing");

        let r = call(&f.ctx, "ls", json!({"path": "."})).await;
        assert!(r.output.contains("out/ (ignored)"), "{}", r.output);
        assert!(r.output.contains(".git/ (ignored)"), "{}", r.output);
        assert!(!r.output.contains("out/deep"), "{}", r.output);
        assert!(r.output.contains("build/real.txt"), "{}", r.output);
        assert!(r.output.contains("(ignored) marks directories"), "{}", r.output);
        let r = call(&f.ctx, "ls", json!({"path": "out"})).await;
        assert!(r.output.contains("out/deep/gen.txt"), "{}", r.output);

        let r = call(&f.ctx, "find", json!({"query": "*.log"})).await;
        assert_eq!(
            r.output,
            "keep.log\nnoise.log (ignored)\n(1 of the matches are in paths Git ignores — or, outside a Git repository, in dependency or build directories. They are listed after the others, marked (ignored).)"
        );
        let r = call(&f.ctx, "find", json!({"query": "gen.txt"})).await;
        assert!(r.output.starts_with("out/deep/gen.txt (ignored)"), "{}", r.output);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ls_depth() {
        let f = tree();
        let r = call(&f.ctx, "ls", json!({"path": ".", "depth": 0})).await;
        assert_eq!(
            r.output,
            "blob.bin\nsrc/\ntarget/ (ignored)\n(ignored) marks directories ignored by Git, version-control data, or, outside a Git repository, dependency/build output: listed but not expanded; pass one as path to list it."
        );
        let r = call(&f.ctx, "ls", json!({"path": "src"})).await;
        assert_eq!(r.output, "src/a.rs\nsrc/nested/\nsrc/nested/b.rs\nsrc/notes.md");
        let r = call(&f.ctx, "ls", json!({"path": "src", "depth": 0})).await;
        assert_eq!(r.output, "src/a.rs\nsrc/nested/\nsrc/notes.md");
        fs::create_dir_all(f.dir.join("work/void")).unwrap();
        assert_eq!(call(&f.ctx, "ls", json!({"path": "void"})).await.output, "(empty directory)");
        assert!(call(&f.ctx, "ls", json!({"path": "src/a.rs"})).await.output.starts_with("ls target is not a directory"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn find_matches_paths_and_names_and_caps_the_page() {
        let f = tree();
        let r = call(&f.ctx, "find", json!({"query": "*.rs", "path": "src"})).await;
        assert_eq!(r.output, "src/a.rs\nsrc/nested/b.rs");
        let r = call(&f.ctx, "find", json!({"query": "nested"})).await;
        assert_eq!(r.output, "src/nested/");
        assert_eq!(call(&f.ctx, "find", json!({"query": "zzz"})).await.output, "No matching files");
        let r = call(&f.ctx, "find", json!({"query": "out.rs"})).await;
        assert!(r.output.starts_with("target/debug/out.rs (ignored)"), "{}", r.output);
        for i in 0..150 {
            fs::write(f.dir.join(format!("work/m{i}.dat")), "").unwrap();
        }
        let r = call(&f.ctx, "find", json!({"query": "*.dat"})).await;
        assert!(r.output.ends_with("(Showing 100 of 150 matches. Narrow the pattern or path to see the rest.)"), "{}", r.output);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_grep_too_long_for_the_context_goes_to_a_file() {
        let f = fixture(PermissionMode::FullAccess);
        let body = "needle padding padding padding padding\n".repeat(2000);
        fs::write(f.dir.join("work/big.txt"), &body).unwrap();
        let r = call(&f.ctx, "grep", json!({"pattern": "needle", "output_mode": "content", "limit": 1000})).await;
        assert!(r.output.starts_with("<persisted-output>"), "{}", &r.output[..80]);
        // A file past the mapping threshold reads the same.
        let big = "x\n".repeat(200_000) + "needle at the end\n";
        fs::write(f.dir.join("work/mapped.txt"), &big).unwrap();
        let r = call(&f.ctx, "grep", json!({"pattern": "needle at", "output_mode": "content", "path": "mapped.txt"})).await;
        assert_eq!(r.output, "mapped.txt:200001:needle at the end");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_workspace_number_selects_the_root() {
        let f = fixture(PermissionMode::FullAccess);
        let other = f.dir.join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("x.txt"), "over here\n").unwrap();
        let ctx = Arc::new(ToolCtx {
            engine: f.ctx.engine.clone(),
            run: f.ctx.run.clone(),
            session: f.ctx.session.clone(),
            spec: f.ctx.spec.clone(),
            item: 1,
            cwd: f.ctx.cwd.clone(),
            workspaces: vec![f.ctx.cwd.clone(), other],
        });
        assert_eq!(call(&ctx, "read", json!({"path": "x.txt", "workspace": 2})).await.output, "over here");
        assert!(call(&ctx, "read", json!({"path": "x.txt"})).await.is_error);
        assert!(call(&ctx, "read", json!({"path": "x.txt", "workspace": 3})).await.output.contains("workspace numbers"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reading_outside_the_workspaces_asks_first() {
        let f = fixture(PermissionMode::Ask);
        let outside = f.dir.join("outside.txt");
        fs::write(&outside, "secret\n").unwrap();
        fs::write(f.dir.join("work/inside.txt"), "fine\n").unwrap();
        assert_eq!(call(&f.ctx, "read", json!({"path": "inside.txt"})).await.output, "fine");
        let (ctx, path) = (f.ctx.clone(), outside.to_string_lossy().into_owned());
        let pending = tokio::spawn(async move { call(&ctx, "read", json!({"path": path})).await });
        let request = loop {
            let mut events = Vec::new();
            f.ctx.run.inbox.drain(&mut events);
            let found = events.into_iter().find_map(|e| match e {
                crate::inbox::RunEvent::Approval(a) => Some(a.request),
                _ => None,
            });
            if let Some(r) = found {
                break r;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        f.ctx.run.approve(request, crate::inbox::ApprovalDecision::Deny);
        let r = pending.await.unwrap();
        assert_eq!(r.output, "The user denied this tool call.");
    }

    #[test]
    fn plan_mode_blocks_only_the_writers() {
        for (name, blocked) in [("write", true), ("edit", true), ("read", false), ("ls", false), ("grep", false), ("find", false)] {
            assert_eq!(super::super::find(name).unwrap().blocked_in_plan_mode(&json!({})), blocked, "{name}");
            assert_eq!(super::super::find(name).unwrap().parallel_safe(&json!({})), !blocked, "{name}");
        }
    }
}
