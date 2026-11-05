//! `lib/lineDiff.ts`: the unified patch a diff card renders. Refuses pairs too large to align rather than blocking a
//! frame, reporting each side as wholly rewritten.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineDiffResult {
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
    pub bailed: bool,
}

const MAX_LINES: usize = 4000;
const MAX_CELLS: usize = 1_000_000;
const CONTEXT: usize = 3;

fn split_lines(text: &str) -> Vec<String> {
    let normal = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normal.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Common,
    Delete,
    Insert,
}

fn align_by_lcs(before: &[String], after: &[String]) -> (Vec<bool>, Vec<bool>) {
    let (rows, columns) = (before.len(), after.len());
    let mut table = vec![0u32; (rows + 1) * (columns + 1)];
    for row in (0..rows).rev() {
        for column in (0..columns).rev() {
            let index = row * (columns + 1) + column;
            table[index] = if before[row] == after[column] { table[index + columns + 2] + 1 } else { table[index + columns + 1].max(table[index + 1]) };
        }
    }
    let (mut before_kept, mut after_kept) = (vec![false; rows], vec![false; columns]);
    let (mut row, mut column) = (0, 0);
    while row < rows && column < columns {
        let index = row * (columns + 1) + column;
        if before[row] == after[column] {
            before_kept[row] = true;
            after_kept[column] = true;
            row += 1;
            column += 1;
        } else if table[index + columns + 1] >= table[index + 1] {
            row += 1;
        } else {
            column += 1;
        }
    }
    (before_kept, after_kept)
}

fn build_ops<'a>(before: &'a [String], after: &'a [String], head: usize, tail: usize) -> Vec<(Op, &'a str)> {
    let before_middle = &before[head..before.len() - tail];
    let after_middle = &after[head..after.len() - tail];
    let (before_kept, after_kept) = align_by_lcs(before_middle, after_middle);
    let mut ops: Vec<(Op, &str)> = before[..head].iter().map(|l| (Op::Common, l.as_str())).collect();
    let (mut b, mut a) = (0, 0);
    while b < before_middle.len() || a < after_middle.len() {
        if b < before_middle.len() && a < after_middle.len() && before_kept[b] && after_kept[a] {
            ops.push((Op::Common, &before_middle[b]));
            b += 1;
            a += 1;
        } else if b < before_middle.len() && !before_kept[b] {
            ops.push((Op::Delete, &before_middle[b]));
            b += 1;
        } else {
            ops.push((Op::Insert, &after_middle[a]));
            a += 1;
        }
    }
    ops.extend(before[before.len() - tail..].iter().map(|l| (Op::Common, l.as_str())));
    ops
}

fn build_hunks(ops: &[(Op, &str)], context: usize) -> Vec<String> {
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, o)| o.0 != Op::Common).map(|(i, _)| i).collect();
    if changed.is_empty() {
        return Vec::new();
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for index in changed {
        match groups.last_mut() {
            Some(current) if index - current[current.len() - 1] - 1 <= context * 2 => current.push(index),
            _ => groups.push(vec![index]),
        }
    }
    groups
        .iter()
        .map(|group| {
            let from = group[0].saturating_sub(context);
            let to = ops.len().min(group[group.len() - 1] + 1 + context);
            let (mut old_start, mut new_start) = (1i64, 1i64);
            for op in &ops[..from] {
                if op.0 != Op::Insert {
                    old_start += 1;
                }
                if op.0 != Op::Delete {
                    new_start += 1;
                }
            }
            let (mut old_count, mut new_count) = (0i64, 0i64);
            for op in &ops[from..to] {
                if op.0 != Op::Insert {
                    old_count += 1;
                }
                if op.0 != Op::Delete {
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
            for op in &ops[from..to] {
                let marker = match op.0 {
                    Op::Common => ' ',
                    Op::Delete => '-',
                    Op::Insert => '+',
                };
                lines.push(format!("{marker}{}", op.1));
            }
            lines.join("\n")
        })
        .collect()
}

pub fn line_diff(before: &str, after: &str, path: Option<&str>) -> LineDiffResult {
    line_diff_with(before, after, path, MAX_LINES, MAX_CELLS, CONTEXT)
}

pub fn line_diff_with(before: &str, after: &str, path: Option<&str>, max_lines: usize, max_cells: usize, context: usize) -> LineDiffResult {
    let (before_lines, after_lines) = (split_lines(before), split_lines(after));
    if before_lines == after_lines {
        return LineDiffResult::default();
    }
    let shortest = before_lines.len().min(after_lines.len());
    let mut head = 0;
    while head < shortest && before_lines[head] == after_lines[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < shortest - head && before_lines[before_lines.len() - 1 - tail] == after_lines[after_lines.len() - 1 - tail] {
        tail += 1;
    }
    let (before_middle, after_middle) = (before_lines.len() - tail - head, after_lines.len() - tail - head);
    if before_middle > max_lines || after_middle > max_lines || (before_middle + 1) * (after_middle + 1) > max_cells {
        let (old_start, new_start) = (usize::from(!before_lines.is_empty()), usize::from(!after_lines.is_empty()));
        let mut lines = vec![format!("@@ -{old_start},{} +{new_start},{} @@", before_lines.len(), after_lines.len())];
        lines.extend(before_lines.iter().map(|l| format!("-{l}")));
        lines.extend(after_lines.iter().map(|l| format!("+{l}")));
        return LineDiffResult { patch: lines.join("\n"), additions: after_lines.len(), deletions: before_lines.len(), bailed: true };
    }
    let ops = build_ops(&before_lines, &after_lines, head, tail);
    let hunks = build_hunks(&ops, context);
    let mut parts: Vec<String> = Vec::new();
    if let Some(path) = path.filter(|p| !p.is_empty()) {
        parts.push(format!("--- a/{path}"));
        parts.push(format!("+++ b/{path}"));
    }
    parts.extend(hunks);
    LineDiffResult {
        patch: parts.join("\n"),
        additions: ops.iter().filter(|o| o.0 == Op::Insert).count(),
        deletions: ops.iter().filter(|o| o.0 == Op::Delete).count(),
        bailed: false,
    }
}
