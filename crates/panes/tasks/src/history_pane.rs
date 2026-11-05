//! `components/HistoryPane.tsx`: the conversation's history, oldest first, as the bars the things that happened to it
//! make. Each bar opens into the messages it added, removed or rewrote.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{Local as ChronoLocal, TimeZone, Timelike, Datelike};
use mewrk_client::prelude::*;

use fluent::{TypeStyle, Weight};

use crate::history_record::*;
use crate::util::{parse_time, to_fixed};
use crate::widgets::*;

/// How often a running turn's new entries are picked up.
const STREAMING_POLL: Duration = Duration::from_millis(1000);
/// Debounced so a burst of trunk changes costs one read, not one per row.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Where entries come from. The real one calls the host; tests answer from fixtures, as upstream's mock the runtime.
pub trait HistoryReader {
    fn list(&self, app: &App, conversation_id: &str, owners: Option<&[String]>, done: Box<dyn FnOnce(&mut App, Result<Vec<HistoryEntry>, String>) + Send>);
    fn load(&self, app: &App, conversation_id: &str, seq: i64, done: Box<dyn FnOnce(&mut App, Result<Option<HistoryEntryDetail>, String>) + Send>);
}

pub struct HostHistoryReader;

impl HistoryReader for HostHistoryReader {
    fn list(&self, app: &App, conversation_id: &str, owners: Option<&[String]>, done: Box<dyn FnOnce(&mut App, Result<Vec<HistoryEntry>, String>) + Send>) {
        app.invoke("list_history_entries", json!({ "conversationId": conversation_id, "owners": owners }), done);
    }
    fn load(&self, app: &App, conversation_id: &str, seq: i64, done: Box<dyn FnOnce(&mut App, Result<Option<HistoryEntryDetail>, String>) + Send>) {
        app.invoke("load_history_entry", json!({ "conversationId": conversation_id, "seq": seq }), done);
    }
}

pub struct HistoryPaneProps {
    pub conversation_id: String,
    /// Change signal: a new value means the trunk moved, so the list is refetched. Upstream's `contexts` array identity.
    pub contexts_revision: u64,
    /// Rows appear as they are sent, so a running turn is polled rather than awaited.
    pub streaming: bool,
    /// Whose history to read: `None` is the session's own, and a child's address is that child's. An empty list asks
    /// for nothing rather than for the trunk.
    pub owners: Option<Vec<String>>,
    pub reader: Option<Rc<dyn HistoryReader>>,
}

impl HistoryPaneProps {
    pub fn new(conversation_id: impl Into<String>, contexts_revision: u64, streaming: bool) -> Self {
        Self { conversation_id: conversation_id.into(), contexts_revision, streaming, owners: None, reader: None }
    }
}

type PaneKey = (String, Option<Vec<String>>);

#[derive(Default)]
pub struct PaneState {
    pub entries: Option<Vec<HistoryEntry>>,
    /// `None` while a read is in flight.
    pub details: HashMap<i64, Option<HistoryEntryDetail>>,
    pub detail_errors: HashMap<i64, String>,
    pub open_rows: HashSet<String>,
    pub open_parts: HashSet<String>,
    pub failure: Option<String>,
    /// Entries already read or in flight, so an expansion never reads twice.
    requested: HashSet<i64>,
    /// Done once per conversation: a reader who closes the newest bar has closed it.
    primed: bool,
    seen: Option<(u64, bool)>,
    /// A read's answer only counts if nothing has changed since it was asked.
    generation: u64,
    due: Option<Instant>,
    last_read: Option<Instant>,
}

#[derive(Default)]
pub struct HistoryPanes {
    panes: HashMap<PaneKey, PaneState>,
}

thread_local! {
    /// Panes that stopped being drawn, whose state goes with them as a component's does on unmount.
    static GONE: RefCell<Vec<PaneKey>> = const { RefCell::new(Vec::new()) };
}

struct Mounted(PaneKey);

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = GONE.try_with(|g| g.borrow_mut().push(self.0.clone()));
    }
}

