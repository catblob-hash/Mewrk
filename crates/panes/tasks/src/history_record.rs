//! `lib/historyRecord.ts`: reading model for the conversation's history. Nothing here re-derives what happened: every
//! label, preview and diff is computed from the recorded bytes.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::line_diff::line_diff;
use crate::ojson::J;
use crate::util::{js_trim, utf16_len, utf16_prefix};

const PREVIEW_LIMIT: usize = 160;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub seq: i64,
    #[serde(default)]
    pub created_at: String,
    /// `request`, `response`, `hook`, `tool`, `result`, `edit` or `run`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<i64>,
    #[serde(default)]
    pub detail: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<HistoryUsage>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryPart {
    pub ordinal: i64,
    /// `system`, `systemDynamic`, `tools` or `message`.
    pub kind: String,
    pub hash: String,
    pub body: String,
    pub bytes: i64,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryOp {
    pub ordinal: i64,
    /// `insert`, `remove` or `replace`.
    pub op: String,
    pub context_id: String,
    pub position: Option<i64>,
    pub body: Option<String>,
    pub before: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryEntryDetail {
    pub entry: HistoryEntry,
    pub body: Option<String>,
    pub truncated: bool,
    pub parts: Option<Vec<HistoryPart>>,
    pub ops: Option<Vec<HistoryOp>>,
}

/// A request entry read as the summary bars are drawn from.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestSummary {
    pub seq: i64,
    pub created_at: String,
    /// `model`, `search` or `fetch`.
    pub kind: String,
    pub request_id: String,
    pub model_id: String,
    pub usage: Option<HistoryUsage>,
    /// Absent on entries recorded before the host counted them, which is not the same as zero.
    pub messages_added: Option<f64>,
}

fn detail_string<'a>(detail: &'a Map<String, Value>, key: &str) -> &'a str {
    detail.get(key).and_then(Value::as_str).unwrap_or("")
}

fn detail_number(detail: &Map<String, Value>, key: &str) -> Option<f64> {
    detail.get(key).and_then(Value::as_f64).filter(|v| v.is_finite())
}

fn detail_flag(detail: &Map<String, Value>, key: &str) -> bool {
    detail.get(key) == Some(&Value::Bool(true))
}

pub fn request_summary(entry: &HistoryEntry) -> RequestSummary {
    let kind = detail_string(&entry.detail, "type");
    RequestSummary {
        seq: entry.seq,
        created_at: entry.created_at.clone(),
        kind: if kind == "search" || kind == "fetch" { kind.to_owned() } else { "model".to_owned() },
        request_id: entry.request_id.clone().unwrap_or_default(),
        model_id: detail_string(&entry.detail, "modelId").to_owned(),
        usage: entry.usage,
        messages_added: detail_number(&entry.detail, "messagesAdded"),
    }
}

fn collapse(text: &str) -> String {
    let Some(line) = text.split('\n').map(js_trim).find(|l| !l.is_empty()) else { return String::new() };
    if utf16_len(line) > PREVIEW_LIMIT { format!("{}…", utf16_prefix(line, PREVIEW_LIMIT)) } else { line.to_owned() }
}

fn pretty(value: Option<&J>) -> String {
    value.map(J::pretty).unwrap_or_default()
}

fn text_of(value: Option<&J>) -> &str {
    value.and_then(J::as_str).unwrap_or("")
}

fn as_record(value: Option<J>) -> Option<J> {
    value.filter(J::is_obj)
}

const REASONING_PAYLOAD_KEYS: [&str; 4] = ["signature", "redactedData", "reasoningEncryptedContent", "itemId"];

fn render_content_part(part: &J, names: &mut Vec<String>) -> String {
    let kind = text_of(part.get("type"));
    match kind {
        "text" => text_of(part.get("text")).to_owned(),
        "reasoning" => {
            let options = part.get("providerOptions").filter(|o| o.is_obj());
            let signed = match options {
                Some(J::Obj(fields)) => fields.iter().any(|(_, v)| v.is_obj() && REASONING_PAYLOAD_KEYS.iter().any(|k| v.get(k).is_some_and(J::truthy))),
                _ => false,
            };
            let head = if signed { "[reasoning · signed]" } else { "[reasoning]" };
            // Whether a signature actually went out is the question this pane exists to answer.
            let tail = options.map(|o| format!("\n\n[providerOptions]\n{}", o.pretty())).unwrap_or_default();
            format!("{head}\n{}{tail}", text_of(part.get("text")))
        }
        "tool-call" => {
            let name = text_of(part.get("toolName"));
            if !name.is_empty() {
                names.push(name.to_owned());
            }
            format!("→ {name} ({})\n{}", text_of(part.get("toolCallId")), pretty(part.get("input")))
        }
        "tool-result" => {
            let name = text_of(part.get("toolName"));
            if !name.is_empty() {
                names.push(name.to_owned());
            }
            let output = part.get("output").filter(|o| o.is_obj());
            let body = match output {
                Some(o) if matches!(text_of(o.get("type")), "text" | "error-text") => text_of(o.get("value")).to_owned(),
                _ => pretty(part.get("output")),
            };
            format!("← {name} ({})\n{body}", text_of(part.get("toolCallId")))
        }
        // An unreadable row is still evidence, an absent one is not.
        _ => part.pretty(),
    }
}

