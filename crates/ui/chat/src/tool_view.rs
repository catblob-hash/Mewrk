//! What a tool card says about itself: upstream's `ToolRenderers.tsx` without its elements. The registry of every built-in
//! tool, the titles and figures of a row, and what each detail view shows, as data. `tool_detail` draws that data.
//!
//! Everything here is pure. Upstream's functions take a translator so that a test can ask for either language; `Lang` is
//! that argument here, and the public names without `_in` use the language the app is in.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use mewrk_client::machine_shells::{backend_of_tool, shell_backend_label};
use mewrk_client::model::{JsonObject, SubagentUpdate, ToolDescriptor, ToolResult};
use mewrk_client::runs::util::parse_iso;
use mewrk_client::runs::{StreamingToolPhase, SubagentLiveState};
use mewrk_pane_git::diff_output::parse_unified_diff_lines;
use mewrk_pane_tasks::context::{Ctx, LiveState, SubagentViewStatus, ToolRef};
use mewrk_pane_tasks::subagents::{agent_timeline_run_status, carries_child_run, initial_message, is_child_main_message_context, send_message_recipient};
use mewrk_pane_tasks::util::{collapse_spaces, is_js_space, js_trim};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::item::ToolItem;
use crate::orchestration::{parse_wait_output, questions_from_input, WaitEnvelopeView};

// ---------------------------------------------------------------- language

/// Upstream's `t` argument: the language to word a card in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub fn current() -> Self {
        if mewrk_client::i18n::is_english() { Self::En } else { Self::Zh }
    }

    pub fn t<'a>(self, zh: &'a str, en: &'a str) -> &'a str {
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }

    /// `{name}` parameters, in one pass as upstream's `interpolateTranslation`: a value holding `{reason}` stays as it is.
    pub fn f(self, zh: &str, en: &str, params: &[(&str, &dyn std::fmt::Display)]) -> String {
        let template = self.t(zh, en);
        let mut out = String::with_capacity(template.len() + 16);
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            rest = &rest[open..];
            let name_len = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len() - 1);
            let name = &rest[1..1 + name_len];
            if name_len > 0 && rest[1 + name_len..].starts_with('}') {
                match params.iter().find(|(k, _)| *k == name) {
                    Some((_, value)) => out.push_str(&value.to_string()),
                    None => out.push_str(&rest[..name_len + 2]),
                }
                rest = &rest[name_len + 2..];
            } else {
                out.push('{');
                rest = &rest[1..];
            }
        }
        out.push_str(rest);
        out
    }
}

// ---------------------------------------------------------------- public shapes

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolSurface {
    Group,
    Question,
    Workflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolViewFamily {
    FileList,
    Grep,
    Read,
    Diff,
    Terminal,
    BrowserSnapshot,
    BrowserEvaluate,
    BrowserScreenshot,
    BrowserConsole,
    BrowserNetwork,
    BrowserJson,
    BrowserPage,
    Memory,
    Question,
    AgentRun,
    AgentWait,
    AgentNote,
    WebSearch,
    Raw,
}

/// The bucket one block row is counted in when the block states what it did.
///
/// `Reasoning` and `Hook` have no registry entry, as they are not tool calls at all, but they share the bucket vocabulary
/// because they share the block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SummaryKind {
    Commands,
    FileChanges,
    Agents,
    Mcp,
    Browser,
    Device,
    Fork,
    Hook,
    Skill,
    Memory,
    Handoff,
    Search,
    Reads,
    Files,
    Reasoning,
    Other,
}

/// A title that names something, in parts: the words around the subject and the subject itself, so a row can set it apart
/// (a file to open, a pattern to read as code). The parts joined are the title.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TitleSubject {
    pub before: String,
    pub text: String,
    pub after: String,
    /// The subject is this file, as the call named it: a click opens it in the file pane.
    pub path: Option<String>,
    /// Where in the file the call started reading.
    pub line: Option<u32>,
    /// The subject is a pattern or a query, set in the code face.
    pub code: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolPresentation {
    pub surface: ToolSurface,
    pub family: ToolViewFamily,
    /// Lucide name, as imported upstream.
    pub icon: &'static str,
    pub title: String,
    /// See `TitleSubject`; absent, the title is a phrase with nothing to set apart.
    pub subject: Option<TitleSubject>,
    /// Lines a file change added and removed, from the diff the host made of the file when it wrote it, not Git's: the
    /// call's own change, whatever else the working tree holds.
    pub counts: Option<(u32, u32)>,
    /// A figure the title carries after its subject: "12 matches".
    pub note: Option<String>,
    pub target: Option<String>,
    /// The target is a path, drawn so that it gives way in the middle rather than cut at its end.
    pub target_is_path: bool,
    pub stat: Option<String>,
    /// The arguments worth reading, in the order the card shows them. `None` means all of them.
    pub keys: Option<&'static [&'static str]>,
}

/// `executionPhase`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolPhase {
    Done,
    Running,
    Failed,
}

struct SubjectTitle {
    subject: TitleSubject,
    counts: Option<(u32, u32)>,
    note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct ParsedJson {
    parsed: bool,
    value: Value,
}

// ---------------------------------------------------------------- JavaScript's strings and numbers

/// `String(n)`: no `.0` on a whole number, and the same switch to exponents.
pub(crate) fn js_number(n: f64) -> String {
    if n == 0.0 {
        return "0".to_owned();
    }
    if n.fract() == 0.0 && n.abs() < 1e21 {
        return format!("{n:.0}");
    }
    if n.abs() >= 1e21 || n.abs() < 1e-6 {
        let text = format!("{n:e}");
        return if text.contains("e-") { text } else { text.replacen('e', "e+", 1) };
    }
    format!("{n}")
}

/// `.length`: UTF-16 units.
fn js_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The longest prefix of at most `units` UTF-16 units. JavaScript would split a surrogate pair and show a broken character;
/// that half is dropped here.
fn utf16_prefix(s: &str, units: usize) -> &str {
    let mut used = 0;
    for (at, c) in s.char_indices() {
        used += c.len_utf16();
        if used > units {
            return &s[..at];
        }
    }
    s
}

fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_space)
}

/// `.` in a JavaScript regex does not cross these.
fn has_line_terminator(s: &str) -> bool {
    s.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
}

fn digits_len(s: &str) -> usize {
    s.bytes().take_while(u8::is_ascii_digit).count()
}

/// `\b` after an ASCII word character at the end of `head`: what follows does not continue the word.
fn word_ends(rest: &str) -> bool {
    rest.bytes().next().is_none_or(|b| !(b.is_ascii_alphanumeric() || b == b'_'))
}

/// `lib/ansi`'s `stripAnsi`: every escape sequence removed, colours included.
pub fn strip_ansi(input: &str) -> Cow<'_, str> {
    if !input.contains('\u{1b}') {
        return Cow::Borrowed(input);
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut copied = 0;
    let mut at = 0;
    while let Some(found) = input[at..].find('\u{1b}') {
        let esc = at + found;
        let end = match bytes.get(esc + 1) {
            Some(b'[') => {
                let mut i = esc + 2;
                while bytes.get(i).is_some_and(|b| b.is_ascii_digit() || *b == b';') {
                    i += 1;
                }
                bytes.get(i).filter(|b| b.is_ascii_alphabetic()).map(|_| i + 1)
            }
            Some(b']') => {
                let mut i = esc + 2;
                while bytes.get(i).is_some_and(|b| *b != 0x07 && *b != 0x1b) {
                    i += 1;
                }
                match (bytes.get(i), bytes.get(i + 1)) {
                    (Some(0x07), _) => Some(i + 1),
                    (Some(0x1b), Some(b'\\')) => Some(i + 2),
                    _ => None,
                }
            }
            Some(b'(' | b')') => bytes.get(esc + 2).filter(|b| b.is_ascii_alphanumeric()).map(|_| esc + 3),
            Some(b'=' | b'>') => Some(esc + 2),
            _ => None,
        };
        match end {
            Some(end) => {
                out.push_str(&input[copied..esc]);
                copied = end;
                at = end;
            }
            None => at = esc + 1,
        }
    }
    out.push_str(&input[copied..]);
    Cow::Owned(out)
}

// ---------------------------------------------------------------- reading a call

fn object(value: Option<&Value>) -> Option<&JsonObject> {
    value.and_then(Value::as_object)
}

fn input_string<'a>(item: &ToolItem<'a>, name: &str) -> Option<&'a str> {
    item.input.get(name).and_then(Value::as_str).map(js_trim).filter(|s| !s.is_empty())
}

fn input_number(item: &ToolItem<'_>, name: &str) -> Option<f64> {
    number_value(item.input.get(name))
}

fn input_flag(item: &ToolItem<'_>, name: &str) -> bool {
    item.input.get(name) == Some(&Value::Bool(true))
}

fn number_value(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

fn diff_of<'a>(item: &ToolItem<'a>) -> Option<&'a str> {
    item.result.diff.as_deref().filter(|d| !d.is_empty())
}

/// `web_fetch` titles show the first URL and summarize later URLs as `+N`.
fn first_url(item: &ToolItem<'_>) -> Option<String> {
    let urls = item.input.get("urls")?.as_array()?;
    let first = js_trim(urls.iter().filter_map(Value::as_str).find(|url| !js_trim(url).is_empty())?);
    let rest = urls.len() - 1;
    Some(if rest > 0 { format!("{first} +{rest}") } else { first.to_owned() })
}

const COMPACT_LIMIT: usize = 108;

fn cut_to(value: String, limit: usize) -> String {
    // UTF-16 units never outnumber bytes, so most values need no counting.
    if value.len() <= limit || js_len(&value) <= limit {
        return value;
    }
    format!("{}…", js_trim_end(utf16_prefix(&value, limit - 1)))
}

fn compact_to(value: Option<&str>, limit: usize) -> Option<String> {
    let value = value.filter(|v| !v.is_empty())?;
    let normalized = collapse_spaces(value);
    if normalized.is_empty() {
        return None;
    }
    Some(cut_to(normalized, limit))
}

fn compact(value: Option<&str>) -> Option<String> {
    compact_to(value, COMPACT_LIMIT)
}

/// A path target, whole: the surface that draws it shortens it in the middle (`PathText`).
fn path_target(value: Option<&str>) -> Option<String> {
    compact_to(value, usize::MAX)
}

fn target_selector(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    let uid = input_number(item, "uid").map(js_number).or_else(|| input_string(item, "uid").map(str::to_owned));
    match uid {
        Some(uid) => compact(Some(&format!("uid {uid}"))),
        None => compact(input_string(item, "selector").or_else(|| input_string(item, "ref"))),
    }
}

/// What a `preview_input` batch opens with, as one short line: its first action and where it lands (`click 100,200`) or
/// what it types or presses. The count of actions is the stat.
///
/// A `type` that pastes (`via: "paste"`) puts its text on the device's clipboard, where it stays, so the line says how much
/// was pasted and not what: `paste 12 characters`.
fn remote_input_target(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    let first = object(item.input.get("actions")?.as_array()?.first())?;
    let name = first.get("action")?.as_str()?;
    let text = first.get("text").and_then(Value::as_str);
    if name == "type"
        && first.get("via").and_then(Value::as_str) == Some("paste")
        && let Some(text) = text
    {
        return Some(lang.f("粘贴 {count} 字", "paste {count} characters", &[("count", &text.chars().count())]));
    }
    let detail = match (number_value(first.get("x")), number_value(first.get("y"))) {
        (Some(x), Some(y)) => Some(format!("{},{}", js_number(x), js_number(y))),
        _ => text.or_else(|| first.get("key").and_then(Value::as_str)).map(str::to_owned),
    };
    match detail.filter(|d| !d.is_empty()) {
        Some(detail) => compact(Some(&format!("{name} {detail}"))),
        None => compact(Some(name)),
    }
}

/// The target line of a tool of a remote-control screen: the parts that say what it acts on, in the order given, as one
/// short line.
fn screen_target(parts: &[Option<&str>]) -> Option<String> {
    let parts: Vec<&str> = parts.iter().flatten().copied().collect();
    compact(Some(&parts.join(" ")))
}

/// `launch com.android.settings`: the call's `action` (or the one it runs with when none is given), then what it acts on.
fn screen_action_target(item: &ToolItem<'_>, default_action: Option<&str>, subjects: &[Option<&str>]) -> Option<String> {
    let mut parts = vec![input_string(item, "action").or(default_action)];
    parts.extend_from_slice(subjects);
    screen_target(&parts)
}

/// `Math.round`: halves go up, towards positive infinity.
fn js_round(n: f64) -> f64 {
    (n + 0.5).floor()
}

/// `x,y width×height` of a call that names a region of the screen, in the frame of the latest screenshot.
fn screen_region(item: &ToolItem<'_>) -> Option<String> {
    let (x, y) = (input_number(item, "x")?, input_number(item, "y")?);
    let (width, height) = (input_number(item, "width")?, input_number(item, "height")?);
    Some(format!("{},{} {}×{}", js_number(js_round(x)), js_number(js_round(y)), js_number(js_round(width)), js_number(js_round(height))))
}

/// `(ms / 1000).toFixed(1)`. Formatting rounds an exact tie to even where `toFixed` rounds up, and for a count of
/// milliseconds such a tie is an odd multiple of 250.
fn seconds_text(ms: u64) -> String {
    if ms % 500 == 250 {
        let tenths = ms / 100 + 1;
        return format!("{}.{}", tenths / 10, tenths % 10);
    }
    format!("{:.1}", ms as f64 / 1000.0)
}

/// The screen tools can run for minutes (an install, a wait), so a long call reads in seconds.
fn screen_duration(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    let ms = item.result.duration_ms;
    if ms == 0 {
        return None;
    }
    Some(if ms >= 1000 { format!("{} s", seconds_text(ms)) } else { format!("{ms} ms") })
}

/// `android_shell` opens its answer with `exit <n>`; that status is the figure worth the row.
fn shell_exit_stat(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    let trimmed = js_trim(&item.result.output);
    if let Some(rest) = trimmed.strip_prefix("exit ") {
        let n = digits_len(rest);
        if n > 0 && word_ends(&rest[n..]) {
            return Some(format!("exit {}", &rest[..n]));
        }
    }
    screen_duration(item, lang)
}

/// `/^logcat .*?, last (\d+) lines?\b/`.
fn logcat_header_count(line: &str) -> Option<&str> {
    let mut rest = line.strip_prefix("logcat ")?;
    while let Some(at) = rest.find(", last ") {
        rest = &rest[at + ", last ".len()..];
        let n = digits_len(rest);
        if n > 0
            && let Some(after) = rest[n..].strip_prefix(" line")
            && word_ends(after.strip_prefix('s').unwrap_or(after))
        {
            return Some(&rest[..n]);
        }
    }
    None
}

/// `android_logcat` opens a read with one header that counts the lines it kept (`logcat default buffers, last 200 lines,
/// tag X:`), then prints them, then maybe a note that the log was cut; a search that matched nothing says so in one
/// sentence. The count is the header's, never the number of lines in the answer: an error is one line, and the note after a
/// cut log is not a log line. Without a header (a failed call, an answer in some other shape) the figure is the duration.
fn logcat_stat(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    if !item.result.success {
        return screen_duration(item, lang);
    }
    let trimmed = js_trim(&item.result.output);
    if trimmed == "No log lines matched." {
        return Some(lang.f("{count} 行", "{count} lines", &[("count", &0)]));
    }
    let first = trimmed.split('\n').next().unwrap_or("");
    let first = first.strip_suffix('\r').unwrap_or(first);
    match logcat_header_count(first).and_then(|n| n.parse::<f64>().ok()) {
        Some(count) => Some(lang.f("{count} 行", "{count} lines", &[("count", &js_number(count))])),
        None => screen_duration(item, lang),
    }
}

/// `preview_zoom` answers with JSON about the close-up it attached; its size is the figure worth the row.
fn zoom_stat(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    let parsed = parse_json_output(item);
    let image = parsed.parsed.then(|| object(parsed.value.get("image"))).flatten();
    let size = image.and_then(|image| Some(format!("{}×{}", js_number(number_value(image.get("width"))?), js_number(number_value(image.get("height"))?))));
    size.or_else(|| screen_duration(item, lang))
}

/// What a clipboard write carried: its length. A read, or a call with no text, has only its duration.
fn text_length_stat(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    if input_string(item, "action") == Some("read") {
        return screen_duration(item, lang);
    }
    match item.input.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        Some(text) => Some(lang.f("{count} 字", "{count} characters", &[("count", &js_len(text))])),
        None => screen_duration(item, lang),
    }
}