/// Token counts sit in a 10px column, so they are shortened rather than wrapped.
pub fn format_history_tokens(tokens: i64) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else if tokens < 10_000 {
        format!("{}k", to_fixed(tokens as f64 / 1000.0, 1))
    } else if tokens < 1_000_000 {
        format!("{}k", (tokens as f64 / 1000.0 + 0.5).floor() as i64)
    } else {
        format!("{}M", to_fixed(tokens as f64 / 1_000_000.0, 1))
    }
}

/// `toLocaleString` with numeric month and day and two-digit time, in the app's language.
pub fn format_history_time(created_at: &str) -> String {
    let Some(at) = parse_time(created_at).and_then(|ms| ChronoLocal.timestamp_millis_opt(ms).single()) else { return String::new() };
    if mewrk_client::i18n::is_english() {
        let (pm, hour) = at.hour12();
        format!("{}/{}, {:02}:{:02}:{:02} {}", at.month(), at.day(), hour, at.minute(), at.second(), if pm { "PM" } else { "AM" })
    } else {
        format!("{}/{} {:02}:{:02}:{:02}", at.month(), at.day(), at.hour(), at.minute(), at.second())
    }
}

pub fn event_labels() -> EventLabels {
    EventLabels {
        tools_added: Box::new(|n| tf("追加 {n} 个工具", "{n} tools added", &[("n", &n)])),
        tools_registered: Box::new(|n| tf("注册 {n} 个工具", "{n} tools registered", &[("n", &n)])),
        native_compaction: Box::new(|c| {
            let name = c.model_name.trim();
            tf("由 {model} 压缩，{before} token → {after} token", "Compacted by {model}, {before} tokens → {after} tokens", &[("model", &if name.is_empty() { c.model } else { name }), ("before", &mewrk_client::runs::format_compact_token_count(c.tokens_before as u64)), ("after", &mewrk_client::runs::format_compact_token_count(c.tokens_after as u64))])
        }),
        local_only: t("仅本地", "local only").into(),
        interrupted: t("中断前的部分", "cut off").into(),
        unseen: t("（记录里没有这一行被删前的内容）", "(the record never saw this row before it went)").into(),
        rewrote_input: t("改写了输入", "rewrote input").into(),
        added_context: t("注入上下文", "added context").into(),
        blocked: t("拦下", "blocked").into(),
        halted: t("中止", "halted").into(),
        failed: t("失败", "failed").into(),
        denied: t("被拒绝", "denied").into(),
        permission: Box::new(|decision| match decision {
            "allow" => t("放行", "allowed").to_owned(),
            "ask" => t("要求确认", "asked").to_owned(),
            "deny" => t("拒绝", "denied").to_owned(),
            other => other.to_owned(),
        }),
        truncated: t("输出被截断", "cut off").into(),
        paused: t("暂停", "paused").into(),
        images: Box::new(|n| tf("{n} 张图", "{n} images", &[("n", &n)])),
        files: Box::new(|n| tf("{n} 个文件", "{n} files", &[("n", &n)])),
    }
}

const SMALL: TypeStyle = ts(10.0, 14.0, Weight::Regular);
const TAG: TypeStyle = ts(9.5, 13.0, Weight::Semibold);
const TEXT_TY: TypeStyle = mono(10.5, 16.0);

fn pane<'a>(app: &'a mut App, key: &PaneKey) -> &'a mut PaneState {
    app.local::<HistoryPanes>().panes.entry(key.clone()).or_default()
}

fn request_list(app: &mut App, key: &PaneKey, reader: &Rc<dyn HistoryReader>) {
    let state = pane(app, key);
    state.last_read = Some(Instant::now());
    state.due = None;
    let generation = state.generation;
    let owner_key = key.clone();
    reader.list(
        app,
        &key.0,
        key.1.as_deref(),
        Box::new(move |app, result| {
            let state = pane(app, &owner_key);
            if state.generation != generation {
                return;
            }
            match result {
                Ok(next) => {
                    state.entries = Some(next);
                    state.failure = None;
                }
                Err(error) => state.failure = Some(error),
            }
            repaint();
        }),
    );
}