struct DescribedMessage {
    detail: String,
    preview: String,
    text: String,
}

fn describe_message(message: &J) -> DescribedMessage {
    let mut names = Vec::new();
    let mut rendered = match message.get("content") {
        Some(J::Str(s)) => s.clone(),
        Some(J::Arr(items)) => items.iter().map(|e| if e.is_obj() { render_content_part(e, &mut names) } else { e.pretty() }).collect::<Vec<_>>().join("\n\n"),
        other => pretty(other),
    };
    if let Some(options) = message.get("providerOptions").filter(|o| o.is_obj()) {
        rendered = format!("{rendered}\n\n[providerOptions]\n{}", options.pretty());
    }
    let mut seen = HashSet::new();
    names.retain(|n: &String| seen.insert(n.clone()));
    DescribedMessage { detail: names.join(", "), preview: collapse(&rendered), text: rendered }
}

/// A counter nobody reported stays absent: the pane says "not recorded" for what it cannot vouch for. Input is summed
/// across rounds though each replays the history; that repetition is what was paid for.
pub fn sum_usage(usages: &[Option<HistoryUsage>]) -> HistoryUsage {
    fn sum(usages: &[Option<HistoryUsage>], get: fn(&HistoryUsage) -> Option<i64>) -> Option<i64> {
        usages.iter().flatten().filter_map(get).fold(None, |t, v| Some(t.unwrap_or(0) + v))
    }
    HistoryUsage { input_tokens: sum(usages, |u| u.input_tokens), cached_input_tokens: sum(usages, |u| u.cached_input_tokens), output_tokens: sum(usages, |u| u.output_tokens) }
}

pub fn usage_is_empty(usage: &HistoryUsage) -> bool {
    usage.input_tokens.is_none() && usage.cached_input_tokens.is_none() && usage.output_tokens.is_none()
}