/// The one line a call with no registry entry can show about itself. Upstream takes the first scalar the schema declared;
/// `JsonObject` sorts its keys here, so this is the first in name order.
fn first_argument_summary(item: &ToolItem<'_>) -> Option<String> {
    for value in item.input.values() {
        match value {
            Value::String(s) => {
                if let Some(text) = compact(Some(s)) {
                    return Some(text);
                }
            }
            Value::Number(n) => return n.as_f64().map(js_number),
            Value::Bool(b) => return Some(b.to_string()),
            _ => {}
        }
    }
    None
}

/// `value.replace(/\r\n?/g, "\n").split("\n")`, without a line for what follows a final newline.
fn output_lines(value: &str) -> Vec<Cow<'_, str>> {
    let mut lines: Vec<Cow<'_, str>> = if value.contains('\r') {
        value.replace("\r\n", "\n").replace('\r', "\n").split('\n').map(|l| Cow::Owned(l.to_owned())).collect()
    } else {
        value.split('\n').map(Cow::Borrowed).collect()
    };
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

fn meaningful_count(value: &str, empty_messages: &[&str]) -> usize {
    let trimmed = js_trim(value);
    if trimmed.is_empty() || empty_messages.contains(&trimmed) {
        return 0;
    }
    // Parses a backend truncation sentinel, so it is not translated.
    output_lines(trimmed).iter().filter(|line| !js_trim(line).is_empty() && !line.starts_with("… 已达到")).count()
}

fn parse_json_output(item: &ToolItem<'_>) -> ParsedJson {
    let value = js_trim(&item.result.output);
    if value.is_empty() {
        return ParsedJson { parsed: false, value: Value::Null };
    }
    match serde_json::from_str::<Value>(value) {
        Ok(value) => ParsedJson { parsed: true, value },
        Err(_) => ParsedJson { parsed: false, value: Value::Null },
    }
}

fn browser_duration(item: &ToolItem<'_>) -> Option<String> {
    (item.result.duration_ms > 0).then(|| format!("{} ms", item.result.duration_ms))
}

fn duration_stat(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    browser_duration(item)
}

/// What the file tools answer with when they found nothing: the built-in profile's sentences, and the Chinese ones older
/// transcripts carry. They are the tool's words, not an entry it found.
const FILE_LIST_EMPTY_ANSWERS: [&str; 4] = ["(empty directory)", "No matching files", "（目录为空）", "未找到匹配文件"];
const GREP_EMPTY_ANSWERS: [&str; 2] = ["No matches found", "未找到匹配内容"];

/// `/^No matches at offset \d+; there are \d+\.$/`: a page asked for past the last entry, the tool's answer and not an entry.
fn is_grep_past_end(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("No matches at offset ") else { return false };
    let n = digits_len(rest);
    if n == 0 {
        return false;
    }
    let Some(rest) = rest[n..].strip_prefix("; there are ") else { return false };
    let n = digits_len(rest);
    n > 0 && &rest[n..] == "."
}

/// What `read` says in place of text: an empty file, a start past the last line. They are the tool's sentences, not lines
/// of the file.
const READ_EMPTY_FILE: &str = "(The file exists but is empty)";

/// `/^\(The file has only \d+ lines?, so the requested start_line is out of range\)$/`.
fn is_read_past_end(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("(The file has only ") else { return false };
    let n = digits_len(rest);
    if n == 0 {
        return false;
    }
    let Some(rest) = rest[n..].strip_prefix(" line") else { return false };
    rest.strip_prefix('s').unwrap_or(rest) == ", so the requested start_line is out of range)"
}

/// The note `read` ends with when it decoded the bytes for the model: lossily, or from UTF-16. It follows the text on a line
/// of its own, like the continuation note.
fn is_read_decode_note(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("(This file is ") else { return false };
    let opened = rest.strip_prefix("not valid UTF-8, ").or_else(|| rest.strip_prefix("UTF-16LE text, ")).or_else(|| rest.strip_prefix("UTF-16BE text, "));
    opened.is_some_and(|rest| rest.ends_with(')') && !has_line_terminator(rest))
}

fn file_list_stat(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    let count = meaningful_count(&item.result.output, &FILE_LIST_EMPTY_ANSWERS);
    Some(lang.f("{count} 项", "{count} items", &[("count", &count)]))
}

fn grep_stat(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    let parsed = parse_grep_output(item);
    Some(match parsed.mode {
        GrepMode::Content => lang.f("{count} 处匹配", "{count} matches", &[("count", &parsed.count)]),
        _ => lang.f("{count} 个文件", "{count} files", &[("count", &parsed.count)]),
    })
}

/// Past this many lines a `read` is shown uncoloured, as a file is (`CodeBlock.tsx`).
pub const READ_HIGHLIGHT_LINES: usize = 8000;

/// A line of `read` output that is the tool talking about the text rather than the text.
fn is_read_note(line: &str) -> bool {
    line.starts_with("… ") || is_read_decode_note(js_trim(line))
}

/// The lines of the file a `read` returned. An empty file and a start past the end come back as a sentence, and the notes
/// trail the text; none of them is a line.
fn read_text_lines(output: &str) -> Vec<Cow<'_, str>> {
    let lines = output_lines(output);
    if lines.len() == 1 && (js_trim(&lines[0]) == READ_EMPTY_FILE || is_read_past_end(js_trim(&lines[0]))) {
        return Vec::new();
    }
    lines.into_iter().filter(|line| !is_read_note(line)).collect()
}

fn read_stat(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    let images = &item.result.images;
    if images.len() == 1 {
        return Some(format!("{}×{}", images[0].width, images[0].height));
    }
    if images.len() > 1 {
        return Some(lang.f("{count} 张图片", "{count} images", &[("count", &images.len())]));
    }
    let start = input_number(item, "start_line").unwrap_or(1.0).floor().max(1.0);
    let requested_end = input_number(item, "end_line");
    let line_count = read_text_lines(&item.result.output).len();
    if line_count == 0 {
        return None;
    }
    let end = requested_end.map_or(start + line_count as f64 - 1.0, f64::floor);
    Some(lang.f("{start}–{end} 行", "{start}–{end} lines", &[("start", &js_number(start)), ("end", &js_number(start.max(end)))]))
}

fn diff_stat(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    let parsed = parse_unified_diff_lines(diff_of(item)?);
    Some(format!("+{} −{}", parsed.additions, parsed.deletions))
}

fn diff_creates_file(item: &ToolItem<'_>) -> bool {
    item.result.diff.as_deref().is_some_and(|diff| output_lines(diff).iter().any(|line| line == "--- /dev/null"))
}

fn write_title(item: &ToolItem<'_>, phase: ToolPhase, lang: Lang) -> Option<String> {
    Some(
        match phase {
            ToolPhase::Running => lang.t("正在写入文件", "Writing file"),
            ToolPhase::Failed => lang.t("写入文件失败", "Failed to write file"),
            ToolPhase::Done if diff_creates_file(item) => lang.t("创建了文件", "Created file"),
            ToolPhase::Done => lang.t("写入了文件", "Wrote file"),
        }
        .to_owned(),
    )
}

/// Stands where the subject goes in a translated title, so the parts around it can be found.
const SUBJECT_MARK: &str = "\u{0}";

/// `template` is a title translated with `SUBJECT_MARK` for its subject; the subject goes where the translation put it.
fn subject_title(template: &str, text: &str) -> TitleSubject {
    match template.split_once(SUBJECT_MARK) {
        Some((before, after)) => TitleSubject { before: before.to_owned(), text: text.to_owned(), after: after.to_owned(), ..Default::default() },
        None => TitleSubject { before: template.to_owned(), text: text.to_owned(), ..Default::default() },
    }
}

/// A path's last name: what a row calls the file.
fn base_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let name = trimmed.rfind(['/', '\\']).map_or(trimmed, |cut| &trimmed[cut + 1..]);
    if !name.is_empty() {
        name
    } else if !trimmed.is_empty() {
        trimmed
    } else {
        path
    }
}

fn diff_counts(item: &ToolItem<'_>) -> Option<(u32, u32)> {
    let parsed = parse_unified_diff_lines(diff_of(item)?);
    Some((parsed.additions as u32, parsed.deletions as u32))
}

fn read_subject(item: &ToolItem<'_>, lang: Lang) -> Option<SubjectTitle> {
    let path = input_string(item, "path")?;
    let start = input_number(item, "start_line").unwrap_or(1.0).floor();
    let mut subject = subject_title(&lang.f("已读取：{name}", "Read {name}", &[("name", &SUBJECT_MARK)]), base_name(path));
    subject.path = Some(path.to_owned());
    subject.line = (start > 1.0).then_some(start as u32);
    Some(SubjectTitle { subject, counts: None, note: None })
}

fn edit_subject(item: &ToolItem<'_>, lang: Lang) -> Option<SubjectTitle> {
    let path = input_string(item, "path")?;
    let mut subject = subject_title(&lang.f("已编辑：{name}", "Edited {name}", &[("name", &SUBJECT_MARK)]), base_name(path));
    subject.path = Some(path.to_owned());
    Some(SubjectTitle { subject, counts: diff_counts(item), note: None })
}

fn write_subject(item: &ToolItem<'_>, lang: Lang) -> Option<SubjectTitle> {
    let path = input_string(item, "path")?;
    let template = if diff_creates_file(item) {
        lang.f("已创建：{name}", "Created {name}", &[("name", &SUBJECT_MARK)])
    } else {
        lang.f("已写入：{name}", "Wrote {name}", &[("name", &SUBJECT_MARK)])
    };
    let mut subject = subject_title(&template, base_name(path));
    subject.path = Some(path.to_owned());
    Some(SubjectTitle { subject, counts: diff_counts(item), note: None })
}

/// `/^\(Showing \d+ of \d+/`.
fn says_showing(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("(Showing ") else { return false };
    let n = digits_len(rest);
    n > 0 && rest[n..].strip_prefix(" of ").is_some_and(|rest| digits_len(rest) > 0)
}

/// The files `find` listed, past its notes. A count the host had to stop short of reads as a floor: the list shows the
/// first of more matches.
fn find_count(item: &ToolItem<'_>) -> (usize, bool) {
    let lines = output_lines(&item.result.output);
    let lines: Vec<&str> = lines.iter().map(|line| js_trim(line)).filter(|line| !line.is_empty()).collect();
    let count = lines.iter().filter(|line| !line.starts_with('(') && !line.starts_with("… ") && !FILE_LIST_EMPTY_ANSWERS.contains(line)).count();
    (count, lines.iter().any(|line| says_showing(line)))
}

fn find_subject(item: &ToolItem<'_>, lang: Lang) -> Option<SubjectTitle> {
    let query = compact(input_string(item, "query"))?;
    let (count, more) = find_count(item);
    let mut subject = subject_title(&lang.f("已查找：{query}", "Found {query}", &[("query", &SUBJECT_MARK)]), &query);
    subject.code = true;
    let note = if more {
        lang.f("{count}+ 个结果", "{count}+ results", &[("count", &count)])
    } else {
        lang.f("{count} 个结果", "{count} results", &[("count", &count)])
    };
    Some(SubjectTitle { subject, counts: None, note: Some(note) })
}

fn grep_subject(item: &ToolItem<'_>, lang: Lang) -> Option<SubjectTitle> {
    let pattern = compact(input_string(item, "pattern"))?;
    let parsed = parse_grep_output(item);
    let count = &parsed.count;
    // The default mode answers with files, so a figure of "matches" would count the wrong thing.
    let note = match (parsed.mode, parsed.more) {
        (GrepMode::Content, true) => lang.f("{count}+ 处匹配", "{count}+ matches", &[("count", count)]),
        (GrepMode::Content, false) => lang.f("{count} 处匹配", "{count} matches", &[("count", count)]),
        (_, true) => lang.f("{count}+ 个文件", "{count}+ files", &[("count", count)]),
        (_, false) => lang.f("{count} 个文件", "{count} files", &[("count", count)]),
    };
    let mut subject = subject_title(&lang.f("已搜索：{pattern}", "Searched for {pattern}", &[("pattern", &SUBJECT_MARK)]), &pattern);
    subject.code = true;
    Some(SubjectTitle { subject, counts: None, note: Some(note) })
}

/// The pattern a search looked for, then what narrowed it: the files it was limited to and a mode other than the default.
/// It is what the card and the waiting line name the call by.
fn grep_target(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    let pattern = compact(input_string(item, "pattern"))?;
    let mut parts = vec![pattern];
    parts.extend(compact_to(input_string(item, "glob"), 48));
    match input_string(item, "output_mode") {
        Some("content") => parts.push(lang.t("内容", "content").to_owned()),
        Some("count") => parts.push(lang.t("计数", "count").to_owned()),
        _ => {}
    }
    Some(parts.join(" · "))
}

/// How much of an error's line a title carries; the row ellipsizes what does not fit.
const ERROR_EXCERPT_LIMIT: usize = 240;

/// The line of a failed call's output that says what went wrong: its first, or for a command the first after the
/// `Exit code N` it leads with, which says only that it failed.
pub fn error_excerpt(output: &str) -> Option<String> {
    let stripped = strip_ansi(output);
    let lines: Vec<&str> = stripped.split('\n').map(|line| js_trim(line.strip_suffix('\r').unwrap_or(line))).filter(|line| !line.is_empty()).collect();
    let says_exit_only = |line: &str| line.strip_prefix("Exit code ").is_some_and(|rest| !rest.is_empty() && !rest.contains(is_js_space));
    let line = if lines.len() > 1 && says_exit_only(lines[0]) { lines[1] } else { *lines.first()? };
    Some(cut_to(collapse_spaces(line), ERROR_EXCERPT_LIMIT))
}

/// A failed call's title: what failed, then why, the local model's reason or the error's own line.
fn failure_title(phrase: &str, reason: Option<&str>, lang: Lang) -> String {
    match reason {
        Some(reason) => lang.f("{title}：{reason}", "{title}: {reason}", &[("title", &phrase), ("reason", &reason)]),
        None => phrase.to_owned(),
    }
}

/// Non-empty output lines, which is how every text-answering preview tool reports its size.
fn output_line_count(item: &ToolItem<'_>) -> usize {
    output_lines(&item.result.output).iter().filter(|line| !js_trim(line).is_empty()).count()
}

/// Text-answering preview tools word their own empty case, and those sentences are the source's: they must not be counted
/// as one entry.
const PREVIEW_EMPTY_ANSWERS: [&str; 5] = ["No console logs.", "No network requests recorded.", "No failed requests.", "No logs yet.", "No output yet."];

fn preview_entry_stat(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    let trimmed = js_trim(&item.result.output);
    if trimmed.is_empty() {
        return browser_duration(item);
    }
    let count = if PREVIEW_EMPTY_ANSWERS.contains(&trimmed) { 0 } else { output_line_count(item) };
    Some(lang.f("{count} 条", "{count} entries", &[("count", &count)]))
}

fn screenshot_stat(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    let parsed = parse_json_output(item);
    let record = parsed.parsed.then(|| parsed.value.as_object()).flatten();
    let size = record.and_then(|r| Some(format!("{}×{}", js_number(number_value(r.get("width"))?), js_number(number_value(r.get("height"))?))));
    size.or_else(|| browser_duration(item))
}

/// `preview_resize` reports the size it was asked for; a preset carries no numbers.
fn resize_stat(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    match (input_number(item, "width"), input_number(item, "height")) {
        (Some(width), Some(height)) => Some(format!("{}×{}", js_number(width), js_number(height))),
        _ => browser_duration(item),
    }
}

/// What a host notice card says it is, by the kind its `notice` names. Mirrors the host's `handoff::NOTICE_KIND`,
/// `plan_mode`'s and `host_append`'s notice kinds and `wire_history::notice_kind`; a delivered background result names no
/// kind and keeps the default title.
fn host_notice_text(kind: &str, lang: Lang) -> Option<&'static str> {
    Some(match kind {
        "handoff" => lang.t("上下文已达自动压缩阈值，请求交接", "Context reached the auto-compact threshold; handoff requested"),
        "handoff_index" => lang.t("送达了交接文档索引", "Delivered the handoff notes index"),
        "plan_mode" => lang.t("送达了计划模式引导", "Delivered the plan mode guidance"),
        "plan_mode_exit" => lang.t("送达了计划模式结束说明", "Delivered the plan mode exit note"),
        "host_messages" => lang.t("送达了后台消息说明", "Delivered the note on background and host messages"),
        "structured_output" => lang.t("提醒通过 structured_output 返回结果", "Reminded to return the result through structured_output"),
        "hook_context" => lang.t("送达了钩子补充的上下文", "Delivered context from a hook"),
        "skill_added" => lang.t("送达了新增的技能", "Delivered a newly added skill"),
        "diagnostics" => lang.t("送达了语言服务器诊断", "Delivered language-server diagnostics"),
        "file_changes" => lang.t("送达了文件变更通知", "Delivered a file-change notice"),
        "mcp_unavailable" => lang.t("本轮有 MCP 服务器无法使用", "MCP servers unavailable this turn"),
        "instruction_skips" => lang.t("跳过了部分指令文件", "Skipped some instruction files"),
        "preview_start_failed" => lang.t("送达了开发服务器启动失败的通知", "Delivered a dev server start failure"),
        _ => return None,
    })
}

