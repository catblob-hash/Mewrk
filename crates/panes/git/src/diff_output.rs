//! `DiffOutput`: a read-only diff card for a file result, in chat and in the history pane. It parses the subset of
//! unified diff syntax it needs on its own, because what it is given has no `diff --git` header.

use std::sync::OnceLock;

use mewrk_client::prelude::*;
use regex::Regex;

use crate::code_table::{table_leaf, CodeTable, TableRow, Tone};
use crate::diff_highlight::{code_language, highlight_diff_lines, CodeToken, DiffSide};
use crate::diff_view::file_icon;
use crate::word_diff::{word_diff, WordDiffSegment};

/// `.diff-output__header .diff-output__path`.
const PATH_TY: TypeStyle = TypeStyle { size: 9.75, line: 13.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
/// `.diff-output__stat`.
const STAT_TY: TypeStyle = TypeStyle { size: 8.5, line: 11.0, weight: Weight::Semibold, mono: true, italic: false, serif: false };
/// `.diff-output__empty`.
const EMPTY_TY: TypeStyle = TypeStyle { size: 10.0, line: 14.0, weight: Weight::Regular, mono: false, italic: false, serif: false };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffOutputLineKind {
    File,
    Hunk,
    Context,
    Addition,
    Deletion,
    Meta,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedDiffLine {
    pub kind: DiffOutputLineKind,
    pub text: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ParsedUnifiedDiff {
    pub lines: Vec<ParsedDiffLine>,
    pub additions: usize,
    pub deletions: usize,
    pub path: Option<String>,
}

const MAX_RENDERED_LINES: usize = 800;
const HEAD_LINES: usize = 500;
const TAIL_LINES: usize = 250;

fn diff_path(header: &str) -> Option<String> {
    let raw = header.get(4..)?.split('\t').next()?.trim();
    if raw.is_empty() || raw == "/dev/null" {
        return None;
    }
    Some(if raw.starts_with("a/") || raw.starts_with("b/") { raw[2..].to_owned() } else { raw.to_owned() })
}

fn hunk_header() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$").expect("valid"))
}

/// Parses the subset of unified diff syntax needed for a read-only file result view.
pub fn parse_unified_diff_lines(value: &str) -> ParsedUnifiedDiff {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let mut raw_lines: Vec<&str> = normalized.split('\n').collect();
    if raw_lines.last() == Some(&"") {
        raw_lines.pop();
    }
    let mut out = ParsedUnifiedDiff::default();
    let (mut old_line, mut new_line, mut in_hunk) = (0u32, 0u32, false);
    for raw in raw_lines {
        let line = |kind, text: &str, old_line, new_line| ParsedDiffLine { kind, text: text.to_owned(), old_line, new_line };
        if let Some(hunk) = hunk_header().captures(raw) {
            old_line = hunk[1].parse().unwrap_or(0);
            new_line = hunk[2].parse().unwrap_or(0);
            in_hunk = true;
            out.lines.push(line(DiffOutputLineKind::Hunk, raw, None, None));
        } else if !in_hunk && (raw.starts_with("--- ") || raw.starts_with("+++ ")) {
            if let Some(path) = diff_path(raw) {
                out.path = Some(path);
            }
            out.lines.push(line(DiffOutputLineKind::File, raw, None, None));
        } else if in_hunk && raw.starts_with('+') {
            out.additions += 1;
            out.lines.push(line(DiffOutputLineKind::Addition, &raw[1..], None, Some(new_line)));
            new_line += 1;
        } else if in_hunk && raw.starts_with('-') {
            out.deletions += 1;
            out.lines.push(line(DiffOutputLineKind::Deletion, &raw[1..], Some(old_line), None));
            old_line += 1;
        } else if in_hunk && raw.starts_with(' ') {
            out.lines.push(line(DiffOutputLineKind::Context, &raw[1..], Some(old_line), Some(new_line)));
            old_line += 1;
            new_line += 1;
        } else {
            out.lines.push(line(DiffOutputLineKind::Meta, raw, None, None));
        }
    }
    out
}

/// Keeps the head and the tail of a long diff, and says how much of the middle went.
pub fn limit_lines(lines: Vec<ParsedDiffLine>) -> Vec<ParsedDiffLine> {
    if lines.len() <= MAX_RENDERED_LINES {
        return lines;
    }
    let omitted = lines.len() - HEAD_LINES - TAIL_LINES;
    let mut out = Vec::with_capacity(HEAD_LINES + TAIL_LINES + 1);
    out.extend_from_slice(&lines[..HEAD_LINES]);
    out.push(ParsedDiffLine { kind: DiffOutputLineKind::Meta, text: tf("… {count} 行差异未显示 …", "… {count} diff lines omitted …", &[("count", &omitted)]), old_line: None, new_line: None });
    out.extend_from_slice(&lines[lines.len() - TAIL_LINES..]);
    out
}

/// The one number a row carries: a deletion exists only in the old file, everything else is addressed in the new one.
pub fn displayed_line_number(line: &ParsedDiffLine) -> Option<u32> {
    if line.kind == DiffOutputLineKind::Deletion { line.old_line } else { line.new_line }
}

/// Reads a replacement as "what was there, then what replaced it": each run of changed lines is regrouped with its
/// deletions first, whatever order the producer emitted them in. Context, hunk and file rows end a run, so nothing moves
/// across a boundary.
pub fn deletions_before_additions(lines: &[ParsedDiffLine]) -> Vec<ParsedDiffLine> {
    let mut ordered = Vec::with_capacity(lines.len());
    let mut run: Vec<&ParsedDiffLine> = Vec::new();
    let flush = |run: &mut Vec<&ParsedDiffLine>, ordered: &mut Vec<ParsedDiffLine>| {
        for kind in [DiffOutputLineKind::Deletion, DiffOutputLineKind::Addition] {
            ordered.extend(run.iter().filter(|l| l.kind == kind).map(|l| (*l).clone()));
        }
        run.clear();
    };
    for line in lines {
        if matches!(line.kind, DiffOutputLineKind::Addition | DiffOutputLineKind::Deletion) {
            run.push(line);
            continue;
        }
        flush(&mut run, &mut ordered);
        ordered.push(line.clone());
    }
    flush(&mut run, &mut ordered);
    ordered
}

/// File and hunk headers are diff transport, not content, so the card drops them.
pub fn is_rendered_kind(kind: DiffOutputLineKind) -> bool {
    !matches!(kind, DiffOutputLineKind::File | DiffOutputLineKind::Hunk)
}

/// The words that changed within a replaced line. A run's removals and additions are paired in order, as the review pane
/// pairs them; a line with no counterpart has nothing to be compared with. `lines` are as drawn, each run's removals first.
pub fn changed_words(lines: &[ParsedDiffLine]) -> Vec<Option<Vec<WordDiffSegment>>> {
    let mut segments = vec![None; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        if lines[index].kind != DiffOutputLineKind::Deletion {
            index += 1;
            continue;
        }
        let mut additions = index;
        while additions < lines.len() && lines[additions].kind == DiffOutputLineKind::Deletion {
            additions += 1;
        }
        let mut end = additions;
        while end < lines.len() && lines[end].kind == DiffOutputLineKind::Addition {
            end += 1;
        }
        for pair in 0..(additions - index).min(end - additions) {
            let result = word_diff(&lines[index + pair].text, &lines[additions + pair].text);
            if result.bailed {
                continue;
            }
            segments[index + pair] = Some(result.before);
            segments[additions + pair] = Some(result.after);
        }
        index = end;
    }
    segments
}

/// Every hunk's lines coloured as the code they are, in the order the file has them. A hunk or file row ends an excerpt, so
/// nothing left open in one hunk colours the next.
fn highlight_parsed(path: &str, lines: &[ParsedDiffLine]) -> Vec<Option<Vec<CodeToken>>> {
    let lang = code_language(path);
    let mut out = vec![None; lines.len()];
    let mut start = 0;
    for end in 0..=lines.len() {
        let code = lines.get(end).is_some_and(|l| matches!(l.kind, DiffOutputLineKind::Context | DiffOutputLineKind::Addition | DiffOutputLineKind::Deletion));
        if code {
            continue;
        }
        let excerpt: Vec<(DiffSide, &str)> = lines[start..end]
            .iter()
            .map(|l| {
                let side = match l.kind {
                    DiffOutputLineKind::Addition => DiffSide::New,
                    DiffOutputLineKind::Deletion => DiffSide::Old,
                    _ => DiffSide::Both,
                };
                (side, l.text.as_str())
            })
            .collect();
        for (i, tokens) in highlight_diff_lines(lang, &excerpt).into_iter().enumerate() {
            out[start + i] = tokens;
        }
        start = end + 1;
    }
    out
}

/// Which parsed line each drawn row came from, in drawn order: `deletions_before_additions`, with `limit_lines`'s gap as `None`.
fn drawn_order(lines: &[ParsedDiffLine]) -> Vec<Option<usize>> {
    let mut ordered: Vec<usize> = Vec::with_capacity(lines.len());
    let mut run: Vec<usize> = Vec::new();
    let flush = |run: &mut Vec<usize>, ordered: &mut Vec<usize>| {
        for kind in [DiffOutputLineKind::Deletion, DiffOutputLineKind::Addition] {
            ordered.extend(run.iter().copied().filter(|&i| lines[i].kind == kind));
        }
        run.clear();
    };
    for (i, line) in lines.iter().enumerate() {
        if matches!(line.kind, DiffOutputLineKind::Addition | DiffOutputLineKind::Deletion) {
            run.push(i);
            continue;
        }
        flush(&mut run, &mut ordered);
        if is_rendered_kind(line.kind) {
            ordered.push(i);
        }
    }
    flush(&mut run, &mut ordered);
    if ordered.len() <= MAX_RENDERED_LINES {
        return ordered.into_iter().map(Some).collect();
    }
    let head = ordered[..HEAD_LINES].iter().copied().map(Some);
    let tail = ordered[ordered.len() - TAIL_LINES..].iter().copied().map(Some);
    head.chain(std::iter::once(None)).chain(tail).collect()
}

/// The rows the card draws, with its header's facts.
pub struct DiffOutputModel {
    pub path: Option<String>,
    pub additions: usize,
    pub deletions: usize,
    pub rendered: Vec<ParsedDiffLine>,
    pub table: CodeTable,
}

pub fn diff_output_model(value: &str, path: Option<&str>) -> DiffOutputModel {
    let parsed = parse_unified_diff_lines(value);
    let rendered = limit_lines(deletions_before_additions(&parsed.lines).into_iter().filter(|l| is_rendered_kind(l.kind)).collect());
    let file_path = path.filter(|p| !p.is_empty()).map(str::to_owned).or(parsed.path.clone());
    let highlighted = highlight_parsed(file_path.as_deref().unwrap_or(""), &parsed.lines);
    let tokens: Vec<Option<Vec<CodeToken>>> = drawn_order(&parsed.lines).into_iter().map(|at| at.and_then(|i| highlighted[i].clone())).collect();
    let words = changed_words(&rendered);
    let rows = rendered
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let tone = match line.kind {
                DiffOutputLineKind::Addition => Tone::Addition,
                DiffOutputLineKind::Deletion => Tone::Deletion,
                DiffOutputLineKind::Context => Tone::Context,
                _ => return TableRow::Note(line.text.clone()),
            };
            TableRow::Line { tone, number: displayed_line_number(line), cell: crate::code_table::cell_for_output(&line.text, tokens[i].as_deref(), words[i].as_deref()) }
        })
        .collect();
    DiffOutputModel { path: file_path, additions: parsed.additions, deletions: parsed.deletions, rendered, table: CodeTable::of_card_rows(rows) }
}

