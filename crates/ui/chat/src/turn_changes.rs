//! `components/TurnChanges.tsx`: the files one span of work changed, at its end.
//!
//! The heading folds the whole list; "Show N more" is its own choice and outlives the fold, so a list opened in full comes
//! back in full. A row opens its file where it can be read: the review pane when Git tracks the change, otherwise the file pane.

use std::collections::HashMap;

use fluent::{TypeStyle, Weight};
use mewrk_client::model::ContextItem;
use mewrk_client::prelude::*;
use mewrk_pane_files::icons::file_icon;
use mewrk_pane_files::links::PathOpenRequest;
use mewrk_pane_files::path_open::open_path;
use mewrk_pane_git::diff_output::parse_unified_diff_lines;
use serde_json::Value;

use crate::item::Item;
use crate::kit::{retained_flag, set_retained_flag};
use crate::orchestration::is_structured_output_nudge;

/// Rows a long list shows before "Show N more".
const COLLAPSED_ROW_COUNT: usize = 3;

/// `.turn-changes__row`.
const ROW: TypeStyle = TypeStyle { size: 11.25, line: 17.0, weight: Weight::Regular, mono: false, italic: false, serif: false };
/// `.turn-changes__heading`.
const HEADING: TypeStyle = TypeStyle { size: 11.75, line: 17.0, weight: Weight::Semibold, mono: false, italic: false, serif: false };
/// `.turn-changes__directory`.
const DIRECTORY: TypeStyle = TypeStyle { size: 10.5, line: 16.0, weight: Weight::Regular, mono: false, italic: false, serif: false };
/// `.turn-changes__counts`.
const COUNTS: TypeStyle = TypeStyle { size: 10.0, line: 14.0, weight: Weight::Regular, mono: true, italic: false, serif: false };

/// A stretch of finished work whose file changes are summed at its end: what follows one of the user's messages, or one run of
/// a subagent. `context_ids` are the contexts it owns, in any order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSpan {
    pub key: String,
    pub context_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnFileChange {
    pub key: String,
    /// As the first call that changed it wrote it: workspace-relative or absolute.
    pub path: String,
    /// The workspace number the calls named, or `None` for the surface's own.
    pub workspace: Option<u32>,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnChangeSummary {
    pub key: String,
    /// In the order the span first touched them.
    pub files: Vec<TurnFileChange>,
    pub additions: u32,
    pub deletions: u32,
    /// Changes exactly when something the card shows does.
    pub signature: String,
}

/// A call that changed a file: it succeeded and carries the diff it made. The path it wrote, and that diff.
fn file_change_of<'a>(item: &Item<'a>) -> Option<(&'a str, &'a str, Option<u32>)> {
    let tool = item.tool()?;
    if !tool.result.success {
        return None;
    }
    let diff = tool.result.diff.as_deref().filter(|diff| !diff.is_empty())?;
    let path = tool.input.get("path")?.as_str().filter(|path| !path.trim().is_empty())?;
    Some((path, diff, workspace_of(tool.input.get("workspace"))))
}

fn workspace_of(value: Option<&Value>) -> Option<u32> {
    let number = value?.as_f64()?;
    (number.fract() == 0.0 && number > 0.0 && number <= u32::MAX as f64).then_some(number as u32)
}

/// Folds the spellings of one path a model is likely to mix within a turn.
fn path_key(path: &str) -> String {
    let key = path.trim().replace('\\', "/");
    let mut rest = key.as_str();
    while let Some(next) = rest.strip_prefix("./") {
        rest = next;
    }
    rest.to_owned()
}

/// Every file the span's calls changed, with their line counts summed.
///
/// Summing is exact for separate hunks and an upper bound where one edit rewrites another's lines; the calls carry no
/// whole-file snapshots to diff end to end.
pub fn summarize_span_changes<'a>(span: &ChangeSpan, context_for: impl Fn(&str) -> Option<Item<'a>>) -> Option<TurnChangeSummary> {
    let mut files: Vec<TurnFileChange> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for id in &span.context_ids {
        let Some(context) = context_for(id) else { continue };
        let Some((path, diff, workspace)) = file_change_of(&context) else { continue };
        let key = format!("{}:{}", workspace.unwrap_or(1), path_key(path));
        let parsed = parse_unified_diff_lines(diff);
        let (additions, deletions) = (parsed.additions as u32, parsed.deletions as u32);
        if let Some(&index) = at.get(&key) {
            files[index].additions += additions;
            files[index].deletions += deletions;
        } else {
            at.insert(key.clone(), files.len());
            files.push(TurnFileChange { key, path: path.to_owned(), workspace, additions, deletions });
        }
    }
    if files.is_empty() {
        return None;
    }
    let rows: Vec<Value> = files.iter().map(|file| json!([file.key, file.path, file.additions, file.deletions])).collect();
    Some(TurnChangeSummary {
        key: span.key.clone(),
        additions: files.iter().map(|file| file.additions).sum(),
        deletions: files.iter().map(|file| file.deletions).sum(),
        signature: Value::Array(rows).to_string(),
        files,
    })
}