/// A card written before cards held the whole message named its kind in its input (`notification.kind`); those keep their
/// titles.
fn legacy_notice_kind<'a>(item: &ToolItem<'a>) -> Option<&'a str> {
    item.input.get("notification")?.as_object()?.get("kind")?.as_str()
}

fn host_notice_title(item: &ToolItem<'_>, _phase: ToolPhase, lang: Lang) -> Option<String> {
    let kind = item.notice.or_else(|| legacy_notice_kind(item)).filter(|kind| !kind.is_empty())?;
    host_notice_text(kind, lang).map(str::to_owned)
}

/// Memory tools take a plain document name and return plain Markdown, so the card shows only that name. There is no owner,
/// scope, version or byte count to project any more: a memory belongs to a location, not to a model.
fn memory_target(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    compact_to(input_string(item, "name"), 160)
}

/// How many task envelopes one `task_wait` call actually drained. A wait that returned nothing is a different event from
/// one that collected six results, and the row is the only place that difference shows without expanding.
///
/// Counted neutrally rather than as "updates": wait blocks through progress to the terminal result, so a normal return
/// carries the result *plus* however many updates piled up behind it.
fn wait_stat(item: &ToolItem<'_>, lang: Lang) -> Option<String> {
    if item.is_live_call() || !item.result.success {
        return None;
    }
    let count = parse_wait_output(&item.result.output).envelopes.len();
    (count > 0).then(|| lang.f("收到 {count} 条", "{count} collected", &[("count", &count)]))
}

/// One site a web call consulted, as the card shows it.
///
/// `host` is what the chip reads, because a row of page titles is a wall of text while a row of domains is scannable, and
/// the domain is what tells a reader whether the answer came from vendor documentation or a forum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebSourceChip {
    pub host: String,
    pub url: String,
    pub title: Option<String>,
}

fn trimmed_field<'a>(fields: Option<&'a JsonObject>, name: &str) -> &'a str {
    fields.and_then(|f| f.get(name)).and_then(Value::as_str).map_or("", js_trim)
}

/// The sites a `web_search` or `web_fetch` result names, deduplicated by host.
///
/// The two backends answer in different shapes and both are load-bearing: a provider-native search returns `{findings,
/// sources}` because its page text is sealed in the provider's own encrypted payload and only the URLs are host-readable,
/// while a catalog provider and every fetch return `{results:[{title,url,content}]}` with real text.
pub fn web_source_chips(item: &ToolItem<'_>) -> Vec<WebSourceChip> {
    web_source_chips_of(&parse_json_output(item).value)
}

fn web_source_chips_of(value: &Value) -> Vec<WebSourceChip> {
    let entries = ["sources", "results"].into_iter().filter_map(|name| value.get(name)?.as_array()).flatten();
    let mut chips: Vec<WebSourceChip> = Vec::new();
    for entry in entries {
        let fields = entry.as_object();
        let url = trimmed_field(fields, "url");
        if url.is_empty() {
            continue;
        }
        let Ok(parsed) = url::Url::parse(url) else { continue };
        if !matches!(parsed.scheme(), "http" | "https") {
            continue;
        }
        // `www.` is noise on a chip: it never distinguishes two sources and it costs four characters of a very narrow row.
        let host = parsed.host_str().unwrap_or("");
        let host = host.strip_prefix("www.").unwrap_or(host);
        if host.is_empty() || chips.iter().any(|chip| chip.host == host) {
            continue;
        }
        let title = trimmed_field(fields, "title");
        chips.push(WebSourceChip { host: host.to_owned(), url: url.to_owned(), title: (!title.is_empty()).then(|| title.to_owned()) });
    }
    chips
}

/// What stands for a site without its icon: the first letter of its domain.
pub fn source_letter(host: &str) -> String {
    host.chars().next().map_or_else(String::new, |c| c.to_uppercase().collect())
}

/// The collapsed row of a settled `web_search`: how many sites it read.
fn web_search_title(item: &ToolItem<'_>, phase: ToolPhase, lang: Lang) -> Option<String> {
    if phase != ToolPhase::Done {
        return None;
    }
    let count = web_source_chips(item).len();
    // A search that reports no sources still ran; falling back to the plain title is more honest than announcing zero sites.
    (count > 0).then(|| lang.f("已搜索 {count} 个网站", "Searched {count} sites", &[("count", &count)]))
}

/// A `ToolRef` of this call for the tasks pane's helpers, which ask only for a stream's status and whether it says anything.
/// A child's whole transcript is not worth a copy per row, so one context stands for all of them.
fn with_task_ref<R>(item: &ToolItem<'_>, f: impl FnOnce(&ToolRef<'_>) -> R) -> R {
    let live: Option<LiveState> = item.live.map(|live| skim_live(live));
    let copy = *item;
    f(&copy.task_ref(live.as_ref()))
}

fn skim_live(live: &SubagentLiveState) -> LiveState {
    let status = LiveState::from(&SubagentLiveState { status: live.status, ..Default::default() }).status;
    LiveState { contexts: live.contexts.first().map(Ctx::from).into_iter().collect(), updates: live.updates.clone(), status, usage_by_round: None }
}

fn send_message_family(item: &ToolItem<'_>) -> ToolViewFamily {
    // The retired child form travels upward and opens nothing, so it is a note rather than an agent run. The main agent's
    // call is the same note while it only queued a message (or was refused), and becomes an agent run once it resumed a
    // child and the child's stream or record rides on it.
    with_task_ref(item, |r| {
        if is_child_main_message_context(r) || (!send_message_recipient(r).is_empty() && !carries_child_run(r)) {
            ToolViewFamily::AgentNote
        } else {
            ToolViewFamily::AgentRun
        }
    })
}

fn send_message_title(item: &ToolItem<'_>, phase: ToolPhase, lang: Lang) -> Option<String> {
    with_task_ref(item, |r| {
        if is_child_main_message_context(r) {
            return Some(
                match phase {
                    ToolPhase::Running => lang.t("正在向主代理发送消息", "Sending message to main agent"),
                    ToolPhase::Failed => lang.t("向主代理发送消息失败", "Failed to send message to main agent"),
                    ToolPhase::Done => lang.t("向主代理发送了消息", "Sent message to main agent"),
                }
                .to_owned(),
            );
        }
        if !send_message_recipient(r).is_empty() {
            return Some(
                match phase {
                    ToolPhase::Running => lang.t("正在发送消息", "Sending message"),
                    ToolPhase::Failed => lang.t("发送消息失败", "Failed to send message"),
                    ToolPhase::Done => lang.t("发送了消息", "Sent message"),
                }
                .to_owned(),
            );
        }
        None
    })
}

fn send_message_target(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    with_task_ref(item, |r| {
        if is_child_main_message_context(r) {
            return compact(input_string(item, "message"));
        }
        let to = send_message_recipient(r);
        if to.is_empty() {
            return compact(input_string(item, "target"));
        }
        match input_string(item, "summary") {
            Some(summary) => compact(Some(&format!("→ {to} · {summary}"))),
            None => compact(Some(&format!("→ {to}"))),
        }
    })
}

fn plan_title(item: &ToolItem<'_>, phase: ToolPhase, lang: Lang) -> Option<String> {
    Some(
        match phase {
            ToolPhase::Running => lang.t("正在撰写计划", "Writing the plan"),
            ToolPhase::Failed => lang.t("计划操作失败", "Plan operation failed"),
            ToolPhase::Done if input_string(item, "action") == Some("read") => lang.t("已读取计划", "Read the plan"),
            ToolPhase::Done => lang.t("已更新计划", "Updated the plan"),
        }
        .to_owned(),
    )
}

// ---------------------------------------------------------------- the registry

