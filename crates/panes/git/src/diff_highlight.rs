//! Syntax colours for diff lines, upstream's `lib/diffHighlight.ts`, on top of `mewrk_panes::highlight`.

use mewrk_panes::highlight::{self, Lang, State, Token};

use crate::word_diff::WordDiffSegment;

/// The file a diff line belongs to: the old one, the new one, or, unchanged, both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    Old,
    New,
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeToken {
    pub kind: Token,
    pub value: String,
}

/// The language a file is coloured in: `codeLanguage`.
pub fn code_language(path: &str) -> Option<Lang> {
    highlight::lang_of(path)
}

fn tokens_of(lang: Lang, state: &mut State, text: &str, scratch: &mut Vec<(usize, Token)>) -> Vec<CodeToken> {
    highlight::line(lang, state, text, scratch);
    let mut out = Vec::with_capacity(scratch.len());
    for (i, (start, kind)) in scratch.iter().enumerate() {
        let end = scratch.get(i + 1).map_or(text.len(), |n| n.0);
        if end > *start {
            out.push(CodeToken { kind: *kind, value: text[*start..end].to_owned() });
        }
    }
    // Whatever the highlighter left out at the front is plain, and so is a line it gave up on.
    let covered: usize = out.iter().map(|t| t.value.len()).sum();
    if covered < text.len() {
        let from = scratch.first().map_or(text.len(), |f| f.0);
        let head = &text[..from.min(text.len())];
        let mut full = Vec::with_capacity(out.len() + 1);
        if !head.is_empty() {
            full.push(CodeToken { kind: Token::Plain, value: head.to_owned() });
        }
        full.extend(out);
        return full;
    }
    out
}

/// Syntax tokens for the lines of one hunk, in the order given.
///
/// A hunk is two excerpts of a file interleaved: its unchanged and removed lines are consecutive lines of the old file,
/// its unchanged and added lines of the new one. Each excerpt is tokenized as the run it is, so a block comment or a
/// string opened on one line still colours the next, on its own side and no further. An unchanged line keeps the new
/// file's reading. What was open above the hunk is not known, so each side starts clean.
///
/// `None` entries are for a language with no grammar: the caller draws the text plain.
pub fn highlight_diff_lines(lang: Option<Lang>, lines: &[(DiffSide, &str)]) -> Vec<Option<Vec<CodeToken>>> {
    let Some(lang) = lang else { return vec![None; lines.len()] };
    let mut out: Vec<Option<Vec<CodeToken>>> = vec![None; lines.len()];
    let mut scratch = Vec::new();
    let mut state = State::default();
    for (i, (side, text)) in lines.iter().enumerate() {
        if *side != DiffSide::Old {
            out[i] = Some(tokens_of(lang, &mut state, text, &mut scratch));
        }
    }
    let mut state = State::default();
    for (i, (side, text)) in lines.iter().enumerate() {
        if *side == DiffSide::New {
            continue;
        }
        let tokens = tokens_of(lang, &mut state, text, &mut scratch);
        if out[i].is_none() {
            out[i] = Some(tokens);
        }
    }
    out
}

/// A stretch of a line's tokens that a word diff marks, or leaves, as one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkedTokenRun {
    pub changed: bool,
    pub tokens: Vec<CodeToken>,
}

