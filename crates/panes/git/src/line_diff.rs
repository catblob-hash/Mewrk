//! Line-level unified diff between two versions of one text: upstream's `lib/lineDiff.ts`.
//!
//! Word-level alignment explains what changed inside one line pair; this produces the whole unified patch a diff card
//! renders. It runs once per message-version pair, so it refuses pairs too large to align rather than blocking a frame
//! on them, reporting each side as wholly rewritten instead.

#[derive(Clone, Debug)]
pub struct LineDiffOptions<'a> {
    /// Path label written into the `---`/`+++` headers. Defaults to no file header.
    pub path: Option<&'a str>,
    /// Context lines kept around each hunk.
    pub context: usize,
    /// Above this line count on either side the pair is reported as wholly rewritten.
    pub max_lines: usize,
    /// Above this `before × after` cell count the pair is reported as wholly rewritten. A million cells is a few
    /// milliseconds; past that a whole-file rewrite is the honest answer anyway, because no reader walks a patch that
    /// size line by line.
    pub max_cells: usize,
}

impl Default for LineDiffOptions<'_> {
    fn default() -> Self {
        Self { path: None, context: 3, max_lines: 4000, max_cells: 1_000_000 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineDiffResult {
    /// Unified-diff text. Empty when the sides are identical.
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
    /// True when the pair was too large to align and each side is reported wholly changed.
    pub bailed: bool,
}

/// Splits text into diff lines after normalising every line-ending flavour to `\n`. A trailing newline splits into a
/// phantom empty final element; that element is dropped on both sides alike, so a trailing newline never shows up as a
/// change of its own.
fn split_lines(text: &str) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Common,
    Delete,
    Insert,
}

fn align(before: &[String], after: &[String]) -> (Vec<bool>, Vec<bool>) {
    let (rows, columns) = (before.len(), after.len());
    let width = columns + 1;
    let mut table = vec![0u32; (rows + 1) * width];
    for row in (0..rows).rev() {
        for column in (0..columns).rev() {
            let index = row * width + column;
            table[index] = if before[row] == after[column] { table[index + width + 1] + 1 } else { table[index + width].max(table[index + 1]) };
        }
    }
    let (mut before_kept, mut after_kept) = (vec![false; rows], vec![false; columns]);
    let (mut row, mut column) = (0, 0);
    while row < rows && column < columns {
        let index = row * width + column;
        if before[row] == after[column] {
            before_kept[row] = true;
            after_kept[column] = true;
            row += 1;
            column += 1;
            continue;
        }
        if table[index + width] >= table[index + 1] {
            row += 1;
        } else {
            column += 1;
        }
    }
    (before_kept, after_kept)
}

fn build_ops<'a>(before: &'a [String], after: &'a [String], head: usize, tail: usize) -> Vec<(OpKind, &'a str)> {
    let before_middle = &before[head..before.len() - tail];
    let after_middle = &after[head..after.len() - tail];
    let (before_kept, after_kept) = align(before_middle, after_middle);
    let mut ops: Vec<(OpKind, &str)> = before[..head].iter().map(|l| (OpKind::Common, l.as_str())).collect();
    let (mut b, mut a) = (0, 0);
    while b < before_middle.len() || a < after_middle.len() {
        if b < before_middle.len() && a < after_middle.len() && before_kept[b] && after_kept[a] {
            ops.push((OpKind::Common, &before_middle[b]));
            b += 1;
            a += 1;
            continue;
        }
        if b < before_middle.len() && !before_kept[b] {
            ops.push((OpKind::Delete, &before_middle[b]));
            b += 1;
            continue;
        }
        ops.push((OpKind::Insert, &after_middle[a]));
        a += 1;
    }
    ops.extend(before[before.len() - tail..].iter().map(|l| (OpKind::Common, l.as_str())));
    ops
}

/// Renders the ops as `@@` hunks, keeping `context` common lines around each run of changes. Two runs whose separating
/// common lines would fit inside the context of both neighbours share one hunk. A side with no lines addresses the line
/// before its would-be start, which is what makes an empty side read as `-0,0` or `+0,0`.
fn build_hunks(ops: &[(OpKind, &str)], context: usize) -> Vec<String> {
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, (k, _))| *k != OpKind::Common).map(|(i, _)| i).collect();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for index in changed {
        match groups.last_mut() {
            Some(current) if index - current[current.len() - 1] - 1 <= context * 2 => current.push(index),
            _ => groups.push(vec![index]),
        }
    }
    let mut hunks = Vec::with_capacity(groups.len());
    for group in groups {
        let from = group[0].saturating_sub(context);
        let to = ops.len().min(group[group.len() - 1] + 1 + context);
        let (mut old_start, mut new_start) = (1, 1);
        for (kind, _) in &ops[..from] {
            if *kind != OpKind::Insert {
                old_start += 1;
            }
            if *kind != OpKind::Delete {
                new_start += 1;
            }
        }
        let (mut old_count, mut new_count) = (0, 0);
        for (kind, _) in &ops[from..to] {
            if *kind != OpKind::Insert {
                old_count += 1;
            }
            if *kind != OpKind::Delete {
                new_count += 1;
            }
        }
        if old_count == 0 {
            old_start -= 1;
        }
        if new_count == 0 {
            new_start -= 1;
        }
        let mut lines = vec![format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@")];
        for (kind, text) in &ops[from..to] {
            let marker = match kind {
                OpKind::Common => ' ',
                OpKind::Delete => '-',
                OpKind::Insert => '+',
            };
            lines.push(format!("{marker}{text}"));
        }
        hunks.push(lines.join("\n"));
    }
    hunks
}