/// Reads one entry's body once.
fn load(app: &mut App, key: &PaneKey, seq: i64, reader: &Rc<dyn HistoryReader>) {
    let state = pane(app, key);
    if !state.requested.insert(seq) {
        return;
    }
    state.details.insert(seq, None);
    let owner_key = key.clone();
    reader.load(
        app,
        &key.0,
        seq,
        Box::new(move |app, result| {
            let state = pane(app, &owner_key);
            let failure = match result {
                Ok(Some(detail)) => {
                    state.details.insert(seq, Some(detail));
                    None
                }
                // An entry the store no longer holds has to be said out loud rather than left as a read that never ends.
                Ok(None) => Some(tf("第 {seq} 条记录已不在历史记录里。", "Entry {seq} is no longer in the history.", &[("seq", &seq)])),
                Err(error) => Some(error),
            };
            if let Some(message) = failure {
                state.requested.remove(&seq);
                state.details.remove(&seq);
                state.detail_errors.insert(seq, message);
            }
            repaint();
        }),
    );
}

fn retry(app: &mut App, key: &PaneKey, seq: i64, reader: &Rc<dyn HistoryReader>) {
    pane(app, key).detail_errors.remove(&seq);
    load(app, key, seq, reader);
}

fn predecessors_of(entries: &[HistoryEntry]) -> HashMap<i64, i64> {
    let mut map = HashMap::new();
    let mut previous = None;
    for entry in entries {
        let ty = entry.detail.get("type").and_then(Value::as_str);
        if entry.kind != "request" || matches!(ty, Some("search" | "fetch")) {
            continue;
        }
        if let Some(p) = previous {
            map.insert(entry.seq, p);
        }
        previous = Some(entry.seq);
    }
    map
}

/// The payloads a bar's system prompt is read from: its first model request, and the one before.
fn prompt_seqs(bar: &HistoryBar, predecessors: &HashMap<i64, i64>) -> Option<(i64, Option<i64>)> {
    let first = bar.requests.iter().find(|r| r.kind == "model")?;
    Some((first.seq, predecessors.get(&first.seq).copied()))
}

struct RowArgs<'a> {
    message: bool,
    fixed: bool,
    label: &'a str,
    detail: &'a str,
    badges: &'a [EventBadge],
    preview: &'a str,
    stat: Option<E>,
    title: Option<String>,
    hook_role: bool,
    /// Coloured across the whole row: `(ink, wash)`, a message put there or taken away whole.
    wash: Option<(Color, Color)>,
    accent_role: bool,
    open: bool,
    key: String,
}

fn tag(label: &str, wash: Color, ink: Color) -> E {
    div().px(4.0).radius(4.0).bg(wash).shrink0().child(text(label.to_owned()).ty(TAG).color(ink).nowrap())
}

fn badge(b: &EventBadge) -> E {
    let c = theme();
    let (wash, ink) = match b.tone {
        BadgeTone::Hook => (c.attention_bg, c.accent_text),
        BadgeTone::Warning => (c.caution_bg, c.caution),
        BadgeTone::Neutral => (c.card_secondary, c.text_tertiary),
    };
    div().px(4.0).radius(4.0).bg(wash).shrink0().child(text(b.label.clone()).ty(ts(9.5, 13.0, Weight::Regular)).color(ink).nowrap())
}