type Words = (&'static str, &'static str);
type Figure = fn(&ToolItem<'_>, Lang) -> Option<String>;

#[derive(Clone, Copy)]
struct ToolViewConfig {
    surface: ToolSurface,
    family: ToolViewFamily,
    icon: &'static str,
    done: Words,
    running: Option<Words>,
    failed: Option<Words>,
    /// `None` falls back to the phase's static title.
    resolve_title: Option<fn(&ToolItem<'_>, ToolPhase, Lang) -> Option<String>>,
    /// A finished call's title that names what it acted on, in place of the phrase. `None` (no path, no pattern) keeps the
    /// phrase.
    resolve_subject: Option<fn(&ToolItem<'_>, Lang) -> Option<SubjectTitle>>,
    /// A tool whose one wire name carries more than one contract resolves its detail family per call.
    resolve_family: Option<fn(&ToolItem<'_>) -> ToolViewFamily>,
    target: Option<Figure>,
    target_is_path: bool,
    stat: Option<Figure>,
    /// A call carries whatever plumbing its transport needed; the reader only wants the few that say what it did. Omitted
    /// means "show them all", the honest answer for a tool nobody has curated.
    keys: Option<&'static [&'static str]>,
    summary_kind: SummaryKind,
}

const BASE: ToolViewConfig = ToolViewConfig {
    surface: ToolSurface::Group,
    family: ToolViewFamily::Raw,
    icon: "Wrench",
    done: ("", ""),
    running: None,
    failed: None,
    resolve_title: None,
    resolve_subject: None,
    resolve_family: None,
    target: None,
    target_is_path: false,
    stat: None,
    keys: None,
    summary_kind: SummaryKind::Other,
};

fn command_target(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    compact(input_string(item, "command"))
}

fn input_path_target(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    path_target(input_string(item, "path"))
}

fn message_target(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    compact(input_string(item, "message"))
}

fn name_target(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    compact(input_string(item, "name"))
}

fn label_or_name_target(item: &ToolItem<'_>, _: Lang) -> Option<String> {
    compact(input_string(item, "label").or_else(|| input_string(item, "name")))
}

const HOST_MESSAGE_VIEW: ToolViewConfig = ToolViewConfig {
    icon: "Inbox",
    done: ("送达了后台结果", "Delivered a background result"),
    resolve_title: Some(host_notice_title),
    keys: Some(&[]),
    summary_kind: SummaryKind::Agents,
    ..BASE
};

use SummaryKind as K;
use ToolViewFamily as F;

/// Every built-in tool is intentionally named here. Similar tools share a renderer family, but no tool silently falls into
/// a category-wide default.
static TOOL_VIEW_REGISTRY: &[(&str, ToolViewConfig)] = &[
    ("ls", ToolViewConfig { family: F::FileList, icon: "FolderTree", done: ("查看了目录", "Viewed directory"), running: Some(("正在查看目录", "Viewing directory")), failed: Some(("查看目录失败", "Failed to view directory")), target: Some(|item, _| path_target(Some(input_string(item, "path").unwrap_or(".")))), target_is_path: true, stat: Some(file_list_stat), summary_kind: K::Files, ..BASE }),
    ("grep", ToolViewConfig { family: F::Grep, icon: "Search", done: ("搜索了文件内容", "Searched file contents"), resolve_subject: Some(grep_subject), running: Some(("正在搜索文件内容", "Searching file contents")), failed: Some(("搜索文件内容失败", "File-content search failed")), target: Some(grep_target), stat: Some(grep_stat), summary_kind: K::Search, ..BASE }),
    ("pwsh", ToolViewConfig { family: F::Terminal, icon: "SquareTerminal", done: ("运行了 PowerShell 7 命令", "Ran PowerShell 7 command"), running: Some(("正在运行 PowerShell 7 命令", "Running PowerShell 7 command")), failed: Some(("PowerShell 7 命令失败", "PowerShell 7 command failed")), target: Some(command_target), stat: Some(duration_stat), summary_kind: K::Commands, ..BASE }),
    ("powershell", ToolViewConfig { family: F::Terminal, icon: "SquareTerminal", done: ("运行了 Windows PowerShell 命令", "Ran Windows PowerShell command"), running: Some(("正在运行 Windows PowerShell 命令", "Running Windows PowerShell command")), failed: Some(("Windows PowerShell 命令失败", "Windows PowerShell command failed")), target: Some(command_target), stat: Some(duration_stat), summary_kind: K::Commands, ..BASE }),
    ("bash", ToolViewConfig { family: F::Terminal, icon: "SquareTerminal", done: ("运行了 Bash 命令", "Ran Bash command"), running: Some(("正在运行 Bash 命令", "Running Bash command")), failed: Some(("Bash 命令失败", "Bash command failed")), target: Some(command_target), stat: Some(duration_stat), summary_kind: K::Commands, ..BASE }),
    ("zsh", ToolViewConfig { family: F::Terminal, icon: "SquareTerminal", done: ("运行了 zsh 命令", "Ran zsh command"), running: Some(("正在运行 zsh 命令", "Running zsh command")), failed: Some(("zsh 命令失败", "zsh command failed")), target: Some(command_target), stat: Some(duration_stat), summary_kind: K::Commands, ..BASE }),
    ("sh", ToolViewConfig { family: F::Terminal, icon: "SquareTerminal", done: ("运行了 sh 命令", "Ran sh command"), running: Some(("正在运行 sh 命令", "Running sh command")), failed: Some(("sh 命令失败", "sh command failed")), target: Some(command_target), stat: Some(duration_stat), summary_kind: K::Commands, ..BASE }),
    ("write", ToolViewConfig { family: F::Diff, icon: "FilePenLine", done: ("写入了文件", "Wrote file"), resolve_title: Some(write_title), resolve_subject: Some(write_subject), target: Some(input_path_target), target_is_path: true, stat: Some(diff_stat), summary_kind: K::FileChanges, ..BASE }),
    ("edit", ToolViewConfig { family: F::Diff, icon: "FilePenLine", done: ("编辑了文件", "Edited file"), resolve_subject: Some(edit_subject), running: Some(("正在编辑文件", "Editing file")), failed: Some(("编辑文件失败", "Failed to edit file")), target: Some(input_path_target), target_is_path: true, stat: Some(diff_stat), summary_kind: K::FileChanges, ..BASE }),
    ("find", ToolViewConfig { family: F::FileList, icon: "FileSearch", done: ("查找了文件", "Found files"), resolve_subject: Some(find_subject), running: Some(("正在查找文件", "Finding files")), failed: Some(("查找文件失败", "Failed to find files")), target: Some(|item, _| compact(input_string(item, "query"))), stat: Some(file_list_stat), summary_kind: K::Files, ..BASE }),
    ("read", ToolViewConfig { family: F::Read, icon: "FileText", done: ("读取了文件", "Read file"), resolve_subject: Some(read_subject), running: Some(("正在读取文件", "Reading file")), failed: Some(("读取文件失败", "Failed to read file")), target: Some(input_path_target), target_is_path: true, stat: Some(read_stat), summary_kind: K::Reads, ..BASE }),
    ("lsp", ToolViewConfig { icon: "Waypoints", done: ("查询了代码语义", "Queried code semantics"), running: Some(("正在查询代码语义", "Querying code semantics")), failed: Some(("查询代码语义失败", "Code semantics query failed")), target: Some(|item, _| path_target(Some(input_string(item, "filePath").unwrap_or("")))), target_is_path: true, keys: Some(&["operation", "filePath", "line", "character", "query"]), summary_kind: K::Search, ..BASE }),

    ("workflow", ToolViewConfig { surface: ToolSurface::Workflow, icon: "Workflow", done: ("完成了工作流", "Completed workflow"), running: Some(("正在运行工作流", "Running workflow")), failed: Some(("工作流运行失败", "Workflow failed")), keys: Some(&["name"]), summary_kind: K::Agents, ..BASE }),
    ("workflow_step", ToolViewConfig { family: F::AgentRun, icon: "Bot", done: ("完成了工作流步骤", "Completed workflow step"), running: Some(("正在运行工作流步骤", "Running workflow step")), failed: Some(("工作流步骤失败", "Workflow step failed")), target: Some(label_or_name_target), summary_kind: K::Agents, ..BASE }),

    // A search names the sites it read, so the card collapses to that count and opens onto one chip per site. Fetching is
    // told what to read, so it keeps the ordinary card: the URLs are already in the call's own arguments.
    ("web_search", ToolViewConfig { family: F::WebSearch, icon: "Search", done: ("完成了联网搜索", "Completed web search"), running: Some(("正在联网搜索", "Searching the web")), failed: Some(("联网搜索失败", "Web search failed")), resolve_title: Some(web_search_title), target: Some(|item, _| compact(input_string(item, "query"))), keys: Some(&["query"]), summary_kind: K::Search, ..BASE }),
    ("web_fetch", ToolViewConfig { icon: "Globe", done: ("抓取了网页", "Fetched pages"), running: Some(("正在抓取网页", "Fetching pages")), failed: Some(("抓取网页失败", "Failed to fetch pages")), target: Some(|item, _| compact(first_url(item).as_deref())), keys: Some(&["urls"]), summary_kind: K::Search, ..BASE }),

    // Sixteen flat preview tools, each its own wire tool. Thirteen are copied from Claude Code and answer in prose the tool
    // wrote itself, so most of them render that text as-is; only the calls that answer with JSON or pixels get a structured
    // family.
    ("preview_start", ToolViewConfig { icon: "MonitorPlay", done: ("启动了预览服务器", "Started preview server"), running: Some(("正在启动预览服务器", "Starting preview server")), failed: Some(("启动预览服务器失败", "Failed to start preview server")), target: Some(name_target), stat: Some(duration_stat), keys: Some(&["name"]), summary_kind: K::Browser, ..BASE }),
    ("preview_stop", ToolViewConfig { icon: "MonitorX", done: ("停止了预览服务器", "Stopped preview server"), running: Some(("正在停止预览服务器", "Stopping preview server")), failed: Some(("停止预览服务器失败", "Failed to stop preview server")), target: Some(|item, _| compact(input_string(item, "serverId"))), stat: Some(duration_stat), keys: Some(&["serverId"]), summary_kind: K::Browser, ..BASE }),
    ("preview_list", ToolViewConfig { family: F::BrowserJson, icon: "Server", done: ("列出了预览服务器", "Listed preview servers"), running: Some(("正在列出预览服务器", "Listing preview servers")), failed: Some(("列出预览服务器失败", "Failed to list preview servers")), keys: Some(&[]), stat: Some(|item, lang| match parse_json_output(item).value.as_array() {
        Some(servers) => Some(lang.f("{count} 个服务器", "{count} servers", &[("count", &servers.len())])),
        None => browser_duration(item),
    }), summary_kind: K::Browser, ..BASE }),
    ("preview_logs", ToolViewConfig { icon: "Logs", done: ("读取了服务器日志", "Read server logs"), running: Some(("正在读取服务器日志", "Reading server logs")), failed: Some(("读取服务器日志失败", "Failed to read server logs")), target: Some(|item, _| compact(input_string(item, "search"))), stat: Some(preview_entry_stat), keys: Some(&["search", "level", "lines"]), summary_kind: K::Browser, ..BASE }),
    ("preview_console_logs", ToolViewConfig { family: F::BrowserConsole, icon: "Logs", done: ("读取了 Console 日志", "Read Console logs"), running: Some(("正在读取 Console 日志", "Reading Console logs")), failed: Some(("读取 Console 日志失败", "Failed to read Console logs")), target: Some(|item, _| compact(input_string(item, "level"))), stat: Some(preview_entry_stat), summary_kind: K::Browser, ..BASE }),
    ("preview_screenshot", ToolViewConfig { family: F::BrowserScreenshot, icon: "Image", done: ("截取了页面", "Captured page"), running: Some(("正在截取页面", "Capturing page")), failed: Some(("页面截图失败", "Page capture failed")), stat: Some(screenshot_stat), summary_kind: K::Browser, ..BASE }),
    ("preview_snapshot", ToolViewConfig { family: F::BrowserSnapshot, icon: "ScanSearch", done: ("读取了页面快照", "Read page snapshot"), running: Some(("正在读取页面快照", "Reading page snapshot")), failed: Some(("读取页面快照失败", "Failed to read page snapshot")), stat: Some(|item, lang| {
        let count = output_line_count(item);
        if count > 0 { Some(lang.f("{count} 个节点", "{count} nodes", &[("count", &count)])) } else { browser_duration(item) }
    }), summary_kind: K::Browser, ..BASE }),
    ("preview_inspect", ToolViewConfig { family: F::BrowserJson, icon: "Crosshair", done: ("检查了页面元素", "Inspected page element"), running: Some(("正在检查页面元素", "Inspecting page element")), failed: Some(("检查页面元素失败", "Failed to inspect page element")), target: Some(target_selector), stat: Some(duration_stat), keys: Some(&["selector", "styles"]), summary_kind: K::Browser, ..BASE }),
    ("preview_click", ToolViewConfig { icon: "MousePointerClick", done: ("点击了页面元素", "Clicked page element"), running: Some(("正在点击页面元素", "Clicking page element")), failed: Some(("点击页面元素失败", "Failed to click page element")), target: Some(target_selector), keys: Some(&["selector", "uid", "doubleClick"]), stat: Some(|item, lang| if input_flag(item, "doubleClick") { Some(lang.t("双击", "Double-click").to_owned()) } else { browser_duration(item) }), summary_kind: K::Browser, ..BASE }),
    ("preview_fill", ToolViewConfig { icon: "TextCursorInput", done: ("填写了页面输入", "Filled page input"), running: Some(("正在填写页面输入", "Filling page input")), failed: Some(("填写页面输入失败", "Failed to fill page input")), target: Some(target_selector), keys: Some(&["selector", "uid", "value"]), stat: Some(|item, lang| match input_string(item, "value") {
        Some(value) => Some(lang.f("{count} 字", "{count} characters", &[("count", &js_len(value))])),
        None => browser_duration(item),
    }), summary_kind: K::Browser, ..BASE }),
    ("preview_eval", ToolViewConfig { family: F::BrowserEvaluate, icon: "Code2", done: ("执行了页面脚本", "Ran page script"), running: Some(("正在执行页面脚本", "Running page script")), failed: Some(("页面脚本执行失败", "Page script failed")), target: Some(|item, _| compact(input_string(item, "expression"))), stat: Some(duration_stat), summary_kind: K::Browser, ..BASE }),
    ("preview_network", ToolViewConfig { family: F::BrowserNetwork, icon: "Network", done: ("读取了网络请求", "Read network requests"), running: Some(("正在读取网络请求", "Reading network requests")), failed: Some(("读取网络请求失败", "Failed to read network requests")), target: Some(|item, _| compact(input_string(item, "requestId").or_else(|| input_string(item, "filter")))), stat: Some(preview_entry_stat), summary_kind: K::Browser, ..BASE }),
    ("preview_resize", ToolViewConfig { icon: "MonitorUp", done: ("调整了预览视口", "Resized preview viewport"), running: Some(("正在调整预览视口", "Resizing preview viewport")), failed: Some(("调整预览视口失败", "Failed to resize preview viewport")), target: Some(|item, _| compact(input_string(item, "preset").or_else(|| input_string(item, "colorScheme")))), stat: Some(resize_stat), keys: Some(&["preset", "width", "height", "colorScheme"]), summary_kind: K::Browser, ..BASE }),
    ("preview_upload_image", ToolViewConfig { family: F::BrowserPage, icon: "Upload", done: ("上传了对话图片", "Uploaded conversation image"), running: Some(("正在上传对话图片", "Uploading conversation image")), failed: Some(("上传对话图片失败", "Failed to upload conversation image")), target: Some(target_selector), stat: Some(duration_stat), summary_kind: K::Browser, ..BASE }),
    // On the web page the answer is a JSON object about the page; on a phone or tablet it is a plain sentence, which the
    // page card would call "not valid JSON". Only an answer that opens like an object (or says nothing) is read as the page's.
    ("preview_dialog", ToolViewConfig { family: F::BrowserPage, resolve_family: Some(|item| {
        let output = item.result.output.trim_start_matches(is_js_space);
        if output.is_empty() || output.starts_with('{') { F::BrowserPage } else { F::Raw }
    }), icon: "CircleAlert", done: ("回答了对话框", "Answered a dialog"), running: Some(("正在回答对话框", "Answering a dialog")), failed: Some(("回答对话框失败", "Failed to answer a dialog")), target: Some(|item, _| compact(input_string(item, "prompt_text"))), stat: Some(|item, lang| {
        // On a phone or tablet the call names the button to press; the label is the figure worth the row.
        if let Some(button) = compact_to(input_string(item, "button"), 48) {
            return Some(lang.f("按下“{button}”", "Press \"{button}\"", &[("button", &button)]));
        }
        Some(if item.input.get("accept") == Some(&Value::Bool(false)) { lang.t("取消", "Dismiss") } else { lang.t("接受", "Accept") }.to_owned())
    }), summary_kind: K::Browser, ..BASE }),
    // Mewrk's own, like the two above: a batch of pixel-level actions on a remote-control screen. It answers in text, so it
    // renders as the raw text; a screenshot it was asked for travels as a tool image, not in this card's output.
    ("preview_input", ToolViewConfig { icon: "Smartphone", done: ("操作了遥控屏幕", "Operated remote screen"), running: Some(("正在操作遥控屏幕", "Operating remote screen")), failed: Some(("操作遥控屏幕失败", "Failed to operate remote screen")), target: Some(remote_input_target), keys: Some(&["serverId", "actions", "screenshot"]), stat: Some(|item, lang| {
        let count = item.input.get("actions").and_then(Value::as_array).map_or(0, Vec::len);
        if count > 0 { Some(lang.f("{count} 个动作", "{count} actions", &[("count", &count)])) } else { browser_duration(item) }
    }), summary_kind: K::Device, ..BASE }),

    // The tools of a remote-control screen: `preview_zoom` and `preview_wait` (which act on screens only, for now), the
    // `screen_*` tools and the two Android-only ones. Like `preview_input` they answer in the plain lines the host wrote, so
    // they render as the raw text, with the arguments worth reading above it; the one picture (`preview_zoom`'s close-up)
    // travels as a tool image and draws through the shared thumbnail strip. They count as device actions: no browser is
    // involved when a call runs a device shell or opens an app.
    ("preview_zoom", ToolViewConfig { icon: "ZoomIn", done: ("放大了屏幕局部", "Zoomed into screen"), running: Some(("正在放大屏幕局部", "Zooming into screen")), failed: Some(("放大屏幕局部失败", "Failed to zoom into screen")), target: Some(|item, _| screen_region(item)), stat: Some(zoom_stat), keys: Some(&["serverId", "x", "y", "width", "height"]), summary_kind: K::Device, ..BASE }),
    ("preview_wait", ToolViewConfig { icon: "Hourglass", done: ("等待了屏幕画面", "Waited on screen"), running: Some(("正在等待屏幕画面", "Waiting on screen")), failed: Some(("等待屏幕画面失败", "Screen wait failed")), target: Some(|item, _| screen_target(&[Some(input_string(item, "until").unwrap_or("change")), screen_region(item).as_deref()])), stat: Some(screen_duration), keys: Some(&["serverId", "until", "timeout", "x", "y", "width", "height"]), summary_kind: K::Device, ..BASE }),
    ("screen_app", ToolViewConfig { icon: "Rocket", done: ("操作了屏幕应用", "Operated screen app"), running: Some(("正在操作屏幕应用", "Operating screen app")), failed: Some(("操作屏幕应用失败", "Failed to operate screen app")), target: Some(|item, _| screen_action_target(item, Some("list"), &[input_string(item, "url"), input_string(item, "app"), input_string(item, "activity"), input_string(item, "filter")])), stat: Some(screen_duration), keys: Some(&["serverId", "action", "app", "url", "filter", "limit", "scope", "activity", "restart", "arguments", "environment"]), summary_kind: K::Device, ..BASE }),
    ("screen_device", ToolViewConfig { icon: "SlidersHorizontal", done: ("操作了屏幕设备", "Operated screen device"), running: Some(("正在操作屏幕设备", "Operating screen device")), failed: Some(("操作屏幕设备失败", "Failed to operate screen device")), target: Some(|item, _| {
        let place = match (input_number(item, "latitude"), input_number(item, "longitude")) {
            (Some(latitude), Some(longitude)) => Some(format!("{},{}", js_number(latitude), js_number(longitude))),
            _ => None,
        };
        let sections = item.input.get("sections").and_then(Value::as_array).map(|sections| sections.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).filter(|s| !s.is_empty());
        screen_action_target(item, Some("info"), &[input_string(item, "value"), place.as_deref(), sections.as_deref()])
    }), stat: Some(screen_duration), keys: Some(&["serverId", "action", "value", "sections", "latitude", "longitude"]), summary_kind: K::Device, ..BASE }),
    ("screen_clipboard", ToolViewConfig { icon: "Clipboard", done: ("操作了屏幕剪贴板", "Used screen clipboard"), running: Some(("正在操作屏幕剪贴板", "Using screen clipboard")), failed: Some(("操作屏幕剪贴板失败", "Screen clipboard failed")), target: Some(|item, _| screen_action_target(item, None, &[])), stat: Some(text_length_stat), keys: Some(&["serverId", "action", "text"]), summary_kind: K::Device, ..BASE }),
    ("screen_package", ToolViewConfig { icon: "PackagePlus", done: ("管理了应用安装包", "Managed app packages"), running: Some(("正在管理应用安装包", "Managing app packages")), failed: Some(("管理应用安装包失败", "Failed to manage app packages")), target: Some(|item, _| screen_action_target(item, None, &[input_string(item, "path"), input_string(item, "app"), input_string(item, "permission")])), stat: Some(screen_duration), keys: Some(&["serverId", "action", "path", "app", "permission", "keep_data", "replace_existing", "downgrade", "grant_permissions"]), summary_kind: K::Device, ..BASE }),
    ("android_logcat", ToolViewConfig { icon: "ScrollText", done: ("读取了 Android 日志", "Read Android logs"), running: Some(("正在读取 Android 日志", "Reading Android logs")), failed: Some(("读取 Android 日志失败", "Failed to read Android logs")), target: Some(|item, _| compact(input_string(item, "package").or_else(|| input_string(item, "tag")).or_else(|| input_string(item, "contains")).or_else(|| input_string(item, "buffer")))), stat: Some(logcat_stat), keys: Some(&["serverId", "lines", "level", "tag", "package", "buffer", "contains"]), summary_kind: K::Device, ..BASE }),
    ("android_shell", ToolViewConfig { icon: "SquareTerminal", done: ("运行了 Android Shell 命令", "Ran Android shell command"), running: Some(("正在运行 Android Shell 命令", "Running Android shell command")), failed: Some(("Android Shell 命令失败", "Android shell command failed")), target: Some(command_target), stat: Some(shell_exit_stat), keys: Some(&["serverId", "command", "timeout"]), summary_kind: K::Device, ..BASE }),

    // Agent-protocol tools are ordinary rows inside the tool block: one uniform disclosure per contiguous tool run, chevron
    // to tailored detail. `AgentRun` carries a child transcript (instruction, updates, returned text) and is the only family
    // that offers the inline "open subagent" affordance; `AgentWait` splits the drained `[name · status]` envelopes;
    // `AgentNote` is a one-shot message whose payload already sits in the row's target.
    ("agent_spawn", ToolViewConfig { family: F::AgentRun, icon: "Bot", done: ("派生了子代理", "Spawned subagent"), running: Some(("正在派生子代理", "Spawning subagent")), failed: Some(("派生子代理失败", "Failed to spawn subagent")), target: Some(label_or_name_target), summary_kind: K::Agents, ..BASE }),
    // `send_message` is the main agent's tool for messaging a child it started (`to`, `message`, optional `summary`); it is
    // host-derived, never in the catalog. One wire name carries three shapes: that call, the retired queue-only call
    // (`target`, `message`) and the retired child-to-main note (`message` alone). `agent_send` and `followup_task` are
    // retired names kept so saved conversations still render their cards; nothing issues them.
    ("agent_send", ToolViewConfig { family: F::AgentRun, icon: "MessageCircleMore", done: ("向子代理发送了消息", "Sent message to subagent"), running: Some(("正在向子代理发送消息", "Sending message to subagent")), failed: Some(("发送子代理消息失败", "Failed to send subagent message")), target: Some(|item, _| compact(input_string(item, "agent"))), summary_kind: K::Agents, ..BASE }),
    ("send_message", ToolViewConfig { family: F::AgentRun, icon: "MessageCircleMore", done: ("消息已加入子代理队列", "Queued message for subagent"), running: Some(("正在排队子代理消息", "Queueing message for subagent")), failed: Some(("排队子代理消息失败", "Failed to queue subagent message")), resolve_family: Some(send_message_family), resolve_title: Some(send_message_title), target: Some(send_message_target), summary_kind: K::Agents, ..BASE }),
    ("followup_task", ToolViewConfig { family: F::AgentRun, icon: "MessageCircleMore", done: ("向子代理追加了任务", "Sent follow-up to subagent"), running: Some(("正在追加子代理任务", "Sending subagent follow-up")), failed: Some(("追加子代理任务失败", "Failed to follow up with subagent")), target: Some(|item, _| compact(input_string(item, "target"))), summary_kind: K::Agents, ..BASE }),
    ("task_wait", ToolViewConfig { family: F::AgentWait, icon: "Timer", done: ("等待了任务", "Waited for tasks"), running: Some(("正在等待任务", "Waiting for tasks")), failed: Some(("等待任务失败", "Failed to wait for tasks")), stat: Some(wait_stat), summary_kind: K::Agents, ..BASE }),
    ("task_list", ToolViewConfig { family: F::AgentNote, icon: "ListChecks", done: ("查看了任务列表", "Viewed task list"), running: Some(("正在查看任务列表", "Viewing task list")), failed: Some(("查看任务列表失败", "Failed to view task list")), summary_kind: K::Agents, ..BASE }),
    // Unlike `task_list`, this call names the task it acts on, so it takes the raw card: the address shows as an argument
    // beside the host's receipt, and the row keeps its phrase instead of trading it for a note's own text.
    ("task_stop", ToolViewConfig { icon: "OctagonX", done: ("停止了任务", "Stopped a task"), running: Some(("正在停止任务", "Stopping a task")), failed: Some(("停止任务失败", "Failed to stop a task")), target: Some(|item, _| compact(input_string(item, "task"))), keys: Some(&["task"]), summary_kind: K::Agents, ..BASE }),
    // A message the host handed the model (the host's `wire_history::HOST_CARD_TOOL`): a user-role message on the wire, or a
    // deferred call's output. The host owns the body, so the row only ever settles as done; the message is the result, hence
    // raw detail. A notice names itself in the card's `notice` so the row can say what arrived.
    ("host_message", HOST_MESSAGE_VIEW),
    // The same message where the conversation's host messages come in `box`: the result of the call the host wrote for it.
    ("box", HOST_MESSAGE_VIEW),

    // Skills return instruction text, so they use raw detail; the skill name is this call's only input and becomes its target.
    ("skill", ToolViewConfig { icon: "Box", done: ("加载了技能", "Loaded skill"), running: Some(("正在加载技能", "Loading skill")), failed: Some(("加载技能失败", "Failed to load skill")), target: Some(name_target), keys: Some(&["name"]), summary_kind: K::Skill, ..BASE }),
    // Tool discovery returns a `<functions>` block, which is raw text for the same reason a skill body is. Its target is the
    // query, because that is what the reader wants to see the run asked for; the MCP summary groups it with the calls it
    // enables.
    ("tool_search", ToolViewConfig { icon: "Blocks", done: ("取回了工具定义", "Fetched tool definitions"), running: Some(("正在查找工具", "Searching for tools")), failed: Some(("查找工具失败", "Tool search failed")), target: Some(|item, _| compact(input_string(item, "query"))), keys: Some(&["query", "max_results"]), summary_kind: K::Mcp, ..BASE }),
    ("subagent", ToolViewConfig { family: F::AgentRun, icon: "Bot", done: ("运行了子代理", "Ran subagent"), running: Some(("子代理正在工作", "Subagent is working")), failed: Some(("子代理已中断", "Subagent was interrupted")), target: Some(|item, _| compact(input_string(item, "label"))), summary_kind: K::Agents, ..BASE }),
    ("subagent_update", ToolViewConfig { family: F::AgentNote, icon: "MessageCircleMore", done: ("子代理更新了状态", "Updated subagent status"), target: Some(message_target), summary_kind: K::Agents, ..BASE }),
    ("structured_output", ToolViewConfig { family: F::AgentNote, icon: "MessageCircleMore", done: ("子代理交回了结构化结果", "Returned a structured result"), target: Some(|_, _| None), summary_kind: K::Agents, ..BASE }),
    ("subagent_activity", ToolViewConfig { family: F::AgentNote, icon: "Bot", done: ("记录了子代理活动", "Recorded subagent activity"), target: Some(message_target), summary_kind: K::Agents, ..BASE }),
    ("update", ToolViewConfig { family: F::AgentNote, icon: "MessageCircleMore", done: ("子代理更新了状态", "Updated subagent status"), target: Some(|item, _| compact(input_string(item, "message").or_else(|| input_string(item, "content")))), summary_kind: K::Agents, ..BASE }),

    ("ask_user", ToolViewConfig { surface: ToolSurface::Question, family: F::Question, icon: "CircleHelp", done: ("向用户提出了问题", "Asked the user a question"), target: Some(|item, _| compact(questions_from_input(item.input).first().map(|q| q.question.as_str()))), summary_kind: K::Other, ..BASE }),
    // A fork call only raises the request and returns; the receipt states that, so the row carries the prompt's first line.
    // An ordinary row: the request has no card of its own.
    ("fork", ToolViewConfig { icon: "GitFork", done: ("请求了分叉会话", "Requested a forked conversation"), running: Some(("正在请求分叉会话", "Requesting a forked conversation")), failed: Some(("请求分叉会话失败", "Failed to request a forked conversation")), target: Some(|item, _| compact(input_string(item, "prompt").and_then(|p| p.split('\n').next()))), keys: Some(&["prompt"]), summary_kind: K::Fork, ..BASE }),
    // The plan itself has a page of its own, so these rows only say what happened; the body is never repeated inline.
    ("plan", ToolViewConfig { icon: "NotebookPen", done: ("已更新计划", "Updated the plan"), resolve_title: Some(plan_title), keys: Some(&["action"]), summary_kind: K::Other, ..BASE }),
    ("exit_plan_mode", ToolViewConfig { icon: "ClipboardCheck", done: ("已提交计划", "Submitted the plan"), running: Some(("等待批准计划", "Waiting for plan approval")), failed: Some(("提交计划失败", "Failed to submit the plan")), keys: Some(&[]), summary_kind: K::Other, ..BASE }),
    // Handoff notes are the host's own notebook, not memory: the note travels in the call's input, so the row opens onto it.
    ("read_handoff_note", ToolViewConfig { icon: "NotebookText", done: ("读取了交接文档", "Read handoff note"), running: Some(("正在读取交接文档", "Reading handoff note")), failed: Some(("读取交接文档失败", "Failed to read handoff note")), target: Some(memory_target), keys: Some(&["name"]), summary_kind: K::Handoff, ..BASE }),
    ("create_handoff_note", ToolViewConfig { icon: "NotebookPen", done: ("写了交接文档", "Wrote handoff note"), running: Some(("正在写交接文档", "Writing handoff note")), failed: Some(("写交接文档失败", "Failed to write handoff note")), target: Some(memory_target), keys: Some(&["name", "description", "content"]), summary_kind: K::Handoff, ..BASE }),
    ("edit_handoff_note", ToolViewConfig { icon: "NotebookPen", done: ("修改了交接文档", "Edited handoff note"), running: Some(("正在修改交接文档", "Editing handoff note")), failed: Some(("修改交接文档失败", "Failed to edit handoff note")), target: Some(memory_target), keys: Some(&["name", "description", "old_text", "new_text"]), summary_kind: K::Handoff, ..BASE }),
    // The result names the conversation the work went on in.
    ("handoff", ToolViewConfig { icon: "Handshake", done: ("已交接到新会话", "Handed off to a new conversation"), running: Some(("正在交接", "Handing off")), failed: Some(("交接未完成", "Handoff not completed")), keys: Some(&[]), summary_kind: K::Handoff, ..BASE }),
    ("read_global_memory", ToolViewConfig { family: F::Memory, icon: "FileText", done: ("读取了全局记忆", "Read global memory"), running: Some(("正在读取全局记忆", "Reading global memory")), failed: Some(("读取全局记忆失败", "Failed to read global memory")), target: Some(memory_target), summary_kind: K::Memory, ..BASE }),
    ("read_project_memory", ToolViewConfig { family: F::Memory, icon: "FileText", done: ("读取了项目记忆", "Read project memory"), running: Some(("正在读取项目记忆", "Reading project memory")), failed: Some(("读取项目记忆失败", "Failed to read project memory")), target: Some(memory_target), summary_kind: K::Memory, ..BASE }),
    ("create_global_memory", ToolViewConfig { family: F::Memory, icon: "FilePenLine", done: ("创建了全局记忆", "Created global memory"), running: Some(("正在创建全局记忆", "Creating global memory")), failed: Some(("创建全局记忆失败", "Failed to create global memory")), target: Some(memory_target), summary_kind: K::Memory, ..BASE }),
    ("create_project_memory", ToolViewConfig { family: F::Memory, icon: "FilePenLine", done: ("创建了项目记忆", "Created project memory"), running: Some(("正在创建项目记忆", "Creating project memory")), failed: Some(("创建项目记忆失败", "Failed to create project memory")), target: Some(memory_target), summary_kind: K::Memory, ..BASE }),
    ("edit_global_memory", ToolViewConfig { family: F::Memory, icon: "FilePenLine", done: ("编辑了全局记忆", "Edited global memory"), running: Some(("正在编辑全局记忆", "Editing global memory")), failed: Some(("编辑全局记忆失败", "Failed to edit global memory")), target: Some(memory_target), summary_kind: K::Memory, ..BASE }),
    ("edit_project_memory", ToolViewConfig { family: F::Memory, icon: "FilePenLine", done: ("编辑了项目记忆", "Edited project memory"), running: Some(("正在编辑项目记忆", "Editing project memory")), failed: Some(("编辑项目记忆失败", "Failed to edit project memory")), target: Some(memory_target), summary_kind: K::Memory, ..BASE }),
];

fn registry_entry(name: &str) -> Option<&'static ToolViewConfig> {
    TOOL_VIEW_REGISTRY.iter().find(|(entry, _)| *entry == name).map(|(_, config)| config)
}

/// Whether the registry names this tool: what upstream's test asks of `TOOL_VIEW_REGISTRY`.
pub fn is_registered_tool(name: &str) -> bool {
    registry_entry(name).is_some()
}

/// Every name the registry holds, with its Lucide icon.
pub fn registered_tools() -> impl Iterator<Item = (&'static str, &'static str)> {
    TOOL_VIEW_REGISTRY.iter().map(|(name, config)| (*name, config.icon))
}

/// The timeline surface a tool renders on.
///
/// Only two tools leave the ordinary block: `ask_user`, whose history card carries the user's own answer, and `workflow`,
/// which draws a card of its own for the whole life of the run. Everything else, including every agent-protocol and task
/// state call, is an ordinary row inside the one tool block for its contiguous run.
pub fn tool_surface_for_name(name: &str) -> ToolSurface {
    registry_entry(name).map_or(ToolSurface::Group, |config| config.surface)
}

/// Whether the timeline hoists this call onto the end-of-stream waiting indicator instead of drawing a block for it.
///
/// An ordinary call in flight has nothing a block can show that the one-line activity does not: its result is empty until
/// the host reports it, so the card would be a heading and a spinner occupying a full row, then replaced the moment the real
/// receipt lands. The two non-ordinary surfaces keep their own live representation and hoist only before execution starts:
/// `workflow` draws a progress card for the whole run, and `ask_user` is answered in its dock.
pub fn is_hoisted_tool_call(item: &ToolItem<'_>) -> bool {
    if !item.is_live_call() {
        return false;
    }
    tool_surface_for_name(item.tool_name) == ToolSurface::Group || matches!(item.stream_status, Some(StreamingToolPhase::Announced | StreamingToolPhase::Ready))
}

pub fn execution_phase(item: &ToolItem<'_>) -> ToolPhase {
    if item.is_live_call() {
        return ToolPhase::Running;
    }
    if item.result.success { ToolPhase::Done } else { ToolPhase::Failed }
}

/// MCP tools are named by the host, never by this registry: `mcp__<server-slug>__<tool-slug>__<digest>`. The prefix is the
/// whole contract: matching it is what lets a server the user installed today render as an MCP call rather than as an
/// unrecognized one.
pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__")
}

fn mcp_label_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^MCP\s+(.+?)\s+/\s+(.+)$").expect("valid"))
}

