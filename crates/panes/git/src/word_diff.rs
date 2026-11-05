//! Word-level alignment of two versions of one line: upstream's `lib/wordDiff.ts`.
//!
//! A line-level diff says a line changed; it does not say what about it changed. Highlighting the differing runs is what
//! turns a wall of red and green into a readable edit. This runs once per paired addition and deletion in a rendered
//! diff, so it refuses long pairs rather than blocking a frame on them.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordDiffSegment {
    pub text: String,
    /// True when this run is absent from the other side, or is whitespace joining two such runs.
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordDiffResult {
    pub before: Vec<WordDiffSegment>,
    pub after: Vec<WordDiffSegment>,
    /// True when the pair was too large to align and each side is reported wholly changed.
    pub bailed: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct WordDiffOptions {
    /// Treat runs of whitespace as equal to each other, so re-indentation is not highlighted.
    pub ignore_whitespace: bool,
    /// Above this token count on either side the pair is reported as wholly changed.
    pub max_tokens: usize,
}

impl Default for WordDiffOptions {
    fn default() -> Self {
        Self { ignore_whitespace: false, max_tokens: 400 }
    }
}

/// Ceiling on the alignment table. The table is `before × after` cells, so two lines that each pass the token cap can
/// still multiply into something not worth allocating. 250k cells is a few milliseconds; past that the highlight is not
/// worth what it costs to compute.
const MAX_TABLE_CELLS: usize = 250_000;

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Splits a line into the runs the alignment works on: a run of word characters, a run of whitespace, and any other
/// single character on its own. Concatenating the result reproduces the input exactly.
pub fn tokenize_line(line: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut rest = line;
    while let Some(first) = rest.chars().next() {
        let end = if is_word(first) {
            rest.find(|c: char| !is_word(c)).unwrap_or(rest.len())
        } else if first.is_whitespace() {
            rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len())
        } else {
            first.len_utf8()
        };
        tokens.push(&rest[..end]);
        rest = &rest[end..];
    }
    tokens
}

fn same(a: &str, b: &str, ignore_whitespace: bool) -> bool {
    a == b || (ignore_whitespace && is_whitespace(a) && is_whitespace(b))
}

fn is_whitespace(token: &str) -> bool {
    !token.is_empty() && token.chars().all(char::is_whitespace)
}

/// Appends a token, merging into the previous run when it carries the same verdict.
fn push(segments: &mut Vec<WordDiffSegment>, text: &str, changed: bool) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = segments.last_mut()
        && last.changed == changed
    {
        last.text.push_str(text);
        return;
    }
    segments.push(WordDiffSegment { text: text.to_owned(), changed });
}

fn whole_side(line: &str) -> Vec<WordDiffSegment> {
    if line.is_empty() { Vec::new() } else { vec![WordDiffSegment { text: line.to_owned(), changed: true }] }
}