/// One single-line row that opens in place. It leads with its role: a glyph beside it would be a second name.
fn history_row(a: RowArgs<'_>, on_toggle: Option<Rc<dyn Fn(&mut App)>>) -> E {
    let c = theme();
    let (ink, muted) = match a.wash {
        Some((ink, _)) => (Some(ink), ink),
        None => (None, c.text_secondary),
    };
    let min_h = if a.message { 22.0 } else { 24.0 };
    let mut r = row().key(("history-row", a.key.as_str())).w_full().min_h(min_h).py(2.0).px(6.0).items_center().gap(6.0).radius(5.0).tooltip(a.title.clone().unwrap_or_default());
    if let Some((_, wash)) = a.wash {
        r = r.bg(wash);
    }
    if a.fixed {
        r = r.pl(24.0);
    } else {
        r = r.aria_expanded(a.open).child(icon("ChevronRight", 12.0).color(c.text_tertiary).rotate(if a.open { 90.0 } else { 0.0 }));
        if let Some(toggle) = on_toggle {
            r = r.on_click(move |app| toggle(app));
            if a.wash.is_none() {
                r = r.hover_bg(c.subtle_hover);
            }
        }
    }
    if !a.label.is_empty() {
        let (wash, tag_ink) = if a.wash.is_some() {
            (Color::TRANSPARENT, muted)
        } else if a.hook_role || a.accent_role {
            (c.attention_bg, c.accent_text)
        } else {
            (c.card_secondary, c.text_secondary)
        };
        r = r.child(tag(a.label, wash, tag_ink));
    }
    if !a.detail.is_empty() {
        r = r.child(text(a.detail.to_owned()).ty(SMALL).color(ink.unwrap_or(c.text_secondary)).ellipsis().with_style(|s| s.max_size.width = taffy::style::Dimension::percent(0.4)));
    }
    for b in a.badges {
        r = r.child(badge(b));
    }
    r = r.child(text(a.preview.to_owned()).ty(SMALL).color(ink.unwrap_or(c.text_tertiary)).ellipsis().flex1());
    r.child(a.stat)
}

/// What an opened row shows: its diff when it is a rewrite, its text otherwise. It is evidence, so it wraps rather than
/// scrolling sideways, and nothing about it is reformatted.
fn row_body(key: &str, patch: &str, body_text: &str, truncated: bool) -> E {
    let c = theme();
    if !patch.is_empty() {
        return col().key(("history-diff", key)).mt(2.0).mb(6.0).ml(18.0).max_h(420.0).scroll_y().child(mewrk_pane_git::diff_output(key, patch, None).mt(6.0).mx(10.0).mb(10.0));
    }
    let shown = if truncated { format!("{body_text}\n\n{}", t("（记录时已截断）", "(truncated when recorded)")) } else { body_text.to_owned() };
    col().key(("history-text", key)).mt(2.0).mb(6.0).ml(18.0).py(6.0).px(8.0).max_h(420.0).radius(5.0).bg(c.card_secondary).scroll_y().child(text(shown).ty(TEXT_TY).color(c.text_primary).selectable())
}

fn rewrite_stat(message: &MessageRow) -> Option<E> {
    if message.change != "replace" {
        return None;
    }
    let c = theme();
    Some(
        row()
            .gap(3.0)
            .shrink0()
            .tooltip(tf("新增 {added} 行，删除 {removed} 行", "{added} lines added, {removed} removed", &[("added", &message.additions), ("removed", &message.deletions)]))
            .child(text(format!("+{}", message.additions)).ty(SMALL).color(c.success).nowrap())
            .child(text(format!("−{}", message.deletions)).ty(SMALL).color(c.critical).nowrap()),
    )
}

struct Ctx<'a> {
    key: &'a PaneKey,
    open_parts: &'a HashSet<String>,
    reader: &'a Rc<dyn HistoryReader>,
    bar_kind: BarKind,
}

fn toggle_part(key: &PaneKey, part: String) -> Rc<dyn Fn(&mut App)> {
    let key = key.clone();
    Rc::new(move |app: &mut App| {
        let open = &mut pane(app, &key).open_parts;
        if !open.remove(&part) {
            open.insert(part.clone());
        }
        repaint();
    })
}

fn message_rows(rows: &[MessageRow], row_key: &str, cx: &Ctx<'_>, out: &mut Vec<E>) {
    let c = theme();
    for row in rows {
        let key = format!("{row_key}:{}", row.key);
        let open = cx.open_parts.contains(&key);
        // Green and red belong to what the user did by hand and to what went: a message put there or taken away whole.
        let wash = if row.change == "remove" {
            Some((c.critical, c.critical_bg))
        } else if row.change == "insert" && cx.bar_kind == BarKind::Edits {
            Some((c.success, c.success_bg))
        } else {
            None
        };
        out.push(history_row(
            RowArgs { message: true, fixed: row.fixed, label: row.label, detail: &row.detail, badges: &row.badges, preview: &row.preview, stat: rewrite_stat(row), title: None, hook_role: false, wash, accent_role: row.change == "replace", open, key: key.clone() },
            (!row.fixed).then(|| toggle_part(cx.key, key.clone())),
        ));
        if open && !row.fixed {
            out.push(row_body(&key, &row.patch, &row.text, false));
        }
    }
}