pub fn line_diff(before: &str, after: &str) -> LineDiffResult {
    line_diff_with(before, after, &LineDiffOptions::default())
}

pub fn line_diff_with(before: &str, after: &str, options: &LineDiffOptions<'_>) -> LineDiffResult {
    let before_lines = split_lines(before);
    let after_lines = split_lines(after);
    if before_lines == after_lines {
        return LineDiffResult { patch: String::new(), additions: 0, deletions: 0, bailed: false };
    }
    // The common head and tail are almost always most of the text; stripping them first is what keeps the table small
    // enough to be worth allocating at all.
    let shortest = before_lines.len().min(after_lines.len());
    let mut head = 0;
    while head < shortest && before_lines[head] == after_lines[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < shortest - head && before_lines[before_lines.len() - 1 - tail] == after_lines[after_lines.len() - 1 - tail] {
        tail += 1;
    }
    let before_middle = before_lines.len() - tail - head;
    let after_middle = after_lines.len() - tail - head;
    if before_middle > options.max_lines || after_middle > options.max_lines || (before_middle + 1) * (after_middle + 1) > options.max_cells {
        // No alignment was computed, so no context would be honest: every before line goes out as a deletion and every
        // after line as an addition, and the counts still describe exactly what a reader would have to retype.
        let (old_start, new_start) = (usize::from(!before_lines.is_empty()), usize::from(!after_lines.is_empty()));
        let mut lines = vec![format!("@@ -{old_start},{} +{new_start},{} @@", before_lines.len(), after_lines.len())];
        lines.extend(before_lines.iter().map(|l| format!("-{l}")));
        lines.extend(after_lines.iter().map(|l| format!("+{l}")));
        return LineDiffResult { patch: lines.join("\n"), additions: after_lines.len(), deletions: before_lines.len(), bailed: true };
    }
    let ops = build_ops(&before_lines, &after_lines, head, tail);
    let mut parts: Vec<String> = Vec::new();
    if let Some(path) = options.path {
        parts.push(format!("--- a/{path}"));
        parts.push(format!("+++ b/{path}"));
    }
    parts.extend(build_hunks(&ops, options.context));
    // Counted from the ops, not by scanning the patch text: a content line that itself starts with `+` or `-` is still
    // just one op, and every op lands in exactly one hunk.
    let additions = ops.iter().filter(|(k, _)| *k == OpKind::Insert).count();
    let deletions = ops.iter().filter(|(k, _)| *k == OpKind::Delete).count();
    LineDiffResult { patch: parts.join("\n"), additions, deletions, bailed: false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_output::parse_unified_diff_lines;

    fn twenty() -> String {
        (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
    }

    fn headers(patch: &str) -> Vec<&str> {
        patch.split('\n').filter(|l| l.starts_with("@@")).collect()
    }

    fn empty() -> LineDiffResult {
        LineDiffResult { patch: String::new(), additions: 0, deletions: 0, bailed: false }
    }

    #[test]
    fn identical_strings_are_an_empty_patch() {
        assert_eq!(line_diff("alpha\nbeta\ngamma", "alpha\nbeta\ngamma"), empty());
        assert_eq!(line_diff("alpha\nbeta\n", "alpha\nbeta\n"), empty());
        assert_eq!(line_diff("", ""), empty());
        // The parser has no "\ No newline" marker, so the trailing newline is dropped on both sides alike.
        assert_eq!(line_diff("alpha\nbeta\n", "alpha\nbeta"), empty());
    }

    #[test]
    fn a_one_line_middle_change_is_one_addition_and_one_deletion() {
        let r = line_diff(&twenty(), &twenty().replace("line 10", "line ten"));
        assert!(!r.bailed);
        assert_eq!((r.additions, r.deletions), (1, 1));
        assert_eq!(headers(&r.patch), ["@@ -7,7 +7,7 @@"]);
        assert!(r.patch.contains("-line 10") && r.patch.contains("+line ten"));
    }

    #[test]
    fn pure_insertions_and_deletions() {
        let r = line_diff("alpha\nbeta\ngamma", "alpha\nbeta\ninserted\ngamma");
        assert_eq!((r.additions, r.deletions), (1, 0));
        assert_eq!(headers(&r.patch), ["@@ -1,3 +1,4 @@"]);
        let r = line_diff("alpha\nbeta\ngamma", "alpha\ngamma");
        assert_eq!((r.additions, r.deletions), (0, 1));
        assert_eq!(headers(&r.patch), ["@@ -1,3 +1,2 @@"]);
    }

    #[test]
    fn an_empty_side_is_addressed_as_zero() {
        let r = line_diff("", "alpha\nbeta\ngamma");
        assert_eq!((r.additions, r.deletions), (3, 0));
        assert_eq!(headers(&r.patch), ["@@ -0,0 +1,3 @@"]);
        let r = line_diff("alpha\nbeta\ngamma", "");
        assert_eq!((r.additions, r.deletions), (0, 3));
        assert_eq!(headers(&r.patch), ["@@ -1,3 +0,0 @@"]);
    }

    #[test]
    fn normalises_crlf_and_lone_cr() {
        let a = line_diff("alpha\r\nbeta\r\ngamma", "alpha\nline two\ngamma");
        let b = line_diff("alpha\rbeta\rgamma", "alpha\nline two\ngamma");
        assert_eq!((a.additions, a.deletions), (1, 1));
        assert_eq!(a.patch, b.patch);
    }

    #[test]
    fn distant_edits_are_two_hunks_and_nearby_ones_one() {
        let r = line_diff(&twenty(), &twenty().replace("line 2\n", "line two\n").replace("line 19", "line nineteen"));
        assert_eq!((r.additions, r.deletions), (2, 2));
        assert_eq!(headers(&r.patch), ["@@ -1,5 +1,5 @@", "@@ -16,5 +16,5 @@"]);
        let r = line_diff(&twenty(), &twenty().replace("line 10", "line ten").replace("line 12", "line twelve"));
        assert_eq!(headers(&r.patch), ["@@ -7,9 +7,9 @@"]);
    }

    #[test]
    fn honours_a_smaller_context() {
        let r = line_diff_with(&twenty(), &twenty().replace("line 10", "line ten"), &LineDiffOptions { context: 1, ..LineDiffOptions::default() });
        assert_eq!(headers(&r.patch), ["@@ -9,3 +9,3 @@"]);
    }

    #[test]
    fn bails_past_its_limits_with_a_whole_rewrite() {
        let (before, after) = ("a\nb\nc\nd\ne", "1\n2\n3\n4\n5");
        let r = line_diff_with(before, after, &LineDiffOptions { max_lines: 2, ..LineDiffOptions::default() });
        assert!(r.bailed);
        assert_eq!((r.additions, r.deletions), (5, 5));
        assert_eq!(headers(&r.patch), ["@@ -1,5 +1,5 @@"]);
        let r = line_diff_with(before, after, &LineDiffOptions { max_cells: 8, ..LineDiffOptions::default() });
        assert!(r.bailed);
    }

    #[test]
    fn writes_file_headers_only_when_given_a_path() {
        let with = line_diff_with("alpha\nbeta", "alpha\ngamma", &LineDiffOptions { path: Some("m.txt"), ..LineDiffOptions::default() });
        assert!(with.patch.starts_with("--- a/m.txt\n+++ b/m.txt\n"));
        assert!(line_diff("alpha\nbeta", "alpha\ngamma").patch.starts_with("@@ "));
    }

    #[test]
    fn round_trips_through_the_cards_parser_with_matching_counts_and_path() {
        let t = twenty();
        let five = t.replace("line 5", "line five").replace("line 15", "line fifteen");
        let cases: [(&str, &str); 6] = [("alpha\nbeta\ngamma", "alpha\nbeta\ngamma delta"), (&t, &five), ("", "brand new\ncontent\nhere"), ("old\ncontent\nhere", ""), ("keep\n- minus\nplus +\nkeep", "keep\n+++ triple\nkeep"), ("a\r\nb\r\nc", "a\nb two\nc")];
        for (before, after) in cases {
            let r = line_diff_with(before, after, &LineDiffOptions { path: Some("m.txt"), ..LineDiffOptions::default() });
            let parsed = parse_unified_diff_lines(&r.patch);
            assert_eq!(parsed.additions, r.additions, "{before:?}");
            assert_eq!(parsed.deletions, r.deletions, "{before:?}");
            assert_eq!(parsed.path.as_deref(), Some("m.txt"));
            assert!(!r.bailed);
        }
    }
}