/// The stretches of a timeline between the user's messages, each listed at its end: everything after one message the user
/// wrote, up to the next.
///
/// Read off the contexts as they stand rather than off the turn records, which remember what a round produced, not where it now
/// sits. Edited (a message inserted into the middle of a round, one deleted between two rounds, a call placed by hand after a
/// reply) a list still sums exactly the calls between it and the nearest user message above it.
///
/// `is_user_message` names the messages the user wrote. `open` withholds the last stretch, which a running round is still writing.
pub fn message_change_spans<'a>(contexts: &[Item<'a>], is_user_message: impl Fn(&Item<'a>) -> bool, open: bool) -> Vec<ChangeSpan> {
    let mut spans = Vec::new();
    let mut opener = "start".to_owned();
    let mut current: Vec<String> = Vec::new();
    for item in contexts {
        if is_user_message(item) {
            if !current.is_empty() {
                spans.push(ChangeSpan { key: format!("after:{opener}"), context_ids: std::mem::take(&mut current) });
            }
            opener = item.id().to_owned();
            continue;
        }
        current.push(item.id().to_owned());
    }
    if !current.is_empty() && !open {
        spans.push(ChangeSpan { key: format!("after:{opener}"), context_ids: current });
    }
    spans
}

/// Whether `item` records the schema-bound result that ends a child's run on the spot.
fn is_structured_result(item: &ContextItem) -> bool {
    matches!(item, ContextItem::Tool { tool_name, result, .. } if tool_name == "structured_output" && result.success)
}

/// A subagent transcript's runs, which it has no turn records for.
///
/// A message opens a run when the work before it had stopped. A transcript holds such messages whenever the main agent's
/// `send_message` reached the child: a mailbox message that landed mid-run arrived between tool batches, so it follows a call,
/// while one that woke an idle child follows the reply its last run ended on. The reminder a schema-bound child gets is part of
/// the run it interrupts.
///
/// `live` withholds the last run, which is still writing its list, unless it already returned its structured result: the host
/// ends such a run the moment the result validates, whatever the child's status still says.
pub fn subagent_change_spans(contexts: &[&ContextItem], live: bool) -> Vec<ChangeSpan> {
    let mut spans: Vec<(ChangeSpan, bool)> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut settled = false;
    let mut previous: Option<&ContextItem> = None;
    let close = |current: &mut Vec<String>, settled: &mut bool, spans: &mut Vec<(ChangeSpan, bool)>| {
        if let Some(first) = current.first() {
            let key = format!("run:{first}");
            spans.push((ChangeSpan { key, context_ids: std::mem::take(current) }, *settled));
        }
        *settled = false;
    };
    for &item in contexts {
        if matches!(item, ContextItem::User { .. }) && !is_structured_output_nudge(item) && !matches!(previous, Some(ContextItem::Tool { .. })) {
            close(&mut current, &mut settled, &mut spans);
        }
        current.push(item.id().to_owned());
        if is_structured_result(item) {
            settled = true;
        }
        // Reasoning and empty protocol replies sit between a call and the message after it without ending anything.
        let empty_reply = matches!(item, ContextItem::Assistant { content, .. } if content.is_empty());
        if !matches!(item, ContextItem::Reasoning { .. }) && !empty_reply {
            previous = Some(item);
        }
    }
    close(&mut current, &mut settled, &mut spans);
    if live && spans.last().is_some_and(|(_, settled)| !settled) {
        spans.pop();
    }
    spans.into_iter().map(|(span, _)| span).collect()
}

/// A path's last segment, and the directory before it.
pub fn split_path(path: &str) -> (String, String) {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        None => (trimmed.to_owned(), String::new()),
        Some(cut) => {
            let name = &trimmed[cut + 1..];
            (if name.is_empty() { trimmed.to_owned() } else { name.to_owned() }, trimmed[..cut].to_owned())
        }
    }
}

/// `−` is the typographic minus the review pane uses; a hyphen reads as a dash. `margin-left: auto` here would lose the row its `gap`:
/// taffy applies a row's gap only when no auto margin takes the free space, so a `spacer` stands before it instead, and the padding is
/// what a tight row has left of upstream's ten once the spacer's own gap is counted.
fn counts(additions: u32, deletions: u32) -> E {
    let t_ = theme();
    row()
        .pl(2.0)
        .gap(4.0)
        .shrink0()
        .items_center()
        .child(text(format!("+{additions}")).ty(COUNTS).color(t_.success).nowrap())
        .child(text(format!("\u{2212}{deletions}")).ty(COUNTS).color(t_.critical).nowrap())
}