fn waiting(error: Option<String>, retry_key: String, on_retry: Option<Rc<dyn Fn(&mut App)>>) -> E {
    let c = theme();
    let Some(error) = error else {
        return text(t("正在读取…", "Reading…")).ty(ts(11.5, 15.0, Weight::Regular)).color(c.text_tertiary).py(14.0).px(12.0);
    };
    // Held where the entry would be, not over the pane: one unreadable entry must not hide the history around it.
    let mut r = row().wrap().items_center().gap(4.0).py(14.0).px(12.0).role(Role::Alert).child(text(error).ty(ts(11.5, 15.0, Weight::Regular)).color(c.text_tertiary));
    if let Some(on_retry) = on_retry {
        r = r.child(div().key(("history-retry", retry_key)).px(5.0).radius(4.0).border(c.card_stroke).hover_bg(c.subtle_hover).on_click(move |app| on_retry(app)).child(text(t("重试", "Retry")).ty(ts(10.5, 14.0, Weight::Regular)).color(c.text_secondary).nowrap()));
    }
    r
}

fn retry_handler(key: &PaneKey, seq: i64, reader: &Rc<dyn HistoryReader>) -> Rc<dyn Fn(&mut App)> {
    let (key, reader) = (key.clone(), reader.clone());
    Rc::new(move |app: &mut App| {
        retry(app, &key, seq, &reader);
        repaint();
    })
}

fn render_prompt(bar: &HistoryBar, state: &PaneState, predecessors: &HashMap<i64, i64>, labels: &EventLabels, cx: &Ctx<'_>, out: &mut Vec<E>) -> bool {
    let Some((first, before)) = prompt_seqs(bar, predecessors) else { return false };
    let failed = [Some(first), before].into_iter().flatten().find(|s| state.detail_errors.contains_key(s));
    if let Some(seq) = failed {
        out.push(waiting(state.detail_errors.get(&seq).cloned(), format!("{}:{seq}", bar.key), Some(retry_handler(cx.key, seq, cx.reader))));
        return true;
    }
    let after = state.details.get(&first).and_then(|d| d.as_ref());
    let previous = before.map(|b| state.details.get(&b).and_then(|d| d.as_ref()));
    let Some(after) = after else {
        out.push(waiting(None, String::new(), None));
        return true;
    };
    if previous == Some(None) {
        out.push(waiting(None, String::new(), None));
        return true;
    }
    let previous_parts = previous.flatten().and_then(|d| d.parts.as_deref());
    let current = after.parts.as_deref().unwrap_or_default();
    let part_key = format!("p{}", after.entry.seq);
    let mut rows = describe_tools(previous_parts, current, &part_key, labels);
    rows.extend(describe_prompts(previous_parts, current, &part_key, labels));
    message_rows(&rows, &bar.key, cx, out);
    true
}