/// `tool.replace(/\s*\([^)]*\)\s*$/, "")`: the last parenthetical, opened as early as it can be.
fn without_trailing_parenthetical(tool: &str) -> &str {
    let trimmed = tool.trim_end_matches(is_js_space);
    let Some(body) = trimmed.strip_suffix(')') else { return tool };
    let scope = body.rfind(')').map_or(0, |close| close + 1);
    match body[scope..].find('(') {
        Some(open) => body[..scope + open].trim_end_matches(is_js_space),
        None => tool,
    }
}

/// The server and tool an MCP call names, in the words the user chose: `(server, tool)`.
///
/// The descriptor's label carries both verbatim (`MCP <server> / <tool>`), so it is preferred; a trailing parenthetical is
/// the host's manual-confirmation notice and belongs in the approval dock, not in a timeline row. Without a descriptor (an
/// archived conversation whose server has since been removed) the wire name still yields readable slugs, and the server
/// slug's trailing digest is dropped because it identifies nothing the reader can act on.
pub fn mcp_tool_naming(item: &ToolItem<'_>, descriptor: Option<&ToolDescriptor>) -> (Option<String>, String) {
    if let Some(label) = descriptor.map(|d| js_trim(&d.label))
        && let Some(found) = mcp_label_pattern().captures(label)
    {
        return (Some(js_trim(&found[1]).to_owned()), js_trim(without_trailing_parenthetical(&found[2])).to_owned());
    }
    let parts: Vec<&str> = item.tool_name.split("__").collect();
    if parts.len() >= 3 {
        return (Some(without_digest(parts[1]).to_owned()), parts[2].to_owned());
    }
    (None, item.tool_name.to_owned())
}

/// `slug.replace(/_[0-9a-f]{10}$/, "")`.
fn without_digest(slug: &str) -> &str {
    if slug.len() > 11 && slug.is_char_boundary(slug.len() - 11) {
        let (head, tail) = slug.split_at(slug.len() - 11);
        if tail.starts_with('_') && tail[1..].bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return head;
        }
    }
    slug
}

/// The bucket this call is counted in by the block heading. MCP is decided by the wire name because those tools are
/// discovered at runtime and have no registry entry to carry a bucket.
pub fn tool_summary_kind(item: &ToolItem<'_>) -> SummaryKind {
    if is_mcp_tool_name(item.tool_name) {
        return SummaryKind::Mcp;
    }
    registry_entry(item.tool_name).map_or(SummaryKind::Other, |config| config.summary_kind)
}

/// What a block row calls the tool: its name, not a sentence about it.
///
/// The row is one line (icon, name, one line of the call itself), so the localized phrase ("Ran Bash command") would crowd
/// out the argument that says what actually happened. That phrase is not lost: it stays the row's accessible name and its
/// tooltip, and the block heading counts it.
pub fn tool_row_name(item: &ToolItem<'_>, descriptor: Option<&ToolDescriptor>) -> String {
    if is_mcp_tool_name(item.tool_name) {
        return mcp_tool_naming(item, descriptor).1;
    }
    item.tool_name.to_owned()
}

/// `explanation` is the local helper model's one-line description of a shell command; when present it replaces the whole
/// title, prefixed with the shell.
///
/// A failed call's title says why after what failed: `error_explanation`, the local helper model's reason, or until there is
/// one (or with that use off) the error's own line.
pub fn get_tool_presentation(item: &ToolItem<'_>, descriptor: Option<&ToolDescriptor>, explanation: Option<&str>, error_explanation: Option<&str>) -> ToolPresentation {
    get_tool_presentation_in(item, descriptor, Lang::current(), explanation, error_explanation)
}