/// Cuts a line's tokens where its word-diff segments begin and end. Both are partitions of the same text, by different
/// rules: a token is a lexical shape, a segment is what changed. A changed run can start inside a string and end past
/// it, so the tokens are split at the segments' edges and regrouped under them.
pub fn mark_tokens(tokens: &[CodeToken], segments: &[WordDiffSegment]) -> Vec<MarkedTokenRun> {
    let mut runs = Vec::with_capacity(segments.len());
    let (mut token_index, mut token_offset) = (0, 0);
    for segment in segments {
        let mut run = MarkedTokenRun { changed: segment.changed, tokens: Vec::new() };
        let mut remaining = segment.text.len();
        while remaining > 0 && token_index < tokens.len() {
            let token = &tokens[token_index];
            let taken = (token.value.len() - token_offset).min(remaining);
            run.tokens.push(CodeToken { kind: token.kind, value: token.value[token_offset..token_offset + taken].to_owned() });
            remaining -= taken;
            token_offset += taken;
            if token_offset == token.value.len() {
                token_index += 1;
                token_offset = 0;
            }
        }
        if !run.tokens.is_empty() {
            runs.push(run);
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(tokens: &Option<Vec<CodeToken>>) -> Vec<(Token, String)> {
        tokens.iter().flatten().map(|t| (t.kind, t.value.clone())).collect()
    }

    #[test]
    fn colours_nothing_when_the_language_has_no_grammar() {
        let out = highlight_diff_lines(code_language("x.klingon"), &[(DiffSide::New, "const x = 1;")]);
        assert_eq!(out, [None]);
    }

    #[test]
    fn tokenizes_the_old_side_alone_so_a_deleted_block_comment_does_not_reach_an_added_line() {
        let lang = code_language("a.ts");
        let out = highlight_diff_lines(lang, &[(DiffSide::Old, "/* start"), (DiffSide::New, "const x = 1;")]);
        assert_eq!(kinds(&out[0]), [(Token::Comment, "/* start".to_owned())]);
        assert_eq!(kinds(&out[1]).first().unwrap().0, Token::Keyword);
    }

    #[test]
    fn a_context_line_reads_as_the_new_file_does_where_no_comment_is_open() {
        let lang = code_language("a.ts");
        let out = highlight_diff_lines(lang, &[(DiffSide::Old, "/* start"), (DiffSide::Both, "still open */")]);
        assert!(kinds(&out[1]).iter().all(|(k, _)| *k != Token::Comment), "{:?}", kinds(&out[1]));
    }

    #[test]
    fn carries_a_block_comment_an_added_line_opens_onto_the_context_line_after_it() {
        let lang = code_language("a.ts");
        let out = highlight_diff_lines(lang, &[(DiffSide::New, "/* start"), (DiffSide::Both, "still open */")]);
        assert_eq!(kinds(&out[0]), [(Token::Comment, "/* start".to_owned())]);
        assert_eq!(kinds(&out[1]), [(Token::Comment, "still open */".to_owned())]);
    }

    fn sample() -> Vec<CodeToken> {
        vec![CodeToken { kind: Token::Keyword, value: "const".into() }, CodeToken { kind: Token::Plain, value: " a = ".into() }, CodeToken { kind: Token::String, value: "\"x\"".into() }]
    }

    fn seg(text: &str, changed: bool) -> WordDiffSegment {
        WordDiffSegment { text: text.into(), changed }
    }

    #[test]
    fn cuts_the_tokens_where_the_segments_begin_and_end() {
        let runs = mark_tokens(&sample(), &[seg("const a = \"", false), seg("x", true), seg("\"", false)]);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].tokens.iter().map(|t| t.value.as_str()).collect::<Vec<_>>(), ["const", " a = ", "\""]);
        assert_eq!((runs[1].changed, runs[1].tokens[0].kind, runs[1].tokens[0].value.as_str()), (true, Token::String, "x"));
        assert_eq!(runs[2].tokens[0].value, "\"");
    }

    #[test]
    fn concatenates_back_to_the_original_text() {
        let runs = mark_tokens(&sample(), &[seg("con", true), seg("st a = \"x", false), seg("\"", true)]);
        let text: String = runs.iter().flat_map(|r| r.tokens.iter().map(|t| t.value.as_str())).collect();
        assert_eq!(text, "const a = \"x\"");
    }

    #[test]
    fn produces_no_run_for_an_empty_segment() {
        let runs = mark_tokens(&sample(), &[seg("", true), seg("const", false), seg("", false)]);
        assert_eq!(runs, [MarkedTokenRun { changed: false, tokens: vec![CodeToken { kind: Token::Keyword, value: "const".into() }] }]);
    }
}