fn render_body(bar: &HistoryBar, state: &PaneState, predecessors: &HashMap<i64, i64>, labels: &EventLabels, cx: &Ctx<'_>) -> E {
    let c = theme();
    let items = bar_items(bar);
    let mut out: Vec<E> = Vec::new();
    let has_prompt = render_prompt(bar, state, predecessors, labels, cx, &mut out);
    if items.is_empty() && !has_prompt {
        // An empty fold would read as a turn in which nothing happened.
        return text(t("这一轮除了发出的请求，没有记录到别的条目。", "Nothing but the requests it sent was recorded for this turn.")).ty(ts(11.5, 15.0, Weight::Regular)).color(c.text_tertiary).py(14.0).px(12.0);
    }
    for item in &items {
        match item {
            BarItem::Request(request) => {
                let seq = request.seq;
                let key = format!("{}:r{seq}", bar.key);
                let before = predecessors.get(&seq).copied();
                let after = state.details.get(&seq).and_then(|d| d.as_ref());
                let previous = before.map(|b| state.details.get(&b).and_then(|d| d.as_ref()));
                // Nothing until both payloads are read: most requests hand over no tool, and a placeholder for each
                // would flicker through a running turn.
                let (Some(after), true) = (after, previous != Some(None)) else { continue };
                let rows = describe_appended(previous.flatten().and_then(|d| d.parts.as_deref()), after.parts.as_deref().unwrap_or_default(), &format!("a{}", after.entry.seq), labels);
                message_rows(&rows, &key, cx, &mut out);
            }
            BarItem::Event(entry) => {
                let key = format!("{}:e{}", bar.key, entry.seq);
                let detail = state.details.get(&entry.seq).and_then(|d| d.as_ref());
                let error = state.detail_errors.get(&entry.seq).cloned();
                let described = describe_event(entry, detail, labels);
                let open = cx.open_parts.contains(&key);
                out.push(history_row(
                    RowArgs {
                        message: false,
                        fixed: false,
                        label: described.label,
                        detail: &described.detail,
                        badges: &described.badges,
                        preview: &described.preview,
                        stat: None,
                        title: Some(tf("第 {seq} 条记录", "Entry {seq}", &[("seq", &entry.seq)])),
                        hook_role: described.tone == EventTone::Hook,
                        wash: None,
                        accent_role: false,
                        open,
                        key: key.clone(),
                    },
                    Some(toggle_part(cx.key, key.clone())),
                ));
                if open {
                    out.push(match (error, detail) {
                        (Some(error), _) => waiting(Some(error), key.clone(), Some(retry_handler(cx.key, entry.seq, cx.reader))),
                        (None, None) => waiting(None, String::new(), None),
                        (None, Some(d)) => row_body(&key, &described.patch, &described.text, d.truncated),
                    });
                }
            }
            BarItem::Change(entry) => {
                let key = format!("{}:e{}", bar.key, entry.seq);
                match state.details.get(&entry.seq).and_then(|d| d.as_ref()) {
                    None => out.push(waiting(state.detail_errors.get(&entry.seq).cloned(), key, Some(retry_handler(cx.key, entry.seq, cx.reader)))),
                    Some(detail) => {
                        let rows = describe_change(entry, detail, labels);
                        message_rows(&rows, &key, cx, &mut out);
                    }
                }
            }
        }
    }
    col().gap(1.0).children(out)
}

fn usage_cell(usage: &HistoryUsage) -> E {
    let c = theme();
    if usage_is_empty(usage) {
        return text("—").ty(SMALL).color(c.text_tertiary).ellipsis().flex1().tooltip(t("没有记录到用量", "No usage was recorded"));
    }
    let shown = |v: Option<i64>| v.map_or("—".to_owned(), |n| n.to_string());
    let mut parts = String::new();
    if let Some(n) = usage.input_tokens {
        parts.push_str(&format!("↑{}", format_history_tokens(n)));
    }
    if let Some(n) = usage.cached_input_tokens {
        parts.push_str(&format!(" ⚡{}", format_history_tokens(n)));
    }
    if let Some(n) = usage.output_tokens {
        parts.push_str(&format!(" ↓{}", format_history_tokens(n)));
    }
    text(parts)
        .ty(SMALL)
        .color(c.text_tertiary)
        .ellipsis()
        .flex1()
        .tooltip(tf("输入 {in} · 其中缓存命中 {cache} · 输出 {out}", "Input {in} · of which cached {cache} · output {out}", &[("in", &shown(usage.input_tokens)), ("cache", &shown(usage.cached_input_tokens)), ("out", &shown(usage.output_tokens))]))
}