/// The card.
pub fn diff_output(key: impl std::hash::Hash, value: &str, path: Option<&str>) -> E {
    let t_ = theme();
    let id = hash_key(("diff-output", key, value, path));
    let state = use_state(("diff-output-model", id), || diff_output_model(value, path));
    let (file_path, additions, deletions, empty) = state.with(|m| (m.path.clone(), m.additions, m.deletions, m.rendered.is_empty()));
    let display = file_path.clone().unwrap_or_else(|| t("文件", "File").to_owned());
    let region = a11y_active().then(|| tf("{path} 文件差异", "{path} file diff", &[("path", &display)]));
    let stat = |value: String, color: Color, ground: Color| row().center().min_w(25.0).px(5.0).py(2.0).radius(4.0).bg(ground).child(text(value).ty(STAT_TY).color(color).nowrap());
    let stats = row()
        .gap(4.0)
        .shrink0()
        .role(Role::Label)
        .aria_label(tf("新增 {additions} 行，删除 {deletions} 行", "{additions} lines added, {deletions} lines deleted", &[("additions", &additions), ("deletions", &deletions)]))
        .child(stat(format!("+{additions}"), t_.success, t_.success_bg))
        .child(stat(format!("−{deletions}"), t_.critical, t_.critical_bg));
    let mut header: E = row().items_center().gap(8.0).min_h(31.0).pl(10.0).pr(8.0).py(5.0).shrink0().border_bottom(t_.divider);
    if let Some(p) = &file_path {
        header = header.child(file_icon(p, 13.0));
    }
    header = header.child(crate::path::path_text(display, "", PATH_TY, None).flex1()).child(stats);
    let body: E = if empty {
        text(t("没有行级变化", "No line-level changes")).ty(EMPTY_TY).color(t_.text_tertiary).text_center().py(16.0).px(10.0)
    } else {
        let table = use_state(("diff-output-table", id), || state.with(|m| m.table.clone()));
        // Lines keep their shape and the card its height: the body scrolls both ways.
        col().key(("diff-output-body", id)).max_h(240.0).scroll_y().bg(t_.code_bg).child(table_leaf(id, table, false, false, false, None, None))
    };
    let card = col().radius(8.0).border(t_.card_stroke).clip().child(header).child(body);
    match region {
        Some(label) => card.role(Role::Region).aria_label(label),
        None => card,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "--- a/src/example.ts\n+++ b/src/example.ts\n@@ -2,3 +2,4 @@\n keep();\n-oldValue();\n+newValue();\n+extraValue();\n done();\n";

    #[test]
    fn parses_line_kinds_paths_counts_and_line_numbers() {
        let parsed = parse_unified_diff_lines(SAMPLE);
        assert_eq!(parsed.path.as_deref(), Some("src/example.ts"));
        assert_eq!((parsed.additions, parsed.deletions), (2, 1));
        let deletion = parsed.lines.iter().find(|l| l.kind == DiffOutputLineKind::Deletion).unwrap();
        assert_eq!((deletion.text.as_str(), deletion.old_line, deletion.new_line), ("oldValue();", Some(3), None));
        let added: Vec<_> = parsed.lines.iter().filter(|l| l.kind == DiffOutputLineKind::Addition).map(|l| (l.text.as_str(), l.new_line)).collect();
        assert_eq!(added, [("newValue();", Some(3)), ("extraValue();", Some(4))]);
    }

    #[test]
    fn carries_one_line_number_per_row() {
        let model = diff_output_model(SAMPLE, None);
        let numbers: Vec<_> = model.rendered.iter().map(displayed_line_number).collect();
        assert_eq!(numbers, [Some(2), Some(3), Some(3), Some(4), Some(5)]);
    }

    #[test]
    fn groups_each_run_of_changes_with_its_deletions_above_its_additions() {
        let parsed = parse_unified_diff_lines("@@ -1,2 +1,2 @@\n+new one\n-old one\n+new two\n-old two\n same\n");
        let ordered: Vec<_> = deletions_before_additions(&parsed.lines).into_iter().filter(|l| is_rendered_kind(l.kind)).map(|l| l.text).collect();
        assert_eq!(ordered, ["old one", "old two", "new one", "new two", "same"]);
    }

    #[test]
    fn does_not_confuse_changed_content_beginning_with_diff_header_markers() {
        let parsed = parse_unified_diff_lines("--- a/x\n+++ b/x\n@@ -1 +1 @@\n--- a removed rule\n+++ an added rule\n");
        let texts: Vec<_> = parsed.lines.iter().filter(|l| matches!(l.kind, DiffOutputLineKind::Addition | DiffOutputLineKind::Deletion)).map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["-- a removed rule", "++ an added rule"]);
    }

    #[test]
    fn drops_file_and_hunk_rows_and_keeps_meta_and_generated_omission_rows() {
        let parsed = parse_unified_diff_lines("--- a/x\n+++ b/x\n@@ -1 +1 @@\n a\n\\ No newline at end of file\n");
        let kept: Vec<_> = parsed.lines.iter().filter(|l| is_rendered_kind(l.kind)).map(|l| l.kind).collect();
        assert_eq!(kept, [DiffOutputLineKind::Context, DiffOutputLineKind::Meta]);
        let long: String = std::iter::once("@@ -1,900 +1,900 @@\n".to_owned()).chain((0..900).map(|i| format!(" line {i}\n"))).collect();
        let rendered = diff_output_model(&long, None).rendered;
        assert_eq!(rendered.len(), HEAD_LINES + TAIL_LINES + 1);
        assert_eq!(rendered[HEAD_LINES].kind, DiffOutputLineKind::Meta);
        assert_eq!(rendered[HEAD_LINES].text, "… 150 行差异未显示 …");
    }

    #[test]
    fn marks_only_the_words_that_changed_in_a_replaced_line_and_none_in_a_lone_addition() {
        let parsed = parse_unified_diff_lines("@@ -1 +1,2 @@\n-const value = 1;\n+const other = 1;\n+extra();\n");
        let lines: Vec<_> = deletions_before_additions(&parsed.lines).into_iter().filter(|l| is_rendered_kind(l.kind)).collect();
        let words = changed_words(&lines);
        assert!(words[0].as_ref().unwrap().iter().any(|s| s.changed && s.text == "value"));
        assert!(words[1].as_ref().unwrap().iter().any(|s| s.changed && s.text == "other"));
        assert!(words[2].is_none());
    }

    #[test]
    fn nothing_left_open_in_one_hunk_colours_the_next() {
        let patch = "--- a/x.ts\n+++ b/x.ts\n@@ -1,2 +1,2 @@\n-const a = 1;\n+/* never closed\n keep\n@@ -9 +9 @@\n+const b = 2;\n";
        let model = diff_output_model(patch, None);
        let last = model.table.rows.last().expect("a row");
        let TableRow::Line { cell, .. } = last else { panic!("a line") };
        assert_eq!(cell.text, "const b = 2;");
        assert_eq!(crate::code_table::kind_at(cell, 0), mewrk_panes::highlight::Token::Keyword, "the second hunk reads as code, not as the comment the first left open");
    }

    #[test]
    fn a_long_diff_keeps_each_rows_colours_past_the_gap() {
        let body: String = (0..900).map(|i| format!("+const v{i} = {i};\n")).collect();
        let model = diff_output_model(&format!("--- a/x.ts\n+++ b/x.ts\n@@ -0,0 +1,900 @@\n{body}"), None);
        assert_eq!(model.table.rows.len(), HEAD_LINES + TAIL_LINES + 1);
        let TableRow::Line { cell, .. } = model.table.rows.last().expect("a row") else { panic!("a line") };
        assert_eq!(cell.text, "const v899 = 899;");
        assert_eq!(crate::code_table::kind_at(cell, 0), mewrk_panes::highlight::Token::Keyword);
    }

    #[test]
    fn an_empty_diff_has_nothing_to_draw() {
        assert!(diff_output_model("", None).rendered.is_empty());
    }
}