/// The same in a language of the caller's choosing, upstream's `t` argument.
pub fn get_tool_presentation_in(item: &ToolItem<'_>, descriptor: Option<&ToolDescriptor>, lang: Lang, explanation: Option<&str>, error_explanation: Option<&str>) -> ToolPresentation {
    let phase = execution_phase(item);
    let reason: Option<String> = (phase == ToolPhase::Failed)
        .then(|| error_explanation.map(js_trim).filter(|reason| !reason.is_empty()).map(str::to_owned).or_else(|| error_excerpt(&item.result.output)))
        .flatten();
    let reason = reason.as_deref();
    let Some(config) = registry_entry(item.tool_name) else {
        return unregistered_presentation(item, descriptor, lang, phase, reason);
    };
    let explanation = explanation.map(js_trim).filter(|text| !text.is_empty());
    let explained = match (backend_of_tool(item.tool_name), explanation) {
        (Some(shell), Some(text)) if phase == ToolPhase::Failed => Some(lang.f("{shell}：{text}（失败）", "{shell}: {text} (failed)", &[("shell", &shell_backend_label(shell)), ("text", &text)])),
        (Some(shell), Some(text)) => Some(lang.f("{shell}：{text}", "{shell}: {text}", &[("shell", &shell_backend_label(shell)), ("text", &text)])),
        _ => None,
    };
    let named = if phase == ToolPhase::Done { config.resolve_subject.and_then(|resolve| resolve(item, lang)) } else { None };
    let phrase = config.resolve_title.and_then(|resolve| resolve(item, phase, lang)).unwrap_or_else(|| {
        let words = match phase {
            ToolPhase::Running => config.running,
            ToolPhase::Failed => config.failed,
            ToolPhase::Done => None,
        };
        let (zh, en) = words.unwrap_or(config.done);
        lang.t(zh, en).to_owned()
    });
    let title = if reason.is_some() {
        failure_title(&phrase, reason, lang)
    } else if let Some(explained) = &explained {
        explained.clone()
    } else if let Some(named) = &named {
        format!("{}{}{}", named.subject.before, named.subject.text, named.subject.after)
    } else {
        phrase
    };
    let target = config.target.and_then(|resolve| resolve(item, lang)).filter(|target| !target.is_empty());
    let stat = config.stat.and_then(|resolve| resolve(item, lang)).filter(|stat| !stat.is_empty());
    let (subject, counts, note) = match named {
        Some(named) => (if explained.is_none() { Some(named.subject) } else { None }, named.counts, named.note.filter(|note| !note.is_empty())),
        None => (None, None, None),
    };
    ToolPresentation {
        surface: config.surface,
        family: config.resolve_family.map_or(config.family, |resolve| resolve(item)),
        icon: config.icon,
        title,
        subject,
        counts,
        note,
        target_is_path: target.is_some() && config.target_is_path,
        target,
        stat,
        keys: config.keys,
    }
}

fn unregistered_presentation(item: &ToolItem<'_>, descriptor: Option<&ToolDescriptor>, lang: Lang, phase: ToolPhase, reason: Option<&str>) -> ToolPresentation {
    let duration = browser_duration(item);
    if is_mcp_tool_name(item.tool_name) {
        let (server, tool) = mcp_tool_naming(item, descriptor);
        let target = first_argument_summary(item).or(server).filter(|target| !target.is_empty());
        let title = match phase {
            ToolPhase::Running => lang.f("正在调用 MCP 工具 {label}", "Calling MCP tool {label}", &[("label", &tool)]),
            ToolPhase::Failed => failure_title(&lang.f("MCP 工具 {label} 调用失败", "MCP tool {label} failed", &[("label", &tool)]), reason, lang),
            ToolPhase::Done => lang.f("调用了 MCP 工具 {label}", "Called MCP tool {label}", &[("label", &tool)]),
        };
        return ToolPresentation { surface: ToolSurface::Group, family: F::Raw, icon: "Plug", title, subject: None, counts: None, note: None, target, target_is_path: false, stat: duration, keys: None };
    }
    let label = descriptor.map(|d| js_trim(&d.label)).filter(|label| !label.is_empty()).unwrap_or(item.tool_name);
    let title = match phase {
        ToolPhase::Running => lang.f("正在使用 {label}", "Using {label}", &[("label", &label)]),
        ToolPhase::Failed => failure_title(&lang.f("{label}执行失败", "{label} failed", &[("label", &label)]), reason, lang),
        ToolPhase::Done => lang.f("使用了 {label}", "Used {label}", &[("label", &label)]),
    };
    ToolPresentation { surface: ToolSurface::Group, family: F::Raw, icon: "Wrench", title, subject: None, counts: None, note: None, target: None, target_is_path: false, stat: duration, keys: None }
}

fn summary_phrase(kind: SummaryKind, count: usize, lang: Lang) -> String {
    let params: &[(&str, &dyn std::fmt::Display)] = &[("count", &count)];
    let one = count == 1;
    match kind {
        K::Commands if one => lang.t("运行了 1 个命令", "Ran 1 command").to_owned(),
        K::Commands => lang.f("运行了 {count} 个命令", "Ran {count} commands", params),
        K::FileChanges if one => lang.t("1 次文件操作", "1 file change").to_owned(),
        K::FileChanges => lang.f("{count} 次文件操作", "{count} file changes", params),
        K::Agents if one => lang.t("调用了 1 个子代理", "Called 1 subagent").to_owned(),
        K::Agents => lang.f("调用了 {count} 个子代理", "Called {count} subagents", params),
        K::Mcp if one => lang.t("调用了 1 个 MCP 工具", "Called 1 MCP tool").to_owned(),
        K::Mcp => lang.f("调用了 {count} 个 MCP 工具", "Called {count} MCP tools", params),
        K::Browser if one => lang.t("执行了 1 次浏览器操作", "Performed 1 browser action").to_owned(),
        K::Browser => lang.f("执行了 {count} 次浏览器操作", "Performed {count} browser actions", params),
        K::Device if one => lang.t("操作了 1 次设备", "Performed 1 device action").to_owned(),
        K::Device => lang.f("操作了 {count} 次设备", "Performed {count} device actions", params),
        K::Fork if one => lang.t("请求了 1 次会话分叉", "Requested 1 conversation fork").to_owned(),
        K::Fork => lang.f("请求了 {count} 次会话分叉", "Requested {count} conversation forks", params),
        K::Hook if one => lang.t("触发了 1 个 hook", "Triggered 1 hook").to_owned(),
        K::Hook => lang.f("触发了 {count} 个 hook", "Triggered {count} hooks", params),
        K::Skill if one => lang.t("读取了 1 个技能", "Loaded 1 skill").to_owned(),
        K::Skill => lang.f("读取了 {count} 个技能", "Loaded {count} skills", params),
        K::Memory if one => lang.t("访问了 1 次记忆", "Accessed memory once").to_owned(),
        K::Memory => lang.f("访问了 {count} 次记忆", "Accessed memory {count} times", params),
        K::Handoff if one => lang.t("1 次交接操作", "1 handoff action").to_owned(),
        K::Handoff => lang.f("{count} 次交接操作", "{count} handoff actions", params),
        K::Search if one => lang.t("搜索了 1 次内容", "Performed 1 content search").to_owned(),
        K::Search => lang.f("搜索了 {count} 次内容", "Performed {count} content searches", params),
        K::Reads if one => lang.t("读取了 1 个文件", "Read 1 file").to_owned(),
        K::Reads => lang.f("读取了 {count} 个文件", "Read {count} files", params),
        K::Files if one => lang.t("检查了 1 次文件与目录", "Checked files and directories once").to_owned(),
        K::Files => lang.f("检查了 {count} 次文件与目录", "Checked files and directories {count} times", params),
        K::Reasoning if one => lang.t("思考了 1 次", "Thought once").to_owned(),
        K::Reasoning => lang.f("思考了 {count} 次", "Thought {count} times", params),
        K::Other if one => lang.t("使用了 1 个工具", "Used 1 tool").to_owned(),
        K::Other => lang.f("使用了 {count} 个工具", "Used {count} tools", params),
    }
}

/// Buckets in the order the heading states them: most consequential first.
///
/// The heading is one line and ellipsizes, so this order decides what survives a narrow window. Work that changed something
/// outside the conversation leads; reads, which are the most numerous and the least consequential, trail. `Handoff` is not
/// stated, as upstream's order leaves it out too.
const SUMMARY_ORDER: [SummaryKind; 15] = [K::Commands, K::FileChanges, K::Agents, K::Mcp, K::Browser, K::Device, K::Fork, K::Hook, K::Skill, K::Memory, K::Search, K::Reads, K::Files, K::Reasoning, K::Other];

/// What one block did, as a sentence of counted clauses.
///
/// A block mixes reasoning, hooks and every kind of tool call, so no single clause can describe it; the heading names each
/// bucket it contains and lets `SUMMARY_ORDER` decide which ones stay visible when it truncates.
pub fn summarize_block_kinds(kinds: &[SummaryKind]) -> String {
    summarize_block_kinds_in(kinds, Lang::current())
}

pub fn summarize_block_kinds_in(kinds: &[SummaryKind], lang: Lang) -> String {
    let clauses: Vec<String> = SUMMARY_ORDER
        .iter()
        .filter_map(|kind| {
            let count = kinds.iter().filter(|k| *k == kind).count();
            (count > 0).then(|| summary_phrase(*kind, count, lang))
        })
        .collect();
    clauses.join(lang.t("，", ", "))
}

/// The line under a call in flight, here and on the waiting indicator.
pub fn running_label(item: &ToolItem<'_>) -> String {
    running_label_in(item, Lang::current())
}

pub fn running_label_in(item: &ToolItem<'_>, lang: Lang) -> String {
    match item.stream_status {
        Some(StreamingToolPhase::Announced) => lang.t("正在准备参数", "Preparing arguments"),
        Some(StreamingToolPhase::Ready) => lang.t("等待执行", "Waiting to run"),
        _ => lang.t("正在执行", "Running"),
    }
    .to_owned()
}

// ---------------------------------------------------------------- what a detail card holds

/// A tone of `.tool-renderer__lines`. Tone is the only mark: where a match was found steps back from what was found, and a
/// line the tool wrote about its own answer steps back further.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineTone {
    Plain,
    Dim,
    Note,
    Error,
    Warning,
}

/// Text in the code face with tone changes in it. Everything in a `.tool-renderer__lines` is one selection region upstream,
/// so it stays one text here, however many lines it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToneText {
    pub text: Arc<str>,
    /// Where each tone starts, in bytes, in order; the first starts at 0.
    pub tones: Vec<(u32, LineTone)>,
}

impl ToneText {
    pub fn plain(text: &str) -> Self {
        Self { text: Arc::from(text), tones: vec![(0, LineTone::Plain)] }
    }

    /// The runs of one tone, in order.
    pub fn runs(&self) -> impl Iterator<Item = (&str, LineTone)> {
        self.tones.iter().enumerate().map(|(i, (start, tone))| {
            let end = self.tones.get(i + 1).map_or(self.text.len(), |next| next.0 as usize);
            (&self.text[*start as usize..end], *tone)
        })
    }

    /// Line by line, each as its runs: what a test wants to read.
    pub fn lines(&self) -> Vec<Vec<(String, LineTone)>> {
        let mut lines: Vec<Vec<(String, LineTone)>> = vec![Vec::new()];
        for (run, tone) in self.runs() {
            let mut parts = run.split('\n');
            if let Some(first) = parts.next()
                && !first.is_empty()
            {
                lines.last_mut().expect("never empty").push((first.to_owned(), tone));
            }
            for part in parts {
                lines.push(if part.is_empty() { Vec::new() } else { vec![(part.to_owned(), tone)] });
            }
        }
        lines
    }

    /// Each line's whole text.
    pub fn line_texts(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }
}

#[derive(Default)]
struct ToneTextBuilder {
    text: String,
    tones: Vec<(u32, LineTone)>,
}

impl ToneTextBuilder {
    fn push(&mut self, text: &str, tone: LineTone) {
        if self.tones.last().is_none_or(|last| last.1 != tone) {
            self.tones.push((self.text.len() as u32, tone));
        }
        self.text.push_str(text);
    }

    /// One line, after whatever came before.
    fn line(&mut self, parts: &[(&str, LineTone)]) {
        if !self.text.is_empty() || !self.tones.is_empty() {
            self.text.push('\n');
        }
        for (text, tone) in parts {
            self.push(text, *tone);
        }
    }

    fn finish(self) -> ToneText {
        ToneText { text: Arc::from(self.text), tones: self.tones }
    }
}

/// The mark ahead of a listed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryIcon {
    /// A line that is the tool's own remark has none.
    None,
    /// A path the tool closed with a slash.
    Folder,
    /// A path's type, chosen from this name.
    File(String),
}

/// `/ \([^()]*\)$/` off the end: what the tool sets after an entry's path to say something of it, `target/ (ignored)`.
fn without_entry_mark(path: &str) -> &str {
    if let Some(body) = path.strip_suffix(')')
        && let Some(open) = body.rfind('(')
        && !body[open + 1..].contains(')')
        && let Some(kept) = body[..open].strip_suffix(' ')
    {
        return kept;
    }
    path
}

/// How a line opens when it is the tool's remark on the list and not an entry of it.
fn is_entry_remark(path: &str) -> bool {
    path.starts_with('(') || path.starts_with("… ") || path.starts_with("[skipped] ")
}

/// The icon ahead of a listed path: its type's, or a folder for a path the tool closed with a slash.
pub fn entry_icon(line: &str) -> EntryIcon {
    let path = without_entry_mark(js_trim(line));
    if path.is_empty() || is_entry_remark(path) {
        EntryIcon::None
    } else if path.ends_with('/') {
        EntryIcon::Folder
    } else {
        EntryIcon::File(path.to_owned())
    }
}

/// A line the tool wrote about its answer (what it left out, how much of it it showed) rather than one of the entries:
/// `… 12 more`, `(Showing 6 of 14)`.
fn is_note_line(line: &str) -> bool {
    let text = js_trim(line);
    text.starts_with("… ") || (text.len() >= 2 && text.starts_with('(') && text.ends_with(')') && !has_line_terminator(text))
}

/// One listed path, or a remark among them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListEntry {
    pub icon: EntryIcon,
    pub text: Arc<str>,
    pub note: bool,
}

/// One line of a `read`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadLine {
    /// The source line it is, counted from where the read started. A note counts too and hides its number.
    pub number: u64,
    pub text: String,
    /// The tool talking about the text rather than the text.
    pub note: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvCard {
    pub rows: Vec<(String, Arc<str>)>,
    pub result: Option<Arc<str>>,
}

impl KvCard {
    /// The value a row of this label holds. Upstream's tests read a card back this way.
    pub fn get(&self, label: &str) -> Option<&str> {
        self.rows.iter().find(|(key, _)| key == label).map(|(_, value)| &**value)
    }

    pub fn labels(&self) -> Vec<&str> {
        self.rows.iter().map(|(key, _)| key.as_str()).collect()
    }
}

/// One result a catalog provider returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebResultView {
    pub title: String,
    pub content: Option<Arc<str>>,
}

/// What a detail card holds, one per family's way of drawing.
#[derive(Clone, Debug, PartialEq)]
pub enum DetailBody {
    /// `RunningView`.
    Running(String),
    /// A result carrying images is the image: its receipt text and screenshot metadata only repeat the thumbnail.
    ImageOnly,
    /// `EmptyResult`.
    Empty(String),
    /// `FileListView`, and a `GrepView` of files: every entry can carry an icon.
    Entries { label: String, entries: Vec<ListEntry> },
    /// Lines of text in tones: `GrepView`'s lines, the console and network ledgers, a snapshot, a response body. `list` says each line
    /// is an `<li>` upstream, which what wraps hangs under.
    Lines { label: Option<String>, list: bool, text: ToneText },
    /// `ReadView`.
    Read { label: String, path: Option<String>, lines: Vec<ReadLine> },
    /// `DiffView` with a diff.
    Diff { value: String, path: Option<String> },
    /// `TerminalView`.
    Terminal { command: Arc<str>, output: Arc<str> },
    /// `KeyValueCard`.
    Kv(KvCard),
    /// `AgentWaitView`.
    Envelopes { notice: Option<String>, envelopes: Vec<WaitEnvelopeView>, status_line: Option<String> },
    /// `WebSearchView`.
    WebSearch { label: String, chips: Vec<WebSourceChip>, findings: Option<Arc<str>>, results: Vec<WebResultView> },
}

/// `ToolDetailRenderer`'s reading of a call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDetailModel {
    pub phase: ToolPhase,
    pub family: ToolViewFamily,
    /// The card's accessible name.
    pub label: String,
    pub body: DetailBody,
}

impl ToolDetailModel {
    pub fn kv(&self) -> Option<&KvCard> {
        match &self.body {
            DetailBody::Kv(card) => Some(card),
            _ => None,
        }
    }
}

fn empty_result(lang: Lang) -> DetailBody {
    DetailBody::Empty(lang.t("没有可显示的结果", "No results to display").to_owned())
}