fn bar_row(bar: &HistoryBar, open: bool, key: &PaneKey) -> E {
    let c = theme();
    let model_name = || if bar.mixed_models { tf("{model} 等", "{model} and others", &[("model", &bar.model_id)]) } else { bar.model_id.clone() };
    let title = match bar.kind {
        BarKind::Edits => t("上下文编辑", "Context edits").to_owned(),
        BarKind::Interrupted => t("意外中断", "Interrupted").to_owned(),
        BarKind::Pending => t("尚未发出", "Not sent yet").to_owned(),
        BarKind::Turn => model_name(),
    };
    let tip = match bar.kind {
        BarKind::Edits => tf("手动改动上下文 {n} 次", "{n} changes made to the context by hand", &[("n", &bar.entries.len())]),
        BarKind::Pending => t("还没有请求带出去的内容", "What no request has carried yet").to_owned(),
        BarKind::Interrupted => t("最后一次请求没有等到回复", "The last request never got an answer back").to_owned(),
        BarKind::Turn => tf("发出 {count} 次请求", "{count} requests", &[("count", &bar.requests.len())]),
    };
    let glyph = match bar.kind {
        BarKind::Turn | BarKind::Pending => "MessageSquare",
        BarKind::Interrupted => "TriangleAlert",
        BarKind::Edits => "Pencil",
    };
    let interrupted = bar.kind == BarKind::Interrupted;
    let (kind_wash, kind_ink, title_ink) = if interrupted { (c.caution_bg, c.caution, c.caution) } else { (c.card_secondary, c.text_primary, c.text_secondary) };
    let ran = matches!(bar.kind, BarKind::Turn | BarKind::Interrupted);
    let (key, bar_key) = (key.clone(), bar.key.clone());
    let mut r = row()
        .key(("history-bar", bar.key.as_str()))
        .w_full()
        .min_h(24.0)
        .py(2.0)
        .px(6.0)
        .items_center()
        .gap(6.0)
        .radius(5.0)
        .hover_bg(c.subtle_hover)
        .tooltip(tip)
        .aria_expanded(open)
        .on_click(move |app| {
            let rows = &mut pane(app, &key).open_rows;
            if !rows.remove(&bar_key) {
                rows.insert(bar_key.clone());
            }
            repaint();
        })
        .child(icon("ChevronRight", 13.0).color(c.text_tertiary).rotate(if open { 90.0 } else { 0.0 }))
        .child(row().center().size(16.0, 16.0).radius(4.0).bg(kind_wash).shrink0().child(icon(glyph, 12.0).color(kind_ink)))
        // No number of its own: a bar's position in this list is not an entry's number.
        .child(text(title).ty(ts(11.5, 15.0, Weight::Semibold)).color(title_ink).ellipsis());
    if interrupted && !bar.model_id.is_empty() {
        r = r.child(text(model_name()).ty(SMALL).color(c.text_secondary).ellipsis());
    }
    r = r.child(if ran { usage_cell(&bar.usage) } else { div().flex1() });
    r.child(text(format_history_time(&bar.created_at)).ty(SMALL).color(c.text_tertiary).nowrap().shrink0())
}

