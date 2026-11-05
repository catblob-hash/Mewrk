//! Turns the text of `git diff` for one file into a `CodeDoc`.
//!
//! Nothing here fails: a patch that cannot be understood still draws as what could be read of it, because a diff the
//! reader can see is partial beats one they cannot see at all.

use crate::code::{CodeDoc, RowKind};
use crate::highlight::{self, State};

pub(crate) fn build(patch: &str, path: &str, truncated: bool) -> CodeDoc {
    let lang = highlight::lang_of(path);
    let mut doc = CodeDoc::new(true);
    let mut words = WordDiff::default();

    let (mut old_no, mut new_no, mut old_left, mut new_left) = (0u32, 0u32, 0u32, 0u32);
    let (mut old_state, mut new_state) = (State::default(), State::default());
    let mut prev_old_end: Option<u32> = None;
    let (mut dels, mut adds): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
    let (mut binary, mut hunks, mut renamed) = (false, 0u32, false);
    let (mut old_mode, mut new_mode): (Option<&str>, Option<&str>) = (None, None);
    let mut scratch = Vec::new();

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix('\\') {
            // `\ No newline at end of file` belongs to the row before it, however the counts stand.
            if rest.starts_with(' ') && hunks > 0 {
                doc.append_to_last(" ↵");
            }
            continue;
        }
        if old_left > 0 || new_left > 0 {
            match line.as_bytes().first() {
                Some(b'+') if new_left > 0 => {
                    doc.push_code(RowKind::Add, new_no, &line[1..], lang, &mut new_state);
                    adds.push(doc.rows.len() - 1);
                    doc.additions += 1;
                    new_no += 1;
                    new_left -= 1;
                    continue;
                }
                Some(b'-') if old_left > 0 => {
                    // Removals after additions start a new replacement.
                    if !adds.is_empty() {
                        pair_up(&mut doc, &mut dels, &mut adds, &mut words);
                    }
                    doc.push_code(RowKind::Del, old_no, &line[1..], lang, &mut old_state);
                    dels.push(doc.rows.len() - 1);
                    doc.deletions += 1;
                    old_no += 1;
                    old_left -= 1;
                    continue;
                }
                Some(b' ') | None if old_left > 0 && new_left > 0 => {
                    pair_up(&mut doc, &mut dels, &mut adds, &mut words);
                    let text = line.get(1..).unwrap_or("");
                    let before = new_state;
                    doc.push_code(RowKind::Code, new_no, text, lang, &mut new_state);
                    // A hunk is two excerpts of a file interleaved, each carrying its own open comments and strings.
                    if old_state == before {
                        old_state = new_state;
                    } else if let Some(lang) = lang {
                        highlight::line(lang, &mut old_state, text, &mut scratch);
                    }
                    old_no += 1;
                    new_no += 1;
                    old_left -= 1;
                    new_left -= 1;
                    continue;
                }
                // Out of step with the header. Whatever this is, it is not a line of this hunk.
                _ => {
                    old_left = 0;
                    new_left = 0;
                }
            }
        }
        pair_up(&mut doc, &mut dels, &mut adds, &mut words);
        if let Some(h) = parse_hunk_header(line) {
            let gap = match prev_old_end {
                None => h.old_start.saturating_sub(1),
                Some(end) => h.old_start.saturating_sub(end),
            };
            if gap > 0 || !h.heading.is_empty() {
                doc.push_band(RowKind::Sep, gap, h.heading);
            }
            prev_old_end = Some(h.old_start.max(1) + h.old_lines);
            hunks += 1;
            (old_no, new_no, old_left, new_left) = (h.old_start, h.new_start, h.old_lines, h.new_lines);
            // What was open above the hunk is not known, so each side starts clean.
            (old_state, new_state) = (State::default(), State::default());
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            binary = true;
        } else if let Some(mode) = line.strip_prefix("old mode ") {
            old_mode = Some(mode);
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            new_mode = Some(mode);
        } else if line.starts_with("rename from ") || line.starts_with("copy from ") {
            renamed = true;
        }
    }
    pair_up(&mut doc, &mut dels, &mut adds, &mut words);

    if hunks == 0 {
        let note = match (binary, old_mode, new_mode, renamed) {
            (true, ..) => "二进制文件，无法显示差异".to_owned(),
            (_, Some(from), Some(to), _) => format!("仅文件模式变更：{from} → {to}"),
            (_, _, _, true) => "仅重命名，内容没有变化".to_owned(),
            _ => "这个文件没有行级差异".to_owned(),
        };
        doc.push_band(RowKind::Note, 0, &note);
    }
    if truncated {
        doc.push_band(RowKind::Note, 0, "差异过大，已被截断。");
    }
    doc
}

struct HunkHeader<'a> {
    old_start: u32,
    old_lines: u32,
    new_start: u32,
    new_lines: u32,
    heading: &'a str,
}