/// `JSON.stringify(value, null, 2)`.
fn safe_stringify(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// A value as a card line shows it.
fn scalar_text(value: Option<&Value>, lang: Lang) -> String {
    match value {
        // `JSON.stringify(undefined)` is itself undefined, and `String(undefined)` what upstream falls to.
        None => "undefined".to_owned(),
        Some(Value::Bool(b)) => lang.t(if *b { "是" } else { "否" }, if *b { "Yes" } else { "No" }).to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.as_f64().map_or_else(|| n.to_string(), js_number),
        Some(Value::Null) => "null".to_owned(),
        Some(other) => safe_stringify(other),
    }
}

/// An argument as the editor would show it: strings verbatim, anything else as JSON.
fn raw_field_text(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.as_f64().map_or_else(|| n.to_string(), js_number),
        Some(other) => safe_stringify(other),
    }
}

/// `KeyValueCard`'s rule for a line: nothing to say, no line.
fn kv_card(rows: Vec<(String, Option<String>)>, result: Option<&str>, lang: Lang) -> DetailBody {
    let rows: Vec<(String, Arc<str>)> = rows.into_iter().filter_map(|(label, value)| value.filter(|v| !v.is_empty()).map(|v| (label, Arc::from(v)))).collect();
    let result = result.filter(|r| !r.is_empty()).map(Arc::from);
    if rows.is_empty() && result.is_none() {
        return empty_result(lang);
    }
    DetailBody::Kv(KvCard { rows, result })
}

fn kv_row(label: &str, value: Option<&str>) -> (String, Option<String>) {
    (label.to_owned(), value.map(str::to_owned))
}

fn file_list_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let lines = output_lines(&item.result.output);
    let lines: Vec<&str> = lines.iter().map(|line| &**line).filter(|line| !js_trim(line).is_empty()).collect();
    if lines.is_empty() {
        return DetailBody::Empty(lang.t("没有文件", "No files").to_owned());
    }
    if lines.len() == 1 && FILE_LIST_EMPTY_ANSWERS.contains(&js_trim(lines[0])) {
        return DetailBody::Empty(js_trim(lines[0]).to_owned());
    }
    let entries = lines
        .iter()
        .map(|line| {
            let value = js_trim_end(line);
            ListEntry { icon: entry_icon(value), text: Arc::from(value), note: is_note_line(value) }
        })
        .collect();
    DetailBody::Entries { label: lang.t("文件结果", "File results").to_owned(), entries }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrepMode {
    FilesWithMatches,
    Content,
    Count,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrepRowKind {
    File,
    Separator,
    Note,
    Match,
    Context,
    Count,
}

/// One line of what `grep` printed, read by the mode the call asked for. `location()` is where a line was found (`path:12:`,
/// `path-13-` for context, `path:`), set back from `content()`, what was found there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepRow {
    pub kind: GrepRowKind,
    pub raw: String,
    /// Where the location ends in `raw`.
    cut: usize,
}

impl GrepRow {
    fn plain(kind: GrepRowKind, raw: &str) -> Self {
        Self { kind, raw: raw.to_owned(), cut: 0 }
    }

    pub fn location(&self) -> &str {
        &self.raw[..self.cut]
    }

    pub fn content(&self) -> &str {
        &self.raw[self.cut..]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepOutput {
    pub mode: GrepMode,
    pub rows: Vec<GrepRow>,
    pub count: usize,
    pub more: bool,
}

/// The cut of `^(.+?)<sep>(\d+)<sep>(.*)$`: where the path ends and where the text starts. It falls at the leftmost
/// `<sep>digits<sep>` with something before it; the text may hold more of them.
fn numbered_line(line: &str, separator: char) -> Option<(usize, usize)> {
    if has_line_terminator(line) {
        return None;
    }
    let mut from = line.chars().next()?.len_utf8();
    while let Some(found) = line[from..].find(separator) {
        let at = from + found;
        let rest = &line[at + 1..];
        let n = digits_len(rest);
        if n > 0 && rest[n..].starts_with(separator) {
            return Some((at, at + 1 + n + 1));
        }
        from = at + 1;
    }
    None
}

/// `^(.+):(\d+)$`, cut after the colon: the path is everything before the last one.
fn count_line(line: &str) -> Option<usize> {
    if has_line_terminator(line) {
        return None;
    }
    let colon = line.rfind(':')?;
    (colon > 0 && colon + 1 < line.len() && line[colon + 1..].bytes().all(|b| b.is_ascii_digit())).then_some(colon + 1)
}

/// The mode a call asked for. One that named none, or a name this card does not know, got the tool's default: the mode alone
/// says how a line reads, because a path may itself hold `:12:` or `-3-`.
fn grep_mode(item: &ToolItem<'_>) -> GrepMode {
    let requested = input_string(item, "output_mode");
    match requested {
        Some("content") => return GrepMode::Content,
        Some("count") => return GrepMode::Count,
        _ => {}
    }
    // A search recorded before `output_mode` existed names no mode and was answered in the old `path:line:text` lines. Only an
    // answer in which every single entry reads that way is taken for one, and only when no mode was named: a list of files
    // whose every path holds `:12:` is not a thing, whereas the old transcripts are.
    if requested.is_none() {
        let lines = output_lines(&item.result.output);
        let mut entries = lines.iter().filter(|line| !js_trim(line).is_empty() && !is_grep_note(line)).peekable();
        if entries.peek().is_some() && entries.all(|line| numbered_line(line, ':').is_some()) {
            return GrepMode::Content;
        }
    }
    GrepMode::FilesWithMatches
}

/// A line the tool wrote about its answer rather than an entry of it: how much is left (`… more matches follow`), what it
/// could not read, or that there was nothing to give.
fn is_grep_note(line: &str) -> bool {
    line.starts_with("… ") || line.starts_with("[skipped] ") || is_grep_empty_answer(line)
}

fn is_grep_empty_answer(line: &str) -> bool {
    let text = js_trim(line);
    GREP_EMPTY_ANSWERS.contains(&text) || is_grep_past_end(text)
}

/// The line as `path<separator>12<separator>text` for a path already known.
fn row_of_path(raw: &str, path: &str, separator: char) -> Option<GrepRow> {
    let rest = raw.strip_prefix(path)?.strip_prefix(separator)?;
    let n = digits_len(rest);
    if n == 0 || !rest[n..].starts_with(separator) {
        return None;
    }
    let kind = if separator == ':' { GrepRowKind::Match } else { GrepRowKind::Context };
    Some(GrepRow { kind, raw: raw.to_owned(), cut: path.len() + n + 2 })
}

/// One group of content lines: the run between two `--` lines, which is one file's.
///
/// With context on, a matching line and a context line differ only by the character after the path, and a path or a line's
/// own text may hold `:12:` or `-3-` too. The group's file settles it: a matching line cuts at its first `:digits:`, and a
/// context line's text can only make that cut later, never earlier, so the shortest such path in the group is the file.
/// Lines are then read against it.
fn grep_group_rows(group: &[&str], with_context: bool, rows: &mut Vec<GrepRow>) {
    let mut path: Option<&str> = None;
    if with_context {
        for raw in group {
            if is_grep_note(raw) {
                continue;
            }
            if let Some((end, _)) = numbered_line(raw, ':') {
                let found = &raw[..end];
                if path.is_none_or(|known| js_len(found) < js_len(known)) {
                    path = Some(found);
                }
            }
        }
    }
    rows.extend(group.iter().map(|raw| {
        if is_grep_note(raw) {
            return GrepRow::plain(GrepRowKind::Note, raw);
        }
        path.and_then(|path| row_of_path(raw, path, ':').or_else(|| row_of_path(raw, path, '-')))
            .or_else(|| numbered_line(raw, ':').map(|(_, cut)| GrepRow { kind: GrepRowKind::Match, raw: (*raw).to_owned(), cut }))
            .or_else(|| if with_context { numbered_line(raw, '-').map(|(_, cut)| GrepRow { kind: GrepRowKind::Context, raw: (*raw).to_owned(), cut }) } else { None })
            .unwrap_or_else(|| GrepRow::plain(GrepRowKind::Note, raw))
    }));
}

fn grep_content_rows(lines: &[&str], with_context: bool) -> Vec<GrepRow> {
    let mut rows = Vec::with_capacity(lines.len());
    for (index, group) in lines.split(|line| *line == "--").enumerate() {
        if index > 0 {
            rows.push(GrepRow::plain(GrepRowKind::Separator, "--"));
        }
        grep_group_rows(group, with_context, &mut rows);
    }
    rows
}

/// What `grep` printed, row by row, and how many entries of the mode's own unit it holds: files, matching lines (never the
/// context around them) or files with their counts. `more` says the tool left entries for a later page.
pub fn parse_grep_output(item: &ToolItem<'_>) -> GrepOutput {
    let mode = grep_mode(item);
    let lines = output_lines(&item.result.output);
    let lines: Vec<&str> = lines.iter().map(|line| &**line).filter(|line| !js_trim(line).is_empty()).collect();
    let rows: Vec<GrepRow> = match mode {
        GrepMode::Content => {
            // Context was asked for, or the tool's own `--` between groups says it was given.
            let with_context = ["context", "before", "after"].iter().any(|name| input_number(item, name).unwrap_or(0.0) > 0.0) || lines.contains(&"--");
            grep_content_rows(&lines, with_context)
        }
        _ => lines
            .iter()
            .map(|raw| {
                if is_grep_note(raw) {
                    return GrepRow::plain(GrepRowKind::Note, raw);
                }
                if mode == GrepMode::FilesWithMatches {
                    return GrepRow::plain(GrepRowKind::File, raw);
                }
                match if raw.starts_with("Total: ") { None } else { count_line(raw) } {
                    Some(cut) => GrepRow { kind: GrepRowKind::Count, raw: (*raw).to_owned(), cut },
                    None => GrepRow::plain(GrepRowKind::Note, raw),
                }
            })
            .collect(),
    };
    GrepOutput {
        mode,
        count: rows.iter().filter(|row| matches!(row.kind, GrepRowKind::File | GrepRowKind::Match | GrepRowKind::Count)).count(),
        more: rows.iter().any(|row| row.kind == GrepRowKind::Note && row.raw.starts_with("… ")),
        rows,
    }
}

/// `grep` as the tool printed it, a line to an entry: the files that matched, or the matching lines with where each was found
/// set back from what was found (and the lines around them, further back still), or a count per file.
fn grep_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let parsed = parse_grep_output(item);
    let rows = &parsed.rows;
    if rows.is_empty() {
        return DetailBody::Empty(lang.t("未找到匹配内容", "No matching content found").to_owned());
    }
    if rows.len() == 1 && is_grep_empty_answer(&rows[0].raw) {
        return DetailBody::Empty(js_trim(&rows[0].raw).to_owned());
    }
    let label = lang.t("搜索结果", "Search results").to_owned();
    if parsed.mode == GrepMode::FilesWithMatches {
        let entries = rows
            .iter()
            .map(|row| ListEntry {
                icon: if row.kind == GrepRowKind::File { entry_icon(&row.raw) } else { EntryIcon::None },
                text: Arc::from(row.raw.as_str()),
                note: row.kind != GrepRowKind::File,
            })
            .collect();
        return DetailBody::Entries { label, entries };
    }
    let mut text = ToneTextBuilder::default();
    for row in rows {
        match row.kind {
            GrepRowKind::Match | GrepRowKind::Count => text.line(&[(row.location(), LineTone::Dim), (row.content(), LineTone::Plain)]),
            GrepRowKind::Context => text.line(&[(row.location(), LineTone::Note), (row.content(), LineTone::Dim)]),
            _ => text.line(&[(&row.raw, LineTone::Note)]),
        }
    }
    DetailBody::Lines { label: Some(label), list: true, text: text.finish() }
}

fn read_body(item: &ToolItem<'_>, lang: Lang) -> Option<DetailBody> {
    let output = item.result.output.as_str();
    let start = input_number(item, "start_line").unwrap_or(1.0).floor().max(1.0) as u64;
    let path = input_string(item, "path");
    let lines = output_lines(output);
    if (lines.is_empty() || output.is_empty()) && item.result.images.is_empty() {
        return Some(DetailBody::Empty(lang.t("文件内容为空", "File is empty").to_owned()));
    }
    if output.is_empty() {
        return None;
    }
    // An empty file and a start past the end are answered with a sentence, which has no line to number.
    let sentence = if lines.len() == 1 { js_trim(&lines[0]) } else { "" };
    if sentence == READ_EMPTY_FILE {
        return Some(DetailBody::Empty(lang.t("文件内容为空", "File is empty").to_owned()));
    }
    if is_read_past_end(sentence) {
        return Some(DetailBody::Empty(sentence.to_owned()));
    }
    let label = lang.f("{path} 内容", "{path} contents", &[("path", &path.unwrap_or(lang.t("文件", "File")))]);
    let lines = lines.iter().enumerate().map(|(index, line)| ReadLine { number: start + index as u64, text: (**line).to_owned(), note: is_read_note(line) }).collect();
    Some(DetailBody::Read { label, path: path.map(str::to_owned), lines })
}

fn diff_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    if let Some(diff) = diff_of(item) {
        return DetailBody::Diff { value: diff.to_owned(), path: input_string(item, "path").map(str::to_owned) };
    }
    let output = item.result.output.as_str();
    let text = if output.is_empty() { lang.t("操作成功，没有行级变化", "Operation succeeded with no line-level changes") } else { output };
    DetailBody::Lines { label: None, list: false, text: ToneText::plain(text) }
}

fn terminal_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let output = item.result.output.as_str();
    DetailBody::Terminal {
        command: Arc::from(input_string(item, "command").unwrap_or("")),
        output: Arc::from(if output.is_empty() { lang.t("命令没有输出", "Command produced no output") } else { output }),
    }
}

fn json_fallback(item: &ToolItem<'_>, parsed: &ParsedJson, lang: Lang) -> DetailBody {
    let output = item.result.output.as_str();
    let note = if parsed.parsed { lang.t("结果结构无法识别", "Unrecognized result structure") } else { lang.t("结果不是有效 JSON", "Result is not valid JSON") };
    let mut text = ToneTextBuilder::default();
    text.line(&[(note, LineTone::Note)]);
    text.line(&[(if output.is_empty() { lang.t("没有返回内容", "No content returned") } else { output }, LineTone::Plain)]);
    DetailBody::Lines { label: None, list: false, text: text.finish() }
}

/// `preview_snapshot`. The answer is the accessibility tree the tool already formatted, plain text and not JSON, so the card
/// shows it verbatim rather than rebuilding a structure it does not have.
fn browser_snapshot_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let text = js_trim(&item.result.output);
    if text.is_empty() {
        return DetailBody::Empty(lang.t("快照中没有可读内容", "The snapshot has no readable content").to_owned());
    }
    DetailBody::Lines { label: Some(lang.t("页面快照", "Page snapshot").to_owned()), list: false, text: ToneText::plain(text) }
}

/// `preview_eval`. The value is already JSON the tool serialized, or the literal `undefined`.
fn browser_evaluate_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let output = item.result.output.as_str();
    kv_card(vec![kv_row("JavaScript", input_string(item, "expression"))], Some(if output.is_empty() { "undefined" } else { output }), lang)
}

fn browser_screenshot_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let parsed = parse_json_output(item);
    let Some(value) = parsed.value.as_object().filter(|_| parsed.parsed) else { return json_fallback(item, &parsed, lang) };
    let size = match (value.get("width"), value.get("height")) {
        (Some(width), Some(height)) => Some(format!("{}×{}", scalar_text(Some(width), lang), scalar_text(Some(height), lang))),
        _ => None,
    };
    kv_card(vec![(lang.t("尺寸", "Dimensions").to_owned(), size)], None, lang)
}

/// `/^\[([a-z]+)\]/i`, lower-cased.
fn console_level(line: &str) -> Option<String> {
    let rest = line.strip_prefix('[')?;
    let n = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
    (n > 0 && rest[n..].starts_with(']')).then(|| rest[..n].to_ascii_lowercase())
}

/// `preview_console_logs`, as the tool printed it: one `[level] text` to a line.
fn browser_console_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let output = js_trim(&item.result.output);
    // The tool's own English answer, copied from Claude Code.
    if output.is_empty() || output == "No console logs." {
        return DetailBody::Empty(lang.t("没有 Console 日志", "No Console logs").to_owned());
    }
    let mut text = ToneTextBuilder::default();
    for line in output_lines(output).iter().filter(|line| !js_trim(line).is_empty()) {
        let tone = if is_note_line(line) {
            LineTone::Note
        } else {
            match console_level(line).as_deref() {
                Some("error") => LineTone::Error,
                Some("warn" | "warning") => LineTone::Warning,
                _ => LineTone::Plain,
            }
        };
        text.line(&[(line, tone)]);
    }
    DetailBody::Lines { label: Some(lang.t("Console 日志", "Console logs").to_owned()), list: true, text: text.finish() }
}