/// The conversation's history. A turn the user started, a run of edits made by hand, and a turn cut off before its answer
/// each stand as a bar of their own.
pub fn history_pane(app: &mut App, props: HistoryPaneProps) -> E {
    let c = theme();
    let key: PaneKey = (props.conversation_id.clone(), props.owners.clone());
    let reader: Rc<dyn HistoryReader> = props.reader.clone().unwrap_or_else(|| Rc::new(HostHistoryReader));
    let _mounted = use_state(("history-pane-mounted", &key), || Mounted(key.clone()));
    for gone in GONE.with(|g| std::mem::take(&mut *g.borrow_mut())) {
        if gone != key {
            app.local::<HistoryPanes>().panes.remove(&gone);
        }
    }

    // ---- the list: refetched when the trunk moves, debounced, and polled while a turn runs.
    let now = Instant::now();
    let signal = (props.contexts_revision, props.streaming);
    let state = pane(app, &key);
    if state.seen != Some(signal) {
        state.seen = Some(signal);
        state.generation += 1;
        state.due = Some(now + DEBOUNCE);
    }
    if props.streaming && state.due.is_none() && state.last_read.is_none_or(|at| now.duration_since(at) >= STREAMING_POLL) {
        state.due = Some(now);
    }
    if state.due.is_some_and(|due| now >= due) {
        request_list(app, &key, &reader);
    }
    let wake = {
        let state = pane(app, &key);
        let due_in = state.due.map(|due| due.saturating_duration_since(now));
        let poll_in = props.streaming.then(|| state.last_read.map_or(Duration::ZERO, |at| STREAMING_POLL.saturating_sub(now.duration_since(at))));
        due_in.into_iter().chain(poll_in).min()
    };

    let entries = pane(app, &key).entries.clone();
    let ticker_el = wake.map(|d| ticker(d.as_secs_f32().max(0.02)));
    let state = pane(app, &key);
    if let Some(failure) = state.failure.clone() {
        return col().flex1().child(ticker_el).child(text(failure).key("history-failure").ty(ts(11.5, 15.0, Weight::Regular)).color(c.text_tertiary).py(14.0).px(12.0).role(Role::Alert));
    }
    let Some(entries) = entries else {
        return col().flex1().child(ticker_el).child(text(t("正在读取历史记录…", "Reading the history…")).key("history-loading").ty(ts(11.5, 15.0, Weight::Regular)).color(c.text_tertiary).py(14.0).px(12.0));
    };
    if entries.is_empty() {
        let message = if props.owners.is_some() { t("还没有历史记录，从它的第一次请求开始。", "Nothing recorded yet. Recording starts at its first request.") } else { t("还没有历史记录，从下一次发送或编辑开始。", "Nothing recorded yet. Recording starts at the next send or edit.") };
        return col().flex1().child(ticker_el).child(text(message).key("history-empty").ty(ts(11.5, 15.0, Weight::Regular)).color(c.text_tertiary).py(14.0).px(12.0));
    }

    let bars = history_bars(&entries, props.streaming);
    let predecessors = predecessors_of(&entries);
    // Every row here is a fold, so a pane that opened onto nothing but collapsed headers would hide what it was opened for.
    if !state.primed && !bars.is_empty() {
        state.primed = true;
        state.open_rows = bars.last().map(|b| b.key.clone()).into_iter().collect();
    }

    // Keeps every open bar's entries read. New entries join a running turn one by one, so this follows the open set
    // rather than the click. A failed read is left alone until the reader asks again: retrying here would turn one
    // unreadable entry into a request a second for as long as the turn runs.
    let open_rows = state.open_rows.clone();
    let errors: HashSet<i64> = state.detail_errors.keys().copied().collect();
    for bar in bars.iter().filter(|b| open_rows.contains(&b.key)) {
        let mut seqs: Vec<i64> = Vec::new();
        if let Some((first, before)) = prompt_seqs(bar, &predecessors) {
            seqs.push(first);
            seqs.extend(before);
        }
        for item in bar_items(bar) {
            match item {
                BarItem::Request(r) => {
                    seqs.push(r.seq);
                    seqs.extend(predecessors.get(&r.seq));
                }
                BarItem::Change(e) | BarItem::Event(e) => seqs.push(e.seq),
            }
        }
        for seq in seqs {
            if !errors.contains(&seq) {
                load(app, &key, seq, &reader);
            }
        }
    }

    let labels = event_labels();
    let state = std::mem::take(pane(app, &key));
    let mut list = col().gap(1.0);
    for bar in &bars {
        let open = state.open_rows.contains(&bar.key);
        let mut entry = col().child(bar_row(bar, open, &key));
        if open {
            let cx = Ctx { key: &key, open_parts: &state.open_parts, reader: &reader, bar_kind: bar.kind };
            entry = entry.child(col().mt(1.0).mb(4.0).ml(12.0).py(2.0).pl(8.0).border_left(c.divider).child(render_body(bar, &state, &predecessors, &labels, &cx)));
        }
        list = list.child(entry);
    }
    *pane(app, &key) = state;
    // The clock sits outside the scroller, so scrolling it out of view does not stop the polling.
    col().flex1().min_w(0.0).child(ticker_el).child(col().key("history-pane").flex1().scroll_y().pt(4.0).px(6.0).pb(10.0).child(list))
}

/// A pane's state, for a test or a shell that wants to know what is open.
pub fn pane_state<'a>(app: &'a mut App, conversation_id: &str, owners: Option<Vec<String>>) -> &'a mut PaneState {
    pane(app, &(conversation_id.to_owned(), owners))
}