fn parse_hunk_header(line: &str) -> Option<HunkHeader<'_>> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, heading) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |r: &str| -> Option<(u32, u32)> {
        match r.split_once(',') {
            Some((start, n)) => Some((start.parse().ok()?, n.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (old_start, old_lines) = range(old)?;
    let (new_start, new_lines) = range(new)?;
    Some(HunkHeader { old_start, old_lines, new_start, new_lines, heading: heading.trim_start() })
}

/// A run of removals followed by a run of additions is one replacement: the nth removal answers the nth addition.
fn pair_up(doc: &mut CodeDoc, dels: &mut Vec<usize>, adds: &mut Vec<usize>, words: &mut WordDiff) {
    for (&d, &a) in dels.iter().zip(adds.iter()) {
        if words.align(doc.row_text(d), doc.row_text(a)) {
            let (before, after) = (std::mem::take(&mut words.before), std::mem::take(&mut words.after));
            doc.set_marks(d, &before);
            doc.set_marks(a, &after);
            (words.before, words.after) = (before, after);
        }
    }
    dels.clear();
    adds.clear();
}

/// Above this many tokens in what remains after trimming, a line is reported with no marks at all.
const MAX_TOKENS: usize = 400;
/// The table is `before × after` cells, and two lines under the token cap can still multiply into too many.
const MAX_CELLS: usize = 64 * 1024;

/// Marks what changed between two versions of a line, by longest common subsequence over words. The common head and tail
/// are almost always most of the line, and stripping them first is what makes the table cheap enough to run on every
/// pair of a long diff.
#[derive(Default)]
struct WordDiff {
    table: Vec<u16>,
    a: Vec<(u32, u32)>,
    b: Vec<(u32, u32)>,
    keep_a: Vec<bool>,
    keep_b: Vec<bool>,
    before: Vec<(u32, u32)>,
    after: Vec<(u32, u32)>,
}

fn tokenize(line: &str, out: &mut Vec<(u32, u32)>) {
    out.clear();
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut chars = line.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let mut end = start + c.len_utf8();
        let same: Option<fn(char) -> bool> = if is_word(c) {
            Some(|c| c.is_alphanumeric() || c == '_' || c == '$')
        } else if c.is_whitespace() {
            Some(char::is_whitespace)
        } else {
            None
        };
        if let Some(same) = same {
            while let Some(&(i, n)) = chars.peek() {
                if !same(n) {
                    break;
                }
                end = i + n.len_utf8();
                chars.next();
            }
        }
        out.push((start as u32, end as u32));
    }
}

fn piece(line: &str, t: (u32, u32)) -> &str {
    &line[t.0 as usize..t.1 as usize]
}

/// Appends the changed tokens as byte ranges. Whitespace between two changed runs joins them: a rewritten phrase still has
/// spaces where the old one did, and left unmarked they cut one edit into a highlight per word.
fn collect(line: &str, tokens: &[(u32, u32)], keep: &[bool], out: &mut Vec<(u32, u32)>) {
    out.clear();
    for (i, &t) in tokens.iter().enumerate() {
        let joined = !keep[i] || (i > 0 && i + 1 < tokens.len() && !keep[i - 1] && !keep[i + 1] && piece(line, t).chars().all(char::is_whitespace));
        if !joined {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 == t.0 => last.1 = t.1,
            _ => out.push(t),
        }
    }
}

impl WordDiff {
    /// True when `before` and `after` now hold the marks for each side.
    fn align(&mut self, before: &str, after: &str) -> bool {
        tokenize(before, &mut self.a);
        tokenize(after, &mut self.b);
        let (a, b) = (&self.a, &self.b);
        let shortest = a.len().min(b.len());
        let mut head = 0;
        while head < shortest && piece(before, a[head]) == piece(after, b[head]) {
            head += 1;
        }
        let mut tail = 0;
        while tail < shortest - head && piece(before, a[a.len() - 1 - tail]) == piece(after, b[b.len() - 1 - tail]) {
            tail += 1;
        }
        let (ra, rb) = (a.len() - head - tail, b.len() - head - tail);
        if ra > MAX_TOKENS || rb > MAX_TOKENS || (ra + 1) * (rb + 1) > MAX_CELLS {
            return false;
        }

        self.keep_a.clear();
        self.keep_a.resize(a.len(), true);
        self.keep_b.clear();
        self.keep_b.resize(b.len(), true);
        let (mid_a, mid_b) = (&a[head..head + ra], &b[head..head + rb]);
        let w = rb + 1;
        self.table.clear();
        self.table.resize((ra + 1) * w, 0);
        for i in (0..ra).rev() {
            for j in (0..rb).rev() {
                self.table[i * w + j] = if piece(before, mid_a[i]) == piece(after, mid_b[j]) { self.table[(i + 1) * w + j + 1] + 1 } else { self.table[(i + 1) * w + j].max(self.table[i * w + j + 1]) };
            }
        }
        for k in &mut self.keep_a[head..head + ra] {
            *k = false;
        }
        for k in &mut self.keep_b[head..head + rb] {
            *k = false;
        }
        let (mut i, mut j) = (0, 0);
        while i < ra && j < rb {
            if piece(before, mid_a[i]) == piece(after, mid_b[j]) {
                self.keep_a[head + i] = true;
                self.keep_b[head + j] = true;
                i += 1;
                j += 1;
            } else if self.table[(i + 1) * w + j] >= self.table[i * w + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
        collect(before, a, &self.keep_a, &mut self.before);
        collect(after, b, &self.keep_b, &mut self.after);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Token;

    fn rows(doc: &CodeDoc) -> Vec<(RowKind, u32, &str)> {
        (0..doc.rows.len()).map(|i| (doc.rows[i].kind, doc.row_no(i), doc.row_text(i))).collect()
    }

    const PATCH: &str = "diff --git a/a.rs b/a.rs
index 111..222 100644
--- a/a.rs
+++ b/a.rs
@@ -3,3 +3,4 @@ fn head() {
 context
-let a = 1;
+let a = 2;
+added
 tail
@@ -20 +21 @@
-old
+new
\\ No newline at end of file
";

    #[test]
    fn hunks_become_numbered_rows() {
        let doc = build(PATCH, "a.rs", false);
        assert_eq!(
            rows(&doc),
            vec![
                (RowKind::Sep, 2, "fn head() {"),
                (RowKind::Code, 3, "context"),
                (RowKind::Del, 4, "let a = 1;"),
                (RowKind::Add, 4, "let a = 2;"),
                (RowKind::Add, 5, "added"),
                (RowKind::Code, 6, "tail"),
                (RowKind::Sep, 14, ""),
                (RowKind::Del, 20, "old"),
                (RowKind::Add, 21, "new ↵"),
            ]
        );
        assert_eq!((doc.additions, doc.deletions), (3, 2));
    }

    #[test]
    fn changed_words_are_marked_on_both_sides() {
        let doc = build(PATCH, "a.rs", false);
        assert_eq!(doc.row_marks(2), &[(8, 9)], "only the 1");
        assert_eq!(doc.row_marks(3), &[(8, 9)], "only the 2");
        assert!(doc.row_marks(4).is_empty(), "a lone addition has nothing to compare to");
    }

    #[test]
    fn lines_that_look_like_headers_inside_a_hunk_are_content() {
        let patch = "@@ -1,2 +1,2 @@\n--- a removed line\n+++ an added line\n diff --git context\n";
        let doc = build(patch, "x", false);
        assert_eq!(rows(&doc), vec![(RowKind::Del, 1, "-- a removed line"), (RowKind::Add, 1, "++ an added line"), (RowKind::Code, 2, "diff --git context")]);
    }

    #[test]
    fn what_has_no_hunks_says_why() {
        for (patch, note) in [
            ("Binary files a/x and b/x differ\n", "二进制文件，无法显示差异"),
            ("old mode 100644\nnew mode 100755\n", "仅文件模式变更：100644 → 100755"),
            ("similarity index 100%\nrename from a\nrename to b\n", "仅重命名，内容没有变化"),
            ("", "这个文件没有行级差异"),
        ] {
            let doc = build(patch, "x", false);
            assert_eq!(rows(&doc), vec![(RowKind::Note, 0, note)]);
        }
    }

    #[test]
    fn a_truncated_patch_says_so_and_keeps_what_it_has() {
        let doc = build("@@ -1,5 +1,5 @@\n-a\n+b\n par", "x", true);
        let r = rows(&doc);
        assert_eq!(r.last().unwrap(), &(RowKind::Note, 0, "差异过大，已被截断。"));
        assert_eq!(r[2], (RowKind::Code, 2, "par"));
    }

    #[test]
    fn each_side_of_a_hunk_keeps_its_own_open_comment() {
        let patch = "@@ -1,3 +1,3 @@\n-/* opened on the old side\n+let a = 1;\n still code or comment\n-*/ fn\n+fn\n";
        let doc = build(patch, "a.rs", false);
        // The removal after the context line continues the old side's comment, not the new side's code.
        let i = doc.rows.iter().rposition(|r| r.kind == RowKind::Del).unwrap();
        assert_eq!(doc.row_text(i), "*/ fn");
        assert_eq!(doc.row_token_kinds(i)[0], Token::Comment);
        let i = doc.rows.iter().rposition(|r| r.kind == RowKind::Add).unwrap();
        assert_eq!(doc.row_token_kinds(i), vec![Token::Keyword]);
    }

    #[test]
    fn word_diff_joins_across_spaces_and_bails_on_huge_lines() {
        let mut w = WordDiff::default();
        assert!(w.align("one two three", "one four five three"));
        assert_eq!(w.before, vec![(4, 7)]);
        assert_eq!(w.after, vec![(4, 13)], "the space between two changed words is part of one edit");
        let (a, b) = ("x ".repeat(5000), "y ".repeat(5000));
        assert!(!w.align(&a, &b));
        assert!(w.align("", "new"));
        assert!(w.before.is_empty());
        assert_eq!(w.after, vec![(0, 3)]);
        assert!(w.align("中文 a", "中文 b"));
        assert_eq!(w.before, vec![(7, 8)]);
    }
}