fn network_entry() -> &'static Regex {
    // Keeps its backtracking: the URL gives way to a `[FAILED:` set straight after it, which a hand-written reader would have
    // to rediscover. `\d` is spelled out because here it would take any Unicode digit.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[([^\]]+)]\s+([A-Z]+)\s+(\S+)(?:\s+→\s+([0-9]+)\s*(.*?))?\s*(\[FAILED:[\s\S]*])?$").expect("valid"))
}

/// `preview_network`. Without `requestId` the answer is the request ledger, one line each, and stays as the tool printed it:
/// the id a line leads with is what a later call asks for a body by. With one it is that body, which is text.
fn browser_network_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let output = js_trim(&item.result.output);
    if input_string(item, "requestId").is_some() {
        if output.is_empty() {
            return DetailBody::Empty(lang.t("没有响应正文", "No response body").to_owned());
        }
        return DetailBody::Lines { label: None, list: false, text: ToneText::plain(output) };
    }
    let lines = output_lines(output);
    let lines: Vec<&str> = lines.iter().map(|line| &**line).filter(|line| !js_trim(line).is_empty()).collect();
    if !lines.iter().any(|line| network_entry().is_match(line)) {
        return DetailBody::Empty(lines.first().map_or_else(|| lang.t("没有网络记录", "No network records").to_owned(), |line| (*line).to_owned()));
    }
    let mut text = ToneTextBuilder::default();
    for line in lines {
        let failed = network_entry().captures(line).is_some_and(|found| found.get(6).is_some_and(|m| !m.as_str().is_empty()));
        let tone = if failed {
            LineTone::Error
        } else if is_note_line(line) {
            LineTone::Note
        } else {
            LineTone::Plain
        };
        text.line(&[(line, tone)]);
    }
    DetailBody::Lines { label: Some(lang.t("网络请求", "Network requests").to_owned()), list: true, text: text.finish() }
}

/// A call shown as the arguments worth reading and then what it answered.
///
/// `presentation.keys` names those arguments and their order. A tool that names none has had none curated, so all of them
/// show rather than none: an uncurated argument is still the reader's only account of the call.
fn raw_rows(item: &ToolItem<'_>, presentation: &ToolPresentation) -> Vec<(String, Option<String>)> {
    match presentation.keys {
        Some(names) => names.iter().map(|name| ((*name).to_owned(), Some(raw_field_text(item.input.get(*name))))).collect(),
        None => item.input.iter().map(|(name, value)| (name.clone(), Some(raw_field_text(Some(value))))).collect(),
    }
}

fn raw_body(item: &ToolItem<'_>, presentation: &ToolPresentation, lang: Lang) -> DetailBody {
    kv_card(raw_rows(item, presentation), Some(&item.result.output), lang)
}

/// A preview tool whose whole answer is one JSON document: `preview_list`'s server array, `preview_inspect`'s element. The host
/// serializes those compactly, so the card re-indents rather than printing one very long line.
fn browser_json_body(item: &ToolItem<'_>, presentation: &ToolPresentation, lang: Lang) -> DetailBody {
    let parsed = parse_json_output(item);
    if !parsed.parsed {
        return raw_body(item, presentation, lang);
    }
    kv_card(raw_rows(item, presentation), Some(&safe_stringify(&parsed.value)), lang)
}

/// Mewrk's own two preview tools answer with a JSON object that carries the page state after the action: where the page ended
/// up, anything that blocked it, the console errors the action produced, and a bounded accessibility tree. Every other key
/// belongs to the tool itself and is shown above them.
const PAGE_REPORT_KEYS: [&str; 5] = ["page", "snapshot", "notices", "newConsoleErrors", "modalState"];

fn browser_page_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let parsed = parse_json_output(item);
    let Some(value) = parsed.value.as_object().filter(|_| parsed.parsed) else { return json_fallback(item, &parsed, lang) };
    let page = object(value.get("page"));
    let snapshot = object(value.get("snapshot"));
    let modal = object(value.get("modalState"));
    let joined = |name: &str, separator: &str| value.get(name).and_then(Value::as_array).map_or(String::new(), |entries| entries.iter().map(|entry| scalar_text(Some(entry), lang)).collect::<Vec<_>>().join(separator));
    let modal_description = modal.map(|modal| match modal.get("description") {
        Some(Value::Array(entries)) => entries.iter().map(|entry| scalar_text(Some(entry), lang)).collect::<Vec<_>>().join("; "),
        description => scalar_text(description, lang),
    });
    let page_field = |name: &str| page.and_then(|page| page.get(name)).map(|field| scalar_text(Some(field), lang));
    let mut rows: Vec<(String, Option<String>)> = value
        .iter()
        .filter(|(key, entry)| !PAGE_REPORT_KEYS.contains(&key.as_str()) && !entry.is_object() && !entry.is_array())
        .map(|(key, entry)| (key.clone(), Some(scalar_text(Some(entry), lang))))
        .collect();
    rows.push(("URL".to_owned(), page_field("url")));
    rows.push((lang.t("标题", "Title").to_owned(), page_field("title")));
    rows.push((lang.t("加载中", "Loading").to_owned(), page_field("loading")));
    rows.push((lang.t("模态状态", "Modal state").to_owned(), modal_description));
    rows.push((lang.t("提示", "Notices").to_owned(), Some(joined("notices", "\n"))));
    rows.push((lang.t("新增 Console 错误", "New Console errors").to_owned(), Some(joined("newConsoleErrors", "\n"))));
    kv_card(rows, snapshot.and_then(|s| s.get("tree")).and_then(Value::as_str), lang)
}

fn memory_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let tier = if item.tool_name.contains("_global_") { lang.t("全局记忆", "Global memory") } else { lang.t("项目记忆", "Project memory") };
    // A memory tool is an ordinary tool: what it wrote is on the card like any other call's arguments, the whole body of a new
    // document, or the passage an edit replaced and what replaced it.
    let rows = vec![
        kv_row(lang.t("范围", "Tier"), Some(tier)),
        (lang.t("文档", "Document").to_owned(), compact_to(input_string(item, "name"), 160)),
        (lang.t("索引描述", "Index description").to_owned(), compact_to(input_string(item, "description"), 300)),
        kv_row(lang.t("内容", "Content"), input_string(item, "content")),
        kv_row(lang.t("原文", "Old text"), input_string(item, "old_text")),
        kv_row(lang.t("新文", "New text"), input_string(item, "new_text")),
    ];
    kv_card(rows, Some(&item.result.output), lang)
}

fn run_status_label(status: SubagentViewStatus, lang: Lang) -> &'static str {
    match status {
        SubagentViewStatus::Running => lang.t("运行中", "Running"),
        SubagentViewStatus::Completed => lang.t("已完成", "Completed"),
        SubagentViewStatus::Failed => lang.t("已失败", "Failed"),
        SubagentViewStatus::Stopped => lang.t("已停止", "Stopped"),
        SubagentViewStatus::RoundLimit => lang.t("已达轮次上限", "Round limit reached"),
        SubagentViewStatus::Interrupted => lang.t("已中断", "Interrupted"),
    }
}

/// The updates a child pushed onto this call while it ran.
///
/// `live.updates` is the streaming feed and `subagent.updates` the persisted record; a call in the handoff window carries both,
/// with the same content in each, so identical text collapses to its latest occurrence.
fn run_updates<'a>(item: &ToolItem<'a>) -> Vec<&'a SubagentUpdate> {
    let live = item.live.map(|live| live.updates.as_slice()).unwrap_or_default();
    let record = item.subagent.map(|run| run.updates.as_slice()).unwrap_or_default();
    let mut latest: Vec<&SubagentUpdate> = Vec::new();
    let mut slots: HashMap<String, usize> = HashMap::new();
    for update in live.iter().chain(record) {
        let key = collapse_spaces(&update.content);
        if key.is_empty() {
            continue;
        }
        match slots.get(&key) {
            None => {
                slots.insert(key, latest.len());
                latest.push(update);
            }
            Some(&slot) => {
                // A date that does not parse never wins, as `NaN <= x` is false.
                if let (Some(previous), Some(next)) = (parse_iso(&latest[slot].created_at), parse_iso(&update.created_at))
                    && previous <= next
                {
                    latest[slot] = update;
                }
            }
        }
    }
    latest.sort_by_key(|update| parse_iso(&update.created_at).unwrap_or(0));
    latest
}

/// A call that owns a child run: what it was asked, what it said, what it returned.
fn agent_run_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let (to, status, initial) = with_task_ref(item, |r| (send_message_recipient(r), agent_timeline_run_status(r), initial_message(r)));
    let to = (!to.is_empty()).then_some(to);
    // A `send_message` that resumed a child owns the run, but what it was asked is the message it sent, not an instruction that
    // started the child.
    let instruction = if !initial.is_empty() { initial } else { item.subagent.map_or(String::new(), |run| run.task.clone()) };
    let run_name = item.subagent.and_then(|run| run.label.as_deref().filter(|l| !l.is_empty()).or(run.name.as_deref().filter(|n| !n.is_empty())));
    let updates: Vec<&str> = run_updates(item).iter().map(|update| update.content.as_str()).collect();
    let rows = vec![
        (lang.t("子代理", "Subagent").to_owned(), run_name.map(str::to_owned).or_else(|| to.clone())),
        kv_row(lang.t("状态", "Status"), Some(run_status_label(status, lang))),
        (if to.is_some() { lang.t("消息", "Message") } else { lang.t("派发指令", "Instruction") }.to_owned(), Some(instruction)),
        kv_row(lang.t("摘要", "Summary"), if to.is_some() { input_string(item, "summary") } else { None }),
        (lang.t("子代理更新", "Subagent updates").to_owned(), Some(updates.join("\n"))),
    ];
    kv_card(rows, Some(&item.result.output), lang)
}

/// `task_wait` output split back into the `[name · status]` envelopes it drained: who answered and how it stands on a line of
/// its own, over what it said. A name is as long as its author made it, so it is not a column.
fn agent_wait_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    let parsed = parse_wait_output(&item.result.output);
    if parsed.envelopes.is_empty() {
        return if parsed.notice.is_empty() {
            DetailBody::Empty(lang.t("等待已结束，没有可显示的更新", "The wait finished with no updates to show").to_owned())
        } else {
            kv_card(Vec::new(), Some(&item.result.output), lang)
        };
    }
    DetailBody::Envelopes {
        notice: (!parsed.notice.is_empty()).then_some(parsed.notice),
        envelopes: parsed.envelopes,
        status_line: (!parsed.status_line.is_empty()).then_some(parsed.status_line),
    }
}

/// A one-shot agent message (`subagent_update`, `structured_output`, `task_list`, ...). Its payload is already the row's target,
/// so the detail only has to show the full untruncated text and the receipt the host wrote back.
fn agent_note_body(item: &ToolItem<'_>, lang: Lang) -> DetailBody {
    // The main agent's `send_message` that only queued a message: who it went to and, when the model wrote one, the preview
    // line, over the whole message and the host's receipt.
    let to = with_task_ref(item, |r| send_message_recipient(r));
    if !to.is_empty() {
        let rows = vec![
            kv_row(lang.t("发给", "To"), Some(&to)),
            kv_row(lang.t("摘要", "Summary"), input_string(item, "summary")),
            kv_row(lang.t("内容", "Message"), input_string(item, "message")),
        ];
        return kv_card(rows, Some(&item.result.output), lang);
    }
    let message = input_string(item, "message").or_else(|| input_string(item, "content")).or_else(|| input_string(item, "status"));
    kv_card(vec![kv_row(lang.t("内容", "Message"), message)], Some(&item.result.output), lang)
}

/// A settled `web_search` card: the sites it consulted, then what it found.
///
/// The chips come first because they are the part a reader can check. The prose under them is a model's summary for a native
/// backend and the providers' own snippets for a catalog backend; either way the domains say where it came from, which is the
/// question a search result actually has to answer.
fn web_search_body(item: &ToolItem<'_>, presentation: &ToolPresentation, lang: Lang) -> DetailBody {
    let parsed = parse_json_output(item);
    let chips = web_source_chips_of(&parsed.value);
    let envelope = parsed.value.as_object();
    let findings = trimmed_field(envelope, "findings");
    let results = envelope.and_then(|e| e.get("results")).and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    if chips.is_empty() && findings.is_empty() && results.is_empty() {
        return raw_body(item, presentation, lang);
    }
    let results = results
        .iter()
        .map(|entry| {
            let fields = entry.as_object();
            let (title, url, content) = (trimmed_field(fields, "title"), trimmed_field(fields, "url"), trimmed_field(fields, "content"));
            let shown = [title, url].into_iter().find(|s| !s.is_empty()).unwrap_or(lang.t("无标题", "Untitled"));
            WebResultView { title: shown.to_owned(), content: (!content.is_empty()).then(|| Arc::from(content)) }
        })
        .collect();
    DetailBody::WebSearch { label: lang.t("搜索到的网站", "Sites searched").to_owned(), chips, findings: (!findings.is_empty()).then(|| Arc::from(findings)), results }
}

/// The families whose card is a picture of what a call that worked returned: a file's numbered lines, a diff, a listing, a parsed
/// report. An error message is none of those (drawn as one it would be a file one line long, or a directory holding a sentence),
/// so a failed call of these opens to the shared card instead: what it was asked, then what it answered.
fn is_result_picture(family: ToolViewFamily) -> bool {
    matches!(family, F::FileList | F::Grep | F::Read | F::Diff | F::BrowserSnapshot | F::BrowserScreenshot | F::BrowserConsole | F::BrowserNetwork | F::BrowserPage | F::AgentWait)
}

fn detail_body(item: &ToolItem<'_>, presentation: &ToolPresentation, lang: Lang) -> DetailBody {
    match presentation.family {
        F::FileList => file_list_body(item, lang),
        F::Grep => grep_body(item, lang),
        // No text and images present: the thumbnail is all there is.
        F::Read => read_body(item, lang).unwrap_or(DetailBody::ImageOnly),
        F::Diff => diff_body(item, lang),
        F::Terminal => terminal_body(item, lang),
        F::BrowserSnapshot => browser_snapshot_body(item, lang),
        F::BrowserEvaluate => browser_evaluate_body(item, lang),
        F::BrowserScreenshot => browser_screenshot_body(item, lang),
        F::BrowserConsole => browser_console_body(item, lang),
        F::BrowserNetwork => browser_network_body(item, lang),
        F::BrowserJson => browser_json_body(item, presentation, lang),
        F::BrowserPage => browser_page_body(item, lang),
        F::Memory => memory_body(item, lang),
        F::AgentRun => agent_run_body(item, lang),
        F::AgentWait => agent_wait_body(item, lang),
        F::AgentNote => agent_note_body(item, lang),
        F::WebSearch => web_search_body(item, presentation, lang),
        F::Question | F::Raw => raw_body(item, presentation, lang),
    }
}

/// What `ToolDetailRenderer` shows for a call.
pub fn tool_detail_model(item: &ToolItem<'_>, descriptor: Option<&ToolDescriptor>) -> ToolDetailModel {
    tool_detail_model_in(item, descriptor, Lang::current())
}

pub fn tool_detail_model_in(item: &ToolItem<'_>, descriptor: Option<&ToolDescriptor>, lang: Lang) -> ToolDetailModel {
    let presentation = get_tool_presentation_in(item, descriptor, lang, None, None);
    let phase = execution_phase(item);
    let body = if phase == ToolPhase::Running {
        DetailBody::Running(running_label_in(item, lang))
    } else if phase == ToolPhase::Done && !item.result.images.is_empty() {
        // A result carrying images is the image. Failures retain their messages.
        DetailBody::ImageOnly
    } else if phase == ToolPhase::Failed && is_result_picture(presentation.family) {
        // A failed call opens to an ordinary card, not to an alert: the row's title is what says it failed, and the card is the
        // account of it, the arguments and the error they met.
        raw_body(item, &presentation, lang)
    } else {
        detail_body(item, &presentation, lang)
    };
    let label = match &presentation.target {
        Some(target) => format!("{}：{target}", presentation.title),
        None => presentation.title.clone(),
    };
    ToolDetailModel { phase, family: presentation.family, label, body }
}

/// What the raw-data disclosure opens onto. Only built once it opens: a call's arguments and output can be large.
pub fn raw_data_text(item: &ToolItem<'_>) -> String {
    // A struct and not a `json!` object, whose keys would come out sorted: the tool first, as upstream writes it.
    #[derive(Serialize)]
    struct Raw<'a> {
        tool: &'a str,
        input: &'a JsonObject,
        result: &'a ToolResult,
    }
    serde_json::to_string_pretty(&Raw { tool: item.tool_name, input: item.input, result: item.result }).unwrap_or_default()
}