fn opens_round(summary: &RequestSummary, previous: &RequestSummary) -> bool {
    if summary.kind != "model" {
        return false;
    }
    if !summary.request_id.is_empty() && summary.request_id == previous.request_id {
        return false;
    }
    summary.messages_added.is_none_or(|n| n > 0.0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BarKind {
    /// Something the user sent, and everything it set off.
    Turn,
    /// A turn whose last request never got its answer back.
    Interrupted,
    /// Changes the user made to the context by hand, back to back.
    Edits,
    /// Something the user sent that no request has carried yet.
    Pending,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistoryBar {
    pub key: String,
    pub kind: BarKind,
    pub index: usize,
    pub requests: Vec<RequestSummary>,
    pub entries: Vec<HistoryEntry>,
    pub model_id: String,
    pub mixed_models: bool,
    pub usage: HistoryUsage,
    pub created_at: String,
}

fn opens_turn(entry: &HistoryEntry) -> bool {
    matches!(entry.detail.get("source").and_then(Value::as_str), Some("message" | "baseline"))
}

struct DraftBar<'a> {
    edits: bool,
    opener: &'a HistoryEntry,
    requests: Vec<RequestSummary>,
    entries: Vec<&'a HistoryEntry>,
}

/// Splits a history into the bars the pane draws. Every entry of a run goes with the turn that run's first request
/// went out in, wherever its number falls. Nothing is dropped and nothing is re-ordered inside a bar.
pub fn history_bars(entries: &[HistoryEntry], live: bool) -> Vec<HistoryBar> {
    let mut drafts: Vec<DraftBar<'_>> = Vec::new();
    let mut by_run: HashMap<String, usize> = HashMap::new();
    // Entries of a run whose first request has not gone out yet, and the turn they followed. In arrival order.
    let mut early: Vec<(String, Option<usize>, Vec<&HistoryEntry>)> = Vec::new();
    let mut turn: Option<usize> = None;
    let mut previous: Option<RequestSummary> = None;
    fn open<'a>(drafts: &mut Vec<DraftBar<'a>>, edits: bool, opener: &'a HistoryEntry) -> usize {
        drafts.push(DraftBar { edits, opener, requests: Vec::new(), entries: Vec::new() });
        drafts.len() - 1
    }
    for entry in entries {
        if entry.kind == "edit" {
            if opens_turn(entry) {
                // A turn nothing has sent yet takes the next message as part of itself.
                let at = match turn.filter(|&t| drafts[t].requests.is_empty()) {
                    Some(t) => t,
                    None => open(&mut drafts, false, entry),
                };
                turn = Some(at);
                drafts[at].entries.push(entry);
            } else {
                let at = match drafts.len().checked_sub(1).filter(|&l| drafts[l].edits) {
                    Some(l) => l,
                    None => open(&mut drafts, true, entry),
                };
                drafts[at].entries.push(entry);
                turn = None;
            }
            continue;
        }
        if entry.kind == "request" {
            let summary = request_summary(entry);
            let mut draft = if summary.request_id.is_empty() { None } else { by_run.get(&summary.request_id).copied() };
            if draft.is_none() {
                let joins = turn.is_some_and(|t| drafts[t].requests.is_empty() || previous.as_ref().is_some_and(|p| !opens_round(&summary, p)));
                let at = match turn.filter(|_| joins) {
                    Some(t) => t,
                    None => open(&mut drafts, false, entry),
                };
                turn = Some(at);
                draft = Some(at);
                if !summary.request_id.is_empty() {
                    by_run.insert(summary.request_id.clone(), at);
                    if let Some(i) = early.iter().position(|w| w.0 == summary.request_id) {
                        let (_, _, waiting) = early.remove(i);
                        drafts[at].entries.extend(waiting);
                    }
                }
            }
            drafts[draft.expect("set above")].requests.push(summary.clone());
            previous = Some(summary);
            continue;
        }
        let request_id = entry.request_id.as_deref().filter(|r| !r.is_empty());
        if let Some(&at) = request_id.and_then(|r| by_run.get(r)) {
            drafts[at].entries.push(entry);
        } else if let Some(r) = request_id {
            match early.iter_mut().find(|w| w.0 == r) {
                Some(w) => w.2.push(entry),
                None => early.push((r.to_owned(), turn, vec![entry])),
            }
        } else {
            let at = match turn {
                Some(t) => t,
                None => open(&mut drafts, false, entry),
            };
            turn = Some(at);
            drafts[at].entries.push(entry);
        }
    }
    // A run a hook stopped before it sent anything stays with the turn it followed.
    for (_, waiting_turn, waiting) in early {
        let at = match waiting_turn {
            Some(t) => t,
            None => open(&mut drafts, false, waiting[0]),
        };
        drafts[at].entries.extend(waiting);
    }

    let answered: HashSet<i64> = entries.iter().filter(|e| e.kind == "response").filter_map(|e| e.answers).collect();
    let first_response = entries.iter().filter(|e| e.kind == "response").map(|e| e.seq).min().unwrap_or(i64::MAX);
    let running = if live { drafts.iter().rposition(|d| !d.requests.is_empty()) } else { None };

    let mut turns = 0;
    drafts
        .iter()
        .enumerate()
        .map(|(i, draft)| {
            let requests = draft.requests.clone();
            let models: Vec<&RequestSummary> = requests.iter().filter(|r| r.kind == "model").collect();
            let model_id = models.first().copied().or(requests.first()).map_or(String::new(), |r| r.model_id.clone());
            let kind = if draft.edits {
                BarKind::Edits
            } else if requests.is_empty() {
                BarKind::Pending
            } else if models.last().is_some_and(|last| Some(i) != running && first_response < last.seq && !answered.contains(&last.seq)) {
                // An answer can only be missing once answers are being recorded at all.
                BarKind::Interrupted
            } else {
                BarKind::Turn
            };
            let index = if draft.edits {
                0
            } else {
                turns += 1;
                turns
            };
            let mut sorted: Vec<HistoryEntry> = draft.entries.iter().map(|e| (*e).clone()).collect();
            sorted.sort_by_key(|e| e.seq);
            let usages: Vec<Option<HistoryUsage>> = requests.iter().map(|r| r.usage).collect();
            HistoryBar {
                key: format!("b{}", draft.opener.seq),
                kind,
                index,
                mixed_models: models.iter().any(|r| r.model_id != model_id),
                usage: sum_usage(&usages),
                created_at: draft.opener.created_at.clone(),
                model_id,
                requests,
                entries: sorted,
            }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum BarItem {
    /// A change to the timeline, drawn as its messages.
    Change(HistoryEntry),
    /// An entry of a run that never settled, drawn as itself: nothing else shows what it did.
    Event(HistoryEntry),
    /// A request of a run that never settled, drawn as the tools it was first to hand over by append.
    Request(RequestSummary),
}

/// What a bar draws, in order. A settled run's responses and calls are what its settled messages were made from, so
/// they are not drawn again.
pub fn bar_items(bar: &HistoryBar) -> Vec<BarItem> {
    let settled: HashSet<&str> = bar.entries.iter().filter(|e| e.kind == "run").filter_map(|e| e.request_id.as_deref()).filter(|r| !r.is_empty()).collect();
    let is_settled = |r: Option<&str>| r.is_some_and(|r| !r.is_empty() && settled.contains(r));
    let mut items: Vec<(i64, BarItem)> = Vec::new();
    for entry in &bar.entries {
        if entry.kind == "edit" || entry.kind == "run" {
            items.push((entry.seq, BarItem::Change(entry.clone())));
        } else if !is_settled(entry.request_id.as_deref()) {
            items.push((entry.seq, BarItem::Event(entry.clone())));
        }
    }
    for request in &bar.requests {
        if request.kind == "model" && !is_settled(Some(&request.request_id)) {
            items.push((request.seq, BarItem::Request(request.clone())));
        }
    }
    items.sort_by_key(|i| i.0);
    items.into_iter().map(|i| i.1).collect()
}

const MARKER_OPTIONS_KEY: &str = "mewrk";
const MARKER_TOOLS_KEY: &str = "toolAddition";

/// The tools a request handed over by append rather than declared, in the order its markers name them.
pub fn appended_tools(parts: &[HistoryPart]) -> Vec<String> {
    let needle = format!("\"{MARKER_TOOLS_KEY}\"");
    let mut names: Vec<String> = Vec::new();
    for part in parts {
        if part.kind != "message" || !part.body.contains(&needle) {
            continue;
        }
        let Some(message) = as_record(J::parse(&part.body)) else { continue };
        if message.get("role").and_then(J::as_str) != Some("system") {
            continue;
        }
        let tools = message.get("providerOptions").and_then(|o| o.get(MARKER_OPTIONS_KEY)).and_then(|o| o.get(MARKER_TOOLS_KEY)).and_then(J::as_arr);
        for tool in tools.unwrap_or_default() {
            if let Some(name) = tool.as_str()
                && !names.iter().any(|n| n == name)
            {
                names.push(name.to_owned());
            }
        }
    }
    names
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BadgeTone {
    Hook,
    Warning,
    Neutral,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventBadge {
    pub label: String,
    pub tone: BadgeTone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventTone {
    Response,
    Hook,
    Tool,
    Result,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DescribedEvent {
    pub tone: EventTone,
    pub label: &'static str,
    pub detail: String,
    pub preview: String,
    pub badges: Vec<EventBadge>,
    pub text: String,
    pub patch: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    User,
    Assistant,
    Reasoning,
    System,
    ToolsAdded,
    Tools,
    Tool,
    Prompt,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MessageRow {
    pub key: String,
    /// `insert`, `remove` or `replace`.
    pub change: String,
    pub kind: MessageKind,
    /// Its role; empty for a row the record never saw.
    pub label: &'static str,
    pub detail: String,
    pub badges: Vec<EventBadge>,
    pub preview: String,
    pub text: String,
    pub patch: String,
    /// Nothing to open: the row says all there is in its title.
    pub fixed: bool,
    pub additions: usize,
    pub deletions: usize,
}

pub struct NativeCompactionTitle<'a> {
    pub model: &'a str,
    pub model_name: &'a str,
    pub tokens_before: f64,
    pub tokens_after: f64,
}

pub struct EventLabels {
    pub tools_added: Box<dyn Fn(usize) -> String>,
    pub tools_registered: Box<dyn Fn(usize) -> String>,
    pub native_compaction: Box<dyn Fn(NativeCompactionTitle<'_>) -> String>,
    pub local_only: String,
    pub interrupted: String,
    pub unseen: String,
    pub rewrote_input: String,
    pub added_context: String,
    pub blocked: String,
    pub halted: String,
    pub failed: String,
    pub denied: String,
    pub permission: Box<dyn Fn(&str) -> String>,
    pub truncated: String,
    pub paused: String,
    pub images: Box<dyn Fn(usize) -> String>,
    pub files: Box<dyn Fn(usize) -> String>,
}

/// The tools a request was first to hand over by append, drawn as the system message that hands them over.
pub fn describe_appended(before: Option<&[HistoryPart]>, after: &[HistoryPart], key: &str, labels: &EventLabels) -> Vec<MessageRow> {
    let earlier = before.map(appended_tools).unwrap_or_default();
    let tools: Vec<String> = appended_tools(after).into_iter().filter(|n| !earlier.contains(n)).collect();
    if tools.is_empty() {
        return Vec::new();
    }
    vec![MessageRow {
        key: format!("{key}-appended"),
        change: "insert".to_owned(),
        kind: MessageKind::ToolsAdded,
        label: "system",
        detail: (labels.tools_added)(tools.len()),
        badges: Vec::new(),
        preview: tools.join(", "),
        text: tools.join("\n"),
        patch: String::new(),
        fixed: false,
        additions: 0,
        deletions: 0,
    }]
}

#[derive(Clone)]
struct DeclaredTool {
    name: String,
    text: String,
}

/// The tools a request declared up front, minus the ones it handed over by append. A list the record had to cut is
/// no longer JSON; it is still compared and shown as the text it is.
fn declared_tools(parts: &[HistoryPart]) -> Option<Vec<DeclaredTool>> {
    let part = parts.iter().find(|p| p.kind == "tools")?;
    let Some(J::Arr(list)) = J::parse(&part.body) else { return Some(vec![DeclaredTool { name: String::new(), text: part.body.clone() }]) };
    let appended = appended_tools(parts);
    Some(
        list.iter()
            .filter_map(|tool| {
                let name = text_of(tool.get("name"));
                if appended.iter().any(|n| n == name) {
                    return None;
                }
                let J::Obj(fields) = tool else { return Some(DeclaredTool { name: String::new(), text: tool.pretty() }) };
                let definition = J::Obj(fields.iter().filter(|f| f.0 != "name").cloned().collect());
                Some(DeclaredTool { name: name.to_owned(), text: format!("{name}\n{}", definition.pretty()) })
            })
            .collect(),
    )
}

/// The tools a turn's request declared, when the list is new or differs from the one before it. It is the request's
/// `tools` field rather than a message, and tagged as that.
pub fn describe_tools(before: Option<&[HistoryPart]>, after: &[HistoryPart], key: &str, labels: &EventLabels) -> Vec<MessageRow> {
    let now = declared_tools(after);
    let then = before.and_then(declared_tools);
    if now.is_none() && then.is_none() {
        return Vec::new();
    }
    let body = |tools: &Option<Vec<DeclaredTool>>| tools.as_deref().unwrap_or_default().iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n\n");
    let (now_text, then_text) = (body(&now), body(&then));
    if now.is_some() && then.is_some() && now_text == then_text {
        return Vec::new();
    }
    let shown: &[DeclaredTool] = now.as_deref().or(then.as_deref()).unwrap_or_default();
    let change = match (&now, &then) {
        (Some(_), Some(_)) => "replace",
        (Some(_), None) => "insert",
        _ => "remove",
    };
    let truncated = after.iter().chain(before.unwrap_or_default()).any(|p| p.kind == "tools" && p.truncated);
    let mut preview = shown.iter().map(|t| t.name.as_str()).filter(|n| !n.is_empty()).collect::<Vec<_>>().join(", ");
    if let (Some(now), Some(then)) = (&now, &then) {
        // What a reader of a changed list is after is which tools it is about.
        let previous: HashMap<&str, &str> = then.iter().map(|t| (t.name.as_str(), t.text.as_str())).collect();
        let current: HashSet<&str> = now.iter().map(|t| t.name.as_str()).collect();
        let added: Vec<&str> = now.iter().filter(|t| !previous.contains_key(t.name.as_str())).map(|t| t.name.as_str()).collect();
        let removed: Vec<&str> = then.iter().filter(|t| !current.contains(t.name.as_str())).map(|t| t.name.as_str()).collect();
        let rewritten: Vec<&str> = now.iter().filter(|t| previous.get(t.name.as_str()).is_some_and(|p| *p != t.text)).map(|t| t.name.as_str()).collect();
        preview = [("+", added), ("−", removed), ("~", rewritten)].iter().filter(|(_, l)| !l.is_empty()).map(|(m, l)| format!("{m}{}", l.join(", "))).collect::<Vec<_>>().join(" ");
    }
    let diff = (now.is_some() && then.is_some()).then(|| line_diff(&then_text, &now_text, Some("tools")));
    vec![MessageRow {
        key: format!("{key}-tools"),
        change: change.to_owned(),
        kind: MessageKind::Tools,
        label: "tools",
        detail: (labels.tools_registered)(shown.len()),
        badges: if truncated { vec![EventBadge { label: labels.truncated.clone(), tone: BadgeTone::Warning }] } else { Vec::new() },
        preview: collapse(&preview),
        text: if change == "remove" { then_text } else { now_text },
        patch: diff.as_ref().map(|d| d.patch.clone()).unwrap_or_default(),
        fixed: false,
        additions: diff.as_ref().map_or(0, |d| d.additions),
        deletions: diff.as_ref().map_or(0, |d| d.deletions),
    }]
}

fn pretty_body(body: Option<&str>) -> String {
    let Some(body) = body else { return String::new() };
    match J::parse(body) {
        None | Some(J::Null) => body.to_owned(),
        Some(value) => value.pretty(),
    }
}

struct RowBody {
    kind: MessageKind,
    label: &'static str,
    detail: String,
    badges: Vec<EventBadge>,
    preview: String,
    text: String,
    fixed: bool,
}

fn describe_context_row(body: &str, labels: &EventLabels) -> RowBody {
    let plain = |kind, label, detail: String, badges, preview, text| RowBody { kind, label, detail, badges, preview, text, fixed: false };
    let Some(row) = as_record(J::parse(body)) else { return plain(MessageKind::System, "", String::new(), Vec::new(), collapse(body), body.to_owned()) };
    let kind = text_of(row.get("kind"));
    let content = match row.get("content") {
        Some(J::Str(s)) => s.clone(),
        None => String::new(),
        other => pretty(other),
    };
    let mut badges = Vec::new();
    if row.get("interrupted") == Some(&J::Bool(true)) {
        badges.push(EventBadge { label: labels.interrupted.clone(), tone: BadgeTone::Warning });
    }
    match kind {
        "tool" => {
            let result = row.get("result").filter(|r| r.is_obj());
            let output = text_of(result.and_then(|r| r.get("output")));
            let rewritten = row.has("requestedInput");
            if rewritten {
                badges.push(EventBadge { label: labels.rewrote_input.clone(), tone: BadgeTone::Hook });
            }
            if result.and_then(|r| r.get("success")) == Some(&J::Bool(false)) {
                badges.push(EventBadge { label: labels.failed.clone(), tone: BadgeTone::Warning });
            }
            // The arguments, then what came back, each as the model reads them; what a hook rewrote comes last.
            let mut sections = vec![pretty(row.get("input"))];
            if !output.is_empty() {
                sections.push(output.to_owned());
            }
            if rewritten {
                sections.push(format!("[requestedInput]\n{}", pretty(row.get("requestedInput"))));
            }
            let preview = collapse(&row.get("input").map(J::stringify).unwrap_or_default());
            plain(MessageKind::Tool, "tool", text_of(row.get("toolName")).to_owned(), badges, preview, sections.join("\n\n"))
        }
        "user" => {
            let count = |k: &str| row.get(k).and_then(J::as_arr).map_or(0, <[J]>::len);
            let (images, files) = (count("images"), count("files"));
            if images > 0 {
                badges.push(EventBadge { label: (labels.images)(images), tone: BadgeTone::Neutral });
            }
            if files > 0 {
                badges.push(EventBadge { label: (labels.files)(files), tone: BadgeTone::Neutral });
            }
            plain(MessageKind::User, "user", String::new(), badges, collapse(&content), content)
        }
        "assistant" => plain(MessageKind::Assistant, "assistant", String::new(), badges, collapse(&content), content),
        "reasoning" => plain(MessageKind::Reasoning, "reasoning", String::new(), badges, collapse(&content), content),
        _ => {
            if kind == "system"
                && let Some(added) = row.get("toolsAdded").and_then(J::as_arr)
            {
                let tools: Vec<&str> = added.iter().filter_map(J::as_str).filter(|s| !s.is_empty()).collect();
                let text = if content.is_empty() { tools.join("\n") } else { content };
                return plain(MessageKind::ToolsAdded, "system", (labels.tools_added)(tools.len()), badges, tools.join(", "), text);
            }
            // A native compaction's card is its title and nothing more, as on the timeline.
            if kind == "system"
                && let Some(c) = row.get("nativeCompaction").filter(|c| c.is_obj())
            {
                let tokens = |k: &str| match c.get(k) {
                    Some(J::Num(n)) => n.as_f64().unwrap_or(0.0),
                    _ => 0.0,
                };
                let detail = (labels.native_compaction)(NativeCompactionTitle { model: text_of(c.get("model")), model_name: text_of(c.get("modelName")), tokens_before: tokens("tokensBefore"), tokens_after: tokens("tokensAfter") });
                return RowBody { kind: MessageKind::System, label: "system", detail, badges, preview: String::new(), text: String::new(), fixed: true };
            }
            if row.get("localOnly") == Some(&J::Bool(true)) {
                badges.push(EventBadge { label: labels.local_only.clone(), tone: BadgeTone::Neutral });
            }
            let hook = row.get("hookExecution").filter(|h| h.is_obj());
            // A row of no kind the timeline knows gets no role rather than a wrong one.
            plain(MessageKind::System, if kind == "system" { "system" } else { "" }, text_of(hook.and_then(|h| h.get("hookName"))).to_owned(), badges, collapse(&content), content)
        }
    }
}

/// The messages one change to the timeline added, removed and rewrote, in the order it made them.
pub fn describe_change(entry: &HistoryEntry, detail: &HistoryEntryDetail, labels: &EventLabels) -> Vec<MessageRow> {
    detail
        .ops
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|op| {
            let after = op.body.as_deref().map(|b| describe_context_row(b, labels));
            let before = op.before.as_deref().map(|b| describe_context_row(b, labels));
            let diff = match (op.op.as_str(), &before, &after) {
                ("replace", Some(b), Some(a)) => {
                    let path = [a.label, op.context_id.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("#");
                    Some(line_diff(&b.text, &a.text, Some(&path)))
                }
                _ => None,
            };
            let shown = (if op.op == "remove" { before } else { after }).unwrap_or_else(|| RowBody { kind: MessageKind::System, label: "", detail: String::new(), badges: Vec::new(), preview: labels.unseen.clone(), text: labels.unseen.clone(), fixed: false });
            // The shell of a round that only called tools says nothing.
            if op.op == "insert" && shown.kind == MessageKind::Assistant && shown.text.is_empty() && shown.badges.is_empty() {
                return None;
            }
            Some(MessageRow {
                key: format!("{}-{}", entry.seq, op.ordinal),
                change: op.op.clone(),
                kind: shown.kind,
                label: shown.label,
                detail: shown.detail,
                badges: shown.badges,
                preview: shown.preview,
                text: shown.text,
                patch: diff.as_ref().map(|d| d.patch.clone()).unwrap_or_default(),
                fixed: shown.fixed,
                additions: diff.as_ref().map_or(0, |d| d.additions),
                deletions: diff.as_ref().map_or(0, |d| d.deletions),
            })
        })
        .collect()
}

/// The system prompt a turn opened with, when it is new or differs from the one the request before it carried.
pub fn describe_prompts(before: Option<&[HistoryPart]>, after: &[HistoryPart], key: &str, labels: &EventLabels) -> Vec<MessageRow> {
    ["system", "systemDynamic"]
        .iter()
        .filter_map(|kind| {
            let now = after.iter().find(|p| p.kind == *kind);
            let then = before.unwrap_or_default().iter().find(|p| p.kind == *kind);
            if let (Some(n), Some(t)) = (now, then)
                && n.hash == t.hash
            {
                return None;
            }
            let shown = now.or(then)?;
            let change = match (now, then) {
                (Some(_), Some(_)) => "replace",
                (Some(_), None) => "insert",
                _ => "remove",
            };
            let diff = now.zip(then).map(|(n, t)| line_diff(&t.body, &n.body, Some("system")));
            Some(MessageRow {
                key: format!("{key}-{kind}"),
                change: change.to_owned(),
                kind: MessageKind::Prompt,
                label: "system",
                detail: String::new(),
                badges: if shown.truncated { vec![EventBadge { label: labels.truncated.clone(), tone: BadgeTone::Warning }] } else { Vec::new() },
                preview: collapse(&shown.body),
                text: shown.body.clone(),
                patch: diff.as_ref().map(|d| d.patch.clone()).unwrap_or_default(),
                fixed: false,
                additions: diff.as_ref().map_or(0, |d| d.additions),
                deletions: diff.as_ref().map_or(0, |d| d.deletions),
            })
        })
        .collect()
}

/// One entry of a run that never settled. Before its body is read the row still says what it is, from the list's
/// metadata.
pub fn describe_event(entry: &HistoryEntry, detail: Option<&HistoryEntryDetail>, labels: &EventLabels) -> DescribedEvent {
    let meta = &entry.detail;
    let body = detail.and_then(|d| d.body.as_deref());
    let record = body.and_then(|b| as_record(J::parse(b)));
    let badge = |label: &str, tone| EventBadge { label: label.to_owned(), tone };
    match entry.kind.as_str() {
        "response" => {
            let described = record.as_ref().map(describe_message);
            let raw = detail_string(meta, "rawFinishReason");
            let finish = if raw.is_empty() { detail_string(meta, "finishReason") } else { raw };
            let mut badges = Vec::new();
            if finish == "pause_turn" {
                badges.push(badge(&labels.paused, BadgeTone::Neutral));
            }
            if finish == "length" || finish == "max_tokens" {
                badges.push(badge(&labels.truncated, BadgeTone::Warning));
            }
            let (detail, preview, text) = match described {
                Some(d) => (d.detail, d.preview, d.text),
                None => (String::new(), String::new(), pretty_body(body)),
            };
            DescribedEvent { tone: EventTone::Response, label: "assistant", detail, preview, badges, text, patch: String::new() }
        }
        "hook" => {
            let mut badges = Vec::new();
            if detail_flag(meta, "halted") {
                badges.push(badge(&labels.halted, BadgeTone::Warning));
            } else if detail_flag(meta, "blocked") {
                badges.push(badge(&labels.blocked, BadgeTone::Hook));
            }
            let permission = detail_string(meta, "permission");
            if !permission.is_empty() && !detail_flag(meta, "blocked") {
                badges.push(badge(&(labels.permission)(permission), BadgeTone::Hook));
            }
            if detail_flag(meta, "rewroteInput") {
                badges.push(badge(&labels.rewrote_input, BadgeTone::Hook));
            }
            if detail_flag(meta, "addedContext") {
                badges.push(badge(&labels.added_context, BadgeTone::Hook));
            }
            if meta.get("success") == Some(&Value::Bool(false)) {
                badges.push(badge(&labels.failed, BadgeTone::Warning));
            }
            let mut sections: Vec<(String, String)> = Vec::new();
            if let Some(r) = &record {
                for key in ["reason", "systemMessage", "additionalContext"] {
                    if let Some(s) = r.get(key).and_then(J::as_str) {
                        sections.push((key.to_owned(), s.to_owned()));
                    }
                }
                if r.has("updatedInput") {
                    sections.push(("updatedInput".to_owned(), pretty(r.get("updatedInput"))));
                }
                if let Some(s) = r.get("output").and_then(J::as_str).filter(|s| !s.is_empty()) {
                    sections.push(("output".to_owned(), s.to_owned()));
                }
            }
            let text = if sections.is_empty() { pretty_body(body) } else { sections.iter().map(|(k, v)| format!("[{k}]\n{v}")).collect::<Vec<_>>().join("\n\n") };
            let who = [detail_string(meta, "event"), detail_string(meta, "hookName"), detail_string(meta, "matcher")].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ");
            DescribedEvent { tone: EventTone::Hook, label: "hook", detail: who, preview: sections.first().map_or(String::new(), |s| collapse(&s.1)), badges, text, patch: String::new() }
        }
        "tool" => {
            let input = record.as_ref().map_or(String::new(), |r| pretty(r.get("input")));
            let mut badges = Vec::new();
            if detail_flag(meta, "rewritten") {
                badges.push(badge(&labels.rewrote_input, BadgeTone::Hook));
            }
            let denied = detail_string(meta, "denied");
            if !denied.is_empty() {
                badges.push(badge(&labels.denied, BadgeTone::Warning));
            }
            let name = detail_string(meta, "name");
            let patch = match &record {
                Some(r) if r.has("requestedInput") => line_diff(&pretty(r.get("requestedInput")), &input, Some(name)).patch,
                _ => String::new(),
            };
            let preview = collapse(&record.as_ref().and_then(|r| r.get("input")).map(J::stringify).unwrap_or_default());
            let denied_section = if denied.is_empty() { String::new() } else { format!("[denied]\n{denied}") };
            let text = [input.as_str(), denied_section.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("\n\n");
            DescribedEvent { tone: EventTone::Tool, label: "tool", detail: name.to_owned(), preview, badges, text, patch }
        }
        "result" => {
            let output = text_of(record.as_ref().and_then(|r| r.get("output")));
            let mut badges = Vec::new();
            if meta.get("success") == Some(&Value::Bool(false)) {
                badges.push(badge(&labels.failed, BadgeTone::Warning));
            }
            if let Some(images) = detail_number(meta, "images").filter(|n| *n != 0.0) {
                badges.push(badge(&(labels.images)(images as usize), BadgeTone::Neutral));
            }
            let text = if output.is_empty() { pretty_body(body) } else { output.to_owned() };
            DescribedEvent { tone: EventTone::Result, label: "tool", detail: detail_string(meta, "name").to_owned(), preview: collapse(output), badges, text, patch: String::new() }
        }
        _ => DescribedEvent { tone: EventTone::Result, label: "", detail: String::new(), preview: String::new(), badges: Vec::new(), text: pretty_body(body), patch: String::new() },
    }
}