/// Aligns two token arrays by longest common subsequence. LCS rather than Myers: with the table already bounded by the
/// guards above, the quadratic table is affordable, and it has no special cases around an empty side.
fn align(before: &[&str], after: &[&str], ignore_whitespace: bool) -> (Vec<bool>, Vec<bool>) {
    let (rows, columns) = (before.len(), after.len());
    let width = columns + 1;
    let mut table = vec![0u32; (rows + 1) * width];
    for row in (0..rows).rev() {
        for column in (0..columns).rev() {
            let index = row * width + column;
            table[index] = if same(before[row], after[column], ignore_whitespace) { table[index + width + 1] + 1 } else { table[index + width].max(table[index + 1]) };
        }
    }
    let (mut before_kept, mut after_kept) = (vec![false; rows], vec![false; columns]);
    let (mut row, mut column) = (0, 0);
    while row < rows && column < columns {
        let index = row * width + column;
        if same(before[row], after[column], ignore_whitespace) {
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

pub fn word_diff(before: &str, after: &str) -> WordDiffResult {
    word_diff_with(before, after, WordDiffOptions::default())
}

pub fn word_diff_with(before: &str, after: &str, options: WordDiffOptions) -> WordDiffResult {
    if before == after {
        let side = |text: &str| if text.is_empty() { Vec::new() } else { vec![WordDiffSegment { text: text.to_owned(), changed: false }] };
        return WordDiffResult { before: side(before), after: side(after), bailed: false };
    }
    let ignore = options.ignore_whitespace;
    let before_tokens = tokenize_line(before);
    let after_tokens = tokenize_line(after);

    // The common head and tail are almost always most of the line; stripping them first is what keeps the table small
    // enough to be worth allocating at all.
    let shortest = before_tokens.len().min(after_tokens.len());
    let mut head = 0;
    while head < shortest && same(before_tokens[head], after_tokens[head], ignore) {
        head += 1;
    }
    let mut tail = 0;
    while tail < shortest - head && same(before_tokens[before_tokens.len() - 1 - tail], after_tokens[after_tokens.len() - 1 - tail], ignore) {
        tail += 1;
    }
    let before_middle = &before_tokens[head..before_tokens.len() - tail];
    let after_middle = &after_tokens[head..after_tokens.len() - tail];
    if before_middle.len() > options.max_tokens || after_middle.len() > options.max_tokens || (before_middle.len() + 1) * (after_middle.len() + 1) > MAX_TABLE_CELLS {
        return WordDiffResult { before: whole_side(before), after: whole_side(after), bailed: true };
    }
    let (before_kept, after_kept) = align(before_middle, after_middle, ignore);
    let build = |tokens: &[&str], middle: &[&str], kept: &[bool]| -> Vec<WordDiffSegment> {
        // Whitespace never carries a change on its own when it is being ignored: marking it would draw a highlight on
        // the very thing the setting hides.
        let mut changed: Vec<bool> = middle.iter().zip(kept).map(|(token, kept)| !kept && !(ignore && is_whitespace(token))).collect();
        // Whitespace between two changed runs joins them. A rewritten phrase still has spaces where the old one did, and
        // they are all the alignment finds in common; left unmarked, they cut one edit into a highlight per word.
        for index in 1..middle.len().saturating_sub(1) {
            if is_whitespace(middle[index]) && changed[index - 1] && changed[index + 1] {
                changed[index] = true;
            }
        }
        let mut segments = Vec::new();
        for token in &tokens[..head] {
            push(&mut segments, token, false);
        }
        for (token, changed) in middle.iter().zip(&changed) {
            push(&mut segments, token, *changed);
        }
        for token in &tokens[tokens.len() - tail..] {
            push(&mut segments, token, false);
        }
        segments
    };
    WordDiffResult { before: build(&before_tokens, before_middle, &before_kept), after: build(&after_tokens, after_middle, &after_kept), bailed: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str, changed: bool) -> WordDiffSegment {
        WordDiffSegment { text: text.into(), changed }
    }

    /// The two invariants every result must hold, on both sides.
    fn sound(result: &WordDiffResult, before: &str, after: &str) {
        assert_eq!(result.before.iter().map(|s| s.text.as_str()).collect::<String>(), before);
        assert_eq!(result.after.iter().map(|s| s.text.as_str()).collect::<String>(), after);
        for side in [&result.before, &result.after] {
            for pair in side.windows(2) {
                assert_ne!(pair[0].changed, pair[1].changed);
            }
        }
    }

    #[test]
    fn tokenize_round_trips_a_variety_of_lines() {
        for line in ["const value = compute(42);", "\tindented\tline\t", "文件名和 café", "snake_case $dollar 123", "punct! (a) [b] {c} #d?", "emoji 🎉 outside the BMP", ""] {
            assert_eq!(tokenize_line(line).concat(), line);
        }
    }

    #[test]
    fn tokenize_keeps_the_runs_it_should() {
        assert_eq!(tokenize_line("文件名"), ["文件名"]);
        assert_eq!(tokenize_line("café"), ["café"]);
        assert_eq!(tokenize_line("a.b"), ["a", ".", "b"]);
        assert_eq!(tokenize_line("a    b"), ["a", "    ", "b"]);
        assert_eq!(tokenize_line("x🎉y"), ["x", "🎉", "y"]);
    }

    #[test]
    fn aligns_a_one_word_substitution_as_three_segments_per_side() {
        let r = word_diff("const value = compute();", "const result = compute();");
        assert!(!r.bailed);
        assert_eq!(r.before, [seg("const ", false), seg("value", true), seg(" = compute();", false)]);
        assert_eq!(r.after, [seg("const ", false), seg("result", true), seg(" = compute();", false)]);
    }

    #[test]
    fn marks_insertions_on_the_after_side_and_deletions_on_the_before_side() {
        let r = word_diff("world", "hello world");
        assert_eq!(r.before, [seg("world", false)]);
        assert_eq!(r.after, [seg("hello ", true), seg("world", false)]);
        let r = word_diff("hello", "hello world");
        assert_eq!(r.after, [seg("hello", false), seg(" world", true)]);
        let r = word_diff("a c", "a b c");
        assert_eq!(r.after, [seg("a ", false), seg("b ", true), seg("c", false)]);
        let r = word_diff("a b c", "a c");
        assert_eq!(r.before, [seg("a ", false), seg("b ", true), seg("c", false)]);
        assert_eq!(r.after, [seg("a c", false)]);
    }

    #[test]
    fn identical_and_empty_sides() {
        let r = word_diff("same line", "same line");
        assert_eq!(r.before, [seg("same line", false)]);
        assert_eq!(r.after, [seg("same line", false)]);
        let r = word_diff("", "hello world");
        assert!(r.before.is_empty());
        assert_eq!(r.after, [seg("hello world", true)]);
        let r = word_diff("hello world", "");
        assert_eq!(r.before, [seg("hello world", true)]);
        assert!(r.after.is_empty());
    }

    #[test]
    fn re_indentation_is_ignored_only_when_asked() {
        let ignoring = WordDiffOptions { ignore_whitespace: true, ..WordDiffOptions::default() };
        let r = word_diff_with("  foo()", "\t\tfoo()", ignoring);
        assert!(!r.before.iter().any(|s| s.changed) && !r.after.iter().any(|s| s.changed));
        sound(&r, "  foo()", "\t\tfoo()");
        let r = word_diff("  foo()", "\t\tfoo()");
        assert_eq!(r.before[0], seg("  ", true));
        assert_eq!(r.after[0], seg("\t\t", true));
    }

    #[test]
    fn bails_on_a_pair_above_max_tokens_and_still_reproduces_both_sides() {
        let (before, after) = ("one two three four five", "1 2 3 4 5");
        let r = word_diff_with(before, after, WordDiffOptions { max_tokens: 2, ..WordDiffOptions::default() });
        assert!(r.bailed);
        assert_eq!(r.before, [seg(before, true)]);
        assert_eq!(r.after, [seg(after, true)]);
        sound(&r, before, after);
    }

    #[test]
    fn lands_the_lcs_on_the_later_common_token() {
        let r = word_diff("a b c d e", "a c b d e");
        assert_eq!(r.before, [seg("a ", false), seg("b ", true), seg("c d e", false)]);
        assert_eq!(r.after, [seg("a c", false), seg(" b", true), seg(" d e", false)]);
    }

    #[test]
    fn joins_rewritten_words_across_the_spaces_they_still_share() {
        let r = word_diff("// the quick brown fox", "// a slow red dog");
        assert_eq!(r.before, [seg("// ", false), seg("the quick brown fox", true)]);
        assert_eq!(r.after, [seg("// ", false), seg("a slow red dog", true)]);
    }

    #[test]
    fn leaves_whitespace_unmarked_when_only_one_neighbour_changed() {
        let r = word_diff("const foo = bar;", "let baz = qux;");
        assert_eq!(r.before, [seg("const foo", true), seg(" = ", false), seg("bar", true), seg(";", false)]);
        assert_eq!(r.after, [seg("let baz", true), seg(" = ", false), seg("qux", true), seg(";", false)]);
    }

    #[test]
    fn joins_changed_words_across_whitespace_that_is_being_ignored() {
        let r = word_diff_with("x foo  bar", "x baz qux", WordDiffOptions { ignore_whitespace: true, ..WordDiffOptions::default() });
        assert_eq!(r.before, [seg("x ", false), seg("foo  bar", true)]);
        assert_eq!(r.after, [seg("x ", false), seg("baz qux", true)]);
    }

    #[test]
    fn holds_the_invariants_on_every_case() {
        for (before, after) in [("const value = compute();", "const result = compute();"), ("world", "hello world"), ("a c", "a b c"), ("", "hello world"), ("  foo()", "\t\tfoo()"), ("a b c d e", "a c b d e"), ("x = [1, 2, 3];", "x = [1, 2, 3, 4];"), ("return foo(a, b);", "return foo(a, c);")] {
            sound(&word_diff(before, after), before, after);
        }
    }
}