/// `.turn-changes__row`, which is a button in every form it takes.
fn row_base(key: impl std::hash::Hash) -> E {
    let t_ = theme();
    row().key(key).items_center().gap(8.0).min_h(32.0).pl(10.0).pr(8.0).min_w(0.0).w_full().hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed)
}

/// The heading's words.
pub fn turn_changes_title(count: usize) -> String {
    if count == 1 { t("编辑了 1 个文件", "Edited 1 file").to_owned() } else { tf("编辑了 {count} 个文件", "Edited {count} files", &[("count", &count)]) }
}

/// What a click on a row asks for. A path written against another workspace does not resolve against this surface's directory;
/// the app knows where it is.
pub fn row_open_request(file: &TurnFileChange, path_base_dir: Option<&str>) -> PathOpenRequest {
    let own = file.workspace.is_none() || file.workspace == Some(1);
    PathOpenRequest { path: file.path.clone(), base_dir: if own { path_base_dir.map(str::to_owned) } else { None }, line: None, workspace: file.workspace, review: true, ..PathOpenRequest::default() }
}

pub fn turn_changes(summary: &TurnChangeSummary, path_base_dir: Option<&str>) -> E {
    let t_ = theme();
    let id = summary.key.as_str();
    let (open_key, all_key) = (hash_key(("turn-changes-open", id)), hash_key(("turn-changes-all", id)));
    let is_open = retained_flag(open_key, true);
    let all = retained_flag(all_key, false);
    let count = summary.files.len();
    // One more row costs the same space as the row that would offer it.
    let foldable = count > COLLAPSED_ROW_COUNT + 1;
    let shown = if foldable && !all { &summary.files[..COLLAPSED_ROW_COUNT] } else { &summary.files[..] };
    // A row reads as its file name; only names the list holds twice say where they are.
    let mut names: HashMap<String, usize> = HashMap::new();
    for file in &summary.files {
        *names.entry(split_path(&file.path).0).or_insert(0) += 1;
    }

    let heading = row_base(("turn-changes-heading", id))
        .aria_expanded(is_open)
        .on_click(move |_| set_retained_flag(open_key, !is_open))
        .child(icon("FileDiff", 14.0).color(t_.text_secondary).shrink0())
        .child(text(turn_changes_title(count)).ty(HEADING).color(t_.text_secondary).ellipsis())
        .child(spacer())
        .child(counts(summary.additions, summary.deletions))
        .child(icon("ChevronRight", 14.0).color(t_.text_tertiary).shrink0().rotate(if is_open { 90.0 } else { 0.0 }));

    let list = is_open.then(|| {
        let rows = shown.iter().map(|file| {
            let (name, directory) = split_path(&file.path);
            let repeated = names.get(&name).copied().unwrap_or(0) > 1;
            let request = row_open_request(file, path_base_dir);
            row_base(("turn-changes-file", id, file.key.as_str()))
                .border_top(t_.card_stroke)
                .full_text(file.path.clone())
                .on_click(move |app: &mut App| {
                    open_path(app, &request);
                })
                .child(file_icon(&file.path, 14.0))
                // The name keeps its room; the directory beside a repeated name gives way first.
                .child(text(name).ty(ROW).color(t_.text_secondary).ellipsis().shrink0().with_style(|s| s.max_size.width = fluent_percent(0.6)))
                .child((!directory.is_empty() && repeated).then(|| text(directory.clone()).key(("turn-changes-directory", id, file.key.as_str())).ty(DIRECTORY).color(t_.text_tertiary).ellipsis().with_style(|s| s.flex_shrink = 1.0)))
                .child(spacer())
                .child(counts(file.additions, file.deletions))
                .child(icon("ChevronRight", 14.0).color(t_.text_tertiary).shrink0())
        });
        let more = foldable.then(|| {
            let label = if all { t("收起", "Show less").to_owned() } else { tf("再显示 {count} 个", "Show {count} more", &[("count", &(count - COLLAPSED_ROW_COUNT))]) };
            row_base(("turn-changes-more", id))
                .border_top(t_.card_stroke)
                .aria_expanded(all)
                .on_click(move |_| set_retained_flag(all_key, !all))
                .child(text(label).ty(ROW).color(t_.text_secondary).ellipsis())
                .child(spacer())
                .child(icon("ChevronRight", 14.0).color(t_.text_tertiary).shrink0().rotate(if all { -90.0 } else { 0.0 }))
        });
        col().children(rows).child(more)
    });

    col().key(("turn-changes", id)).mt(12.0).clip().border(t_.card_stroke).radius(8.0).bg(t_.card).when(a11y_active(), |e| e.role(Role::Region).aria_label(turn_changes_title(count))).child(heading).child(list)
}

fn fluent_percent(fraction: f32) -> taffy::style::Dimension {
    taffy::style::Dimension::percent(fraction)
}
