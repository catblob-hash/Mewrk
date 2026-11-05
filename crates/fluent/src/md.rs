//! Markdown for a stream, where the text is never complete.
//!
//! A normal parser wants the whole document, so running one on every token would be quadratic. This one is a
//! character-at-a-time state machine that never looks back. It holds back only the few characters needed to
//! decide what a line is ("#" might be a heading, "-" might be a list), which are invisible at reading speed.
//! Bold applies optimistically: `**` turns it on at once, and the line's end resets it if no closer ever arrives, so a
//! half-received style never garbles what follows. A single `*` is too ambiguous for that, so the little text after it
//! waits for a closer, and if none comes it was simply an asterisk.

use crate::text::{BlockKind, Doc, TextCtx};
use crate::theme::*;

#[derive(Clone, Copy)]
pub struct Palette {
    pub text: Color,
    pub muted: Color,
    pub code: Color,
}

/// Colours code blocks a line at a time. It lives outside this module because knowing languages is not a text engine's job.
pub trait CodeColors: Send {
    /// A fenced block starts, with whatever followed the opening fence.
    fn begin(&mut self, info: &str);
    /// Runs of one line: the byte offset each colour starts at. Nothing in `out` means the default colour.
    fn line(&mut self, line: &str, out: &mut Vec<(usize, Color)>);
}

pub struct MdStream {
    pub doc: Doc,
    code_colors: Option<Box<dyn CodeColors>>,
    fence_info: String,
    code_line: String,
    code_runs: Vec<(usize, Color)>,
    /// The newline after a code line. It only exists if another line follows, or every block would end in a blank one.
    code_newline: bool,
    palette: Palette,
    base: TypeStyle,
    head: String,
    at_line_start: bool,
    in_fence: bool,
    skip_fence_info: bool,
    bold: bool,
    italic: bool,
    code: bool,
    pending_star: bool,
    pending_backslash: bool,
    /// Text after a `*` that might open emphasis. It is held back until a closer settles what it was, because `int *p`
    /// looks the same as `*word*` at first, and text cannot be taken back once it is on screen.
    held_emphasis: Option<String>,
    /// The last character shown on this line, which decides whether a `*` can open emphasis.
    prev: Option<char>,
    line_style: TypeStyle,
    line_color: Color,
    saw_blank: bool,
    has_content: bool,
    last_was_list: bool,
    last_was_code: bool,
    out: String,
    out_style: Option<(TypeStyle, Color)>,
}

enum Head {
    Undecided,
    Paragraph,
    Fence,
    Heading(usize),
    Bullet,
    Numbered(usize),
    Quote,
    Table,
}

/// How much text waits for a closing `*`. Emphasis is a word or a few, and anything longer is not worth delaying.
const MAX_HELD_EMPHASIS: usize = 96;

fn is_rule(head: &str) -> bool {
    let first = head.as_bytes().first().copied().unwrap_or(0);
    head.len() >= 3 && matches!(first, b'-' | b'*' | b'_') && head.bytes().all(|b| b == first)
}

fn classify(head: &str) -> Head {
    let bytes = head.as_bytes();
    match bytes.first().copied().unwrap_or(0) {
        b'`' => {
            if head == "```" {
                Head::Fence
            } else if "```".starts_with(head) {
                Head::Undecided
            } else {
                Head::Paragraph
            }
        }
        b'#' => {
            let level = bytes.iter().take_while(|b| **b == b'#').count();
            if level > 6 {
                Head::Paragraph
            } else if level == bytes.len() {
                Head::Undecided
            } else if bytes[level] == b' ' {
                Head::Heading(level)
            } else {
                Head::Paragraph
            }
        }
        b'-' | b'*' | b'+' | b'_' => {
            let first = bytes[0];
            // A run of one marker might be a rule, but only the end of the line can say: `***Note:***` is not one.
            if bytes.iter().all(|b| *b == first) {
                return if bytes.len() < 64 { Head::Undecided } else { Head::Paragraph };
            }
            if first != b'_' && bytes[1] == b' ' { Head::Bullet } else { Head::Paragraph }
        }
        b'0'..=b'9' => {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 4 {
                Head::Paragraph
            } else if digits == bytes.len() {
                Head::Undecided
            } else if bytes[digits] != b'.' {
                Head::Paragraph
            } else if digits + 1 == bytes.len() {
                Head::Undecided
            } else if bytes[digits + 1] == b' ' {
                Head::Numbered(digits)
            } else {
                Head::Paragraph
            }
        }
        b'>' => {
            if bytes.len() == 1 {
                Head::Undecided
            } else if bytes[1] == b' ' {
                Head::Quote
            } else {
                Head::Paragraph
            }
        }
        b'|' => Head::Table,
        _ => Head::Paragraph,
    }
}

impl MdStream {
    pub fn new(width: f32, base: TypeStyle, palette: Palette) -> Self {
        Self {
            doc: Doc::new(width),
            code_colors: None,
            fence_info: String::new(),
            code_line: String::new(),
            code_runs: Vec::new(),
            code_newline: false,
            palette,
            base,
            head: String::new(),
            at_line_start: true,
            in_fence: false,
            skip_fence_info: false,
            bold: false,
            italic: false,
            code: false,
            pending_star: false,
            pending_backslash: false,
            held_emphasis: None,
            prev: None,
            line_style: base,
            line_color: palette.text,
            saw_blank: false,
            has_content: false,
            last_was_list: false,
            last_was_code: false,
            out: String::with_capacity(256),
            out_style: None,
        }
    }

    pub fn set_code_colors(&mut self, colors: Box<dyn CodeColors>) {
        self.code_colors = Some(colors);
    }

    pub fn push(&mut self, ctx: &mut TextCtx, s: &str) {
        for c in s.chars() {
            self.step(ctx, c);
        }
        self.flush(ctx);
    }

    /// The stream is complete, so anything held back is what it is.
    pub fn finish(&mut self, ctx: &mut TextCtx) {
        self.flush_code_line(ctx);
        self.release_pending(ctx);
        if !self.head.is_empty() {
            if !self.in_fence && is_rule(&self.head) {
                self.head.clear();
                self.start_rule(ctx);
                self.flush(ctx);
                return;
            }
            let head = std::mem::take(&mut self.head);
            if self.in_fence {
                self.release_code_newline(ctx);
                let (ty, color) = (MONO, self.palette.text);
                self.emit_styled(ctx, &head, ty, color);
            } else {
                // A head still undecided at the very end is only ever literal text: an opening marker that
                // nothing followed, such as a lone backtick.
                self.start_paragraph(ctx);
                let (ty, color) = self.current_style();
                self.emit_styled(ctx, &head, ty, color);
            }
            self.at_line_start = false;
        }
        self.flush(ctx);
    }

    fn step(&mut self, ctx: &mut TextCtx, c: char) {
        if self.skip_fence_info {
            if c == '\n' {
                self.skip_fence_info = false;
                self.at_line_start = true;
                if self.in_fence {
                    let info = std::mem::take(&mut self.fence_info);
                    if let Some(colors) = &mut self.code_colors {
                        colors.begin(info.trim());
                    }
                }
            } else if self.in_fence {
                self.fence_info.push(c);
            }
            return;
        }
        if self.in_fence {
            return self.step_fence(ctx, c);
        }
        if self.at_line_start {
            if c == '\n' {
                if self.head.is_empty() {
                    self.saw_blank = true;
                } else {
                    self.end_head_only_line(ctx);
                }
                return;
            }
            self.head.push(c);
            return self.decide_head(ctx);
        }
        if c == '\n' {
            self.end_line(ctx);
            return;
        }
        self.inline(ctx, c);
    }

    fn step_fence(&mut self, ctx: &mut TextCtx, c: char) {
        if self.at_line_start {
            if c == '`' && "```".starts_with(&format!("{}`", self.head)) {
                self.head.push(c);
                if self.head == "```" {
                    self.head.clear();
                    self.in_fence = false;
                    self.skip_fence_info = true;
                    self.last_was_code = true;
                    self.code_newline = false;
                }
                return;
            }
            self.at_line_start = false;
            if !self.head.is_empty() {
                let head = std::mem::take(&mut self.head);
                self.code_text(ctx, &head);
            }
        }
        if c == '\n' {
            self.flush_code_line(ctx);
            self.release_code_newline(ctx);
            self.code_newline = true;
            self.at_line_start = true;
        } else {
            self.code_text(ctx, c.encode_utf8(&mut [0; 4]));
        }
    }

    fn release_code_newline(&mut self, ctx: &mut TextCtx) {
        if std::mem::take(&mut self.code_newline) {
            self.emit_styled(ctx, "\n", MONO, self.palette.text);
        }
    }

    /// With colours, a line waits for its end, because what a word is depends on what follows it.
    fn code_text(&mut self, ctx: &mut TextCtx, s: &str) {
        if self.code_colors.is_some() {
            self.code_line.push_str(s);
        } else {
            self.release_code_newline(ctx);
            self.emit_styled(ctx, s, MONO, self.palette.text);
        }
    }

    fn flush_code_line(&mut self, ctx: &mut TextCtx) {
        if self.code_line.is_empty() {
            return;
        }
        self.release_code_newline(ctx);
        let line = std::mem::take(&mut self.code_line);
        let mut runs = std::mem::take(&mut self.code_runs);
        runs.clear();
        if let Some(colors) = &mut self.code_colors {
            colors.line(&line, &mut runs);
        }
        let text = self.palette.text;
        // A colourer that cuts a character in half, or runs backwards, costs the colours and never the text.
        let usable = runs.windows(2).all(|w| w[0].0 <= w[1].0) && runs.iter().all(|r| line.is_char_boundary(r.0));
        if runs.is_empty() || !usable {
            self.emit_styled(ctx, &line, MONO, text);
        } else {
            let first = runs[0].0;
            if first > 0 {
                self.emit_styled(ctx, &line[..first], MONO, text);
            }
            for (i, (start, color)) in runs.iter().enumerate() {
                let end = runs.get(i + 1).map_or(line.len(), |r| r.0);
                if let Some(piece) = line.get(*start..end) {
                    self.emit_styled(ctx, piece, MONO, *color);
                }
            }
        }
        self.code_runs = runs;
    }

    /// The line ended while its start was still undecided, so now it is decided: "---" is a rule and "42" is 42.
    fn end_head_only_line(&mut self, ctx: &mut TextCtx) {
        let head = std::mem::take(&mut self.head);
        if is_rule(&head) {
            self.start_rule(ctx);
        } else {
            self.start_paragraph(ctx);
            let (ty, color) = self.current_style();
            self.emit_styled(ctx, &head, ty, color);
        }
        self.end_line(ctx);
    }

    fn start_rule(&mut self, ctx: &mut TextCtx) {
        self.flush(ctx);
        let muted = self.palette.muted;
        self.doc.begin_block(ctx, BlockKind::Rule, 0.0, 8.0, self.base);
        self.after_block(self.base, muted, false);
        self.doc.push(ctx, " ", self.base, muted);
    }

    fn space_before(&self, list_item: bool) -> f32 {
        if !self.has_content {
            0.0
        } else if self.last_was_code {
            12.0
        } else if self.saw_blank {
            10.0
        } else if list_item && self.last_was_list {
            3.0
        } else {
            0.0
        }
    }

    fn start_paragraph(&mut self, ctx: &mut TextCtx) {
        self.flush(ctx);
        let space = self.space_before(false);
        self.doc.begin_block(ctx, BlockKind::Text, 0.0, space, self.base);
        self.after_block(self.base, self.palette.text, false);
    }

    fn after_block(&mut self, style: TypeStyle, color: Color, list: bool) {
        self.line_style = style;
        self.line_color = color;
        self.saw_blank = false;
        self.has_content = true;
        self.last_was_list = list;
        self.last_was_code = false;
        self.bold = false;
        self.italic = false;
        self.code = false;
        self.pending_star = false;
    }

    fn decide_head(&mut self, ctx: &mut TextCtx) {
        self.flush(ctx);
        let palette = self.palette;
        match classify(&self.head) {
            Head::Undecided => {}
            Head::Paragraph => {
                self.start_paragraph(ctx);
                self.at_line_start = false;
                let head = std::mem::take(&mut self.head);
                for c in head.chars() {
                    self.inline(ctx, c);
                }
            }
            Head::Fence => {
                self.head.clear();
                let space = if self.has_content { 12.0 } else { 0.0 };
                self.doc.begin_block(ctx, BlockKind::Code, 12.0, space, MONO);
                self.after_block(MONO, palette.text, false);
                self.in_fence = true;
                self.skip_fence_info = true;
            }
            Head::Heading(level) => {
                self.head.clear();
                let ty = match level {
                    1 => SUBTITLE,
                    2 => TypeStyle { size: 18.0, line: 26.0, weight: Weight::Semibold, mono: false, italic: false, serif: false },
                    3 => TypeStyle { size: 16.0, line: 24.0, weight: Weight::Semibold, mono: false, italic: false, serif: false },
                    _ => TypeStyle { weight: Weight::Semibold, ..self.base },
                };
                let space = if self.has_content { 16.0 } else { 0.0 };
                self.doc.begin_block(ctx, BlockKind::Heading, 0.0, space, ty);
                self.after_block(ty, palette.text, false);
                self.at_line_start = false;
            }
            Head::Bullet => {
                self.head.clear();
                let space = self.space_before(true);
                self.doc.begin_hanging_block(ctx, BlockKind::Text, 20.0, 14.0, space, self.base);
                self.after_block(self.base, palette.text, true);
                self.emit_styled(ctx, "•  ", self.base, palette.muted);
                self.at_line_start = false;
            }
            Head::Numbered(digits) => {
                let number = self.head[..digits + 1].to_owned();
                self.head.clear();
                let space = self.space_before(true);
                self.doc.begin_hanging_block(ctx, BlockKind::Text, 24.0, 18.0, space, self.base);
                self.after_block(self.base, palette.text, true);
                self.emit_styled(ctx, &format!("{number} "), self.base, palette.muted);
                self.at_line_start = false;
            }
            Head::Quote => {
                self.head.clear();
                let space = self.space_before(false);
                self.doc.begin_block(ctx, BlockKind::Quote, 14.0, space, self.base);
                self.after_block(self.base, palette.muted, false);
                self.at_line_start = false;
            }
            Head::Table => {
                let head = std::mem::take(&mut self.head);
                self.doc.begin_block(ctx, BlockKind::Text, 0.0, self.space_before(false), MONO);
                self.after_block(MONO, palette.text, false);
                self.code = true;
                self.at_line_start = false;
                self.emit_styled(ctx, &head, MONO, palette.text);
            }
        }
    }

    fn release_pending(&mut self, ctx: &mut TextCtx) {
        if self.pending_star {
            self.pending_star = false;
            self.emit(ctx, '*');
        }
        if self.pending_backslash {
            self.pending_backslash = false;
            self.emit(ctx, '\\');
        }
        self.release_held_literally(ctx, false);
    }

    /// No closer came, so the `*` was just an asterisk, and the text behind it was plain all along.
    fn release_held_literally(&mut self, ctx: &mut TextCtx, closing_star: bool) {
        let Some(held) = self.held_emphasis.take() else { return };
        self.emit(ctx, '*');
        let (ty, color) = self.current_style();
        self.emit_styled(ctx, &held, ty, color);
        if closing_star {
            self.emit(ctx, '*');
        }
        self.prev = if closing_star { Some('*') } else { held.chars().next_back().or(Some('*')) };
    }

    fn end_line(&mut self, ctx: &mut TextCtx) {
        self.release_pending(ctx);
        self.flush(ctx);
        self.at_line_start = true;
        self.saw_blank = false;
        self.bold = false;
        self.italic = false;
        self.code = false;
        self.prev = None;
    }

    /// Multiplication and pointers are as common as emphasis in what models write, so `*` only opens emphasis at the
    /// start of a word. CJK text has no spaces, which is why this looks at ASCII only.
    fn can_open_emphasis(&self) -> bool {
        !self.prev.is_some_and(|p| p.is_ascii_alphanumeric())
    }

    fn inline(&mut self, ctx: &mut TextCtx, c: char) {
        if self.pending_backslash {
            self.pending_backslash = false;
            // Only punctuation can be escaped. `C:\Users` is a path.
            if c.is_ascii_punctuation() {
                self.emit(ctx, c);
                self.prev = Some(c);
                return;
            }
            self.emit(ctx, '\\');
            self.prev = Some('\\');
        }
        if self.code {
            // Table rows are mono throughout and have no closing marker to look for.
            if c == '`' && !self.line_style.mono {
                self.code = false;
            } else {
                self.emit(ctx, c);
            }
            self.prev = Some(c);
            return;
        }
        if self.pending_star {
            self.pending_star = false;
            if c == '*' {
                if self.bold || self.can_open_emphasis() {
                    self.bold = !self.bold;
                } else {
                    self.emit(ctx, '*');
                    self.emit(ctx, '*');
                    self.prev = Some('*');
                }
                return;
            }
            if c.is_whitespace() || !self.can_open_emphasis() {
                self.emit(ctx, '*');
                self.prev = Some('*');
            } else {
                self.held_emphasis = Some(String::new());
            }
            // Fall through: `c` still has to be handled.
        }
        if let Some(held) = &mut self.held_emphasis {
            if c == '*' {
                // A closer has to touch the word it closes. `*p = *q` has two asterisks and no emphasis.
                if held.is_empty() || held.ends_with(char::is_whitespace) {
                    self.release_held_literally(ctx, true);
                } else {
                    let held = self.held_emphasis.take().unwrap_or_default();
                    self.italic = true;
                    let (ty, color) = self.current_style();
                    self.emit_styled(ctx, &held, ty, color);
                    self.italic = false;
                    self.prev = held.chars().next_back();
                }
                return;
            }
            held.push(c);
            if held.len() > MAX_HELD_EMPHASIS {
                self.release_held_literally(ctx, false);
            }
            return;
        }
        match c {
            '\\' => self.pending_backslash = true,
            '*' => self.pending_star = true,
            '`' => self.code = true,
            _ => {
                self.emit(ctx, c);
                self.prev = Some(c);
            }
        }
    }

    fn current_style(&self) -> (TypeStyle, Color) {
        if self.code && !self.line_style.mono {
            return (TypeStyle { mono: true, size: self.line_style.size - 1.0, ..self.line_style }, self.palette.code);
        }
        let mut ty = self.line_style;
        if self.bold {
            ty.weight = Weight::Semibold;
        }
        ty.italic = self.italic;
        (ty, self.line_color)
    }

    fn emit(&mut self, ctx: &mut TextCtx, c: char) {
        let (ty, color) = self.current_style();
        self.emit_styled(ctx, c.encode_utf8(&mut [0; 4]), ty, color);
    }

    fn emit_styled(&mut self, ctx: &mut TextCtx, s: &str, ty: TypeStyle, color: Color) {
        if self.out_style != Some((ty, color)) {
            self.flush(ctx);
            self.out_style = Some((ty, color));
        }
        self.out.push_str(s);
    }

    fn flush(&mut self, ctx: &mut TextCtx) {
        if !self.out.is_empty() {
            if let Some((ty, color)) = self.out_style {
                self.doc.push(ctx, &self.out, ty, color);
            }
            self.out.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette { text: Color::WHITE, muted: Color::rgb(0x888888), code: Color::rgb(0x00FF00) }
    }

    fn render(chunks: &[&str]) -> (String, Vec<BlockKind>) {
        let mut ctx = TextCtx::new(2.0);
        let mut md = MdStream::new(4000.0, BODY, palette());
        for c in chunks {
            md.push(&mut ctx, c);
        }
        md.finish(&mut ctx);
        let kinds = md.doc.lines.iter().map(|l| l.kind).collect();
        (md.doc.text().to_owned(), kinds)
    }

    const DOC: &str = "# Title\nSome **bold** and *italic* and `code` text.\n\n- one\n- two\n\n1. first\n2. second\n\n> quoted\n\n```rust\nlet a = 1;\nlet b = 2 * 3;\n```\nAfter 2*3=6 and a*b.\n---\nend";

    #[test]
    fn markers_are_consumed_and_text_survives() {
        let (text, kinds) = render(&[DOC]);
        for gone in ["#", "**", "`", "```", "---", "rust"] {
            assert!(!text.contains(gone), "{gone:?} still in {text:?}");
        }
        for kept in ["Title", "Some bold and italic and code text.", "•  one", "1. first", "2. second", "quoted", "let a = 1;", "let b = 2 * 3;", "end"] {
            assert!(text.contains(kept), "{kept:?} missing from {text:?}");
        }
        assert_eq!(kinds[0], BlockKind::Heading);
        assert!(kinds.contains(&BlockKind::Code));
        assert!(kinds.contains(&BlockKind::Quote));
        assert!(kinds.contains(&BlockKind::Rule));
    }

    /// The property that makes this usable for streaming: tokenisation must not change the outcome.
    #[test]
    fn every_chunking_renders_the_same() {
        let (whole, whole_kinds) = render(&[DOC]);
        let chars: Vec<char> = DOC.chars().collect();
        for size in 1..=17 {
            let chunks: Vec<String> = chars.chunks(size).map(|c| c.iter().collect()).collect();
            let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
            let (text, kinds) = render(&refs);
            assert_eq!(text, whole, "chunk size {size}");
            assert_eq!(kinds, whole_kinds, "chunk size {size}");
        }
    }

    #[test]
    fn multiplication_is_not_mistaken_for_emphasis_and_arithmetic_survives() {
        for s in ["Compute 3 * 4 = 12 today", "12345*6789", "a*b*c", "2**10 is 1024", "int *p = &x;", "*p = *q", "x *", "*"] {
            let (text, _) = render(&[s]);
            assert_eq!(text, s);
        }
    }

    #[test]
    fn single_star_text_is_held_only_until_it_can_be_decided() {
        let mut ctx = TextCtx::new(2.0);
        let mut md = MdStream::new(4000.0, BODY, palette());
        md.push(&mut ctx, "before *wo");
        assert_eq!(md.doc.text(), "before ", "undecided text waits");
        md.push(&mut ctx, "rd* after");
        assert_eq!(md.doc.text(), "before word after");

        // Nothing closes it: it has to come out as written, at the end of the line at the latest.
        let mut md = MdStream::new(4000.0, BODY, palette());
        md.push(&mut ctx, "int *p = &x;\nnext");
        assert_eq!(md.doc.text(), "int *p = &x;\nnext");

        // And a very long run stops waiting.
        let mut md = MdStream::new(4000.0, BODY, palette());
        let long = "x".repeat(200);
        md.push(&mut ctx, &format!("*{long}"));
        assert!(md.doc.text().starts_with("*xxx"), "{:?}", md.doc.text());
    }

    #[test]
    fn emphasis_still_works_where_it_should() {
        for (s, want) in [("*word*", "word"), ("**word**", "word"), ("***word*** rest", "word rest"), ("这是**粗体**文字", "这是粗体文字"), ("(*x*)", "(x)")] {
            let (text, _) = render(&[s]);
            assert_eq!(text, want, "input {s:?}");
        }
    }

    #[test]
    fn a_line_that_starts_with_markers_is_not_lost() {
        let (text, kinds) = render(&["***Important:*** read this"]);
        assert_eq!(text, "Important: read this");
        assert!(!kinds.contains(&BlockKind::Rule));
        let (_, kinds) = render(&["before\n---\nafter"]);
        assert!(kinds.contains(&BlockKind::Rule), "a line of only markers is still a rule");
        for s in ["---", "***", "___", "-----"] {
            let (_, kinds) = render(&[s]);
            assert_eq!(kinds, vec![BlockKind::Rule], "{s:?}");
        }
        let (text, _) = render(&["--flag value"]);
        assert_eq!(text, "--flag value");
    }

    #[test]
    fn lines_made_of_only_a_number_stay_separate_lines() {
        let (text, _) = render(&["42\nThat is it"]);
        assert_eq!(text, "42\nThat is it");
        let (text, _) = render(&["1\n2\n3"]);
        assert_eq!(text, "1\n2\n3");
    }

    #[test]
    fn a_backslash_only_escapes_punctuation() {
        let (text, _) = render(&["C:\\Users\\me and \\*literal\\* end\\"]);
        assert_eq!(text, "C:\\Users\\me and *literal* end\\");
    }

    #[test]
    fn unterminated_markers_do_not_leak_past_the_line() {
        let mut ctx = TextCtx::new(2.0);
        let mut md = MdStream::new(4000.0, BODY, palette());
        md.push(&mut ctx, "**never closed\nplain");
        assert!(!md.bold, "bold must reset at the end of the line");
        assert_eq!(md.doc.text(), "never closed\nplain");
    }

    #[test]
    fn held_back_heads_are_released_at_the_end() {
        for s in ["1", "#", "-", "`", "12."] {
            let (text, _) = render(&[s]);
            assert_eq!(text, s, "lost {s:?}");
        }
    }

    #[test]
    fn numbers_at_a_line_start_are_not_lists_unless_they_are() {
        let (text, _) = render(&["2026 was a good year\n3.5 is a number\n4. is a list"]);
        assert!(text.contains("2026 was a good year"));
        assert!(text.contains("3.5 is a number"));
        assert!(text.contains("4. is a list"));
    }

    #[test]
    fn a_code_block_does_not_end_in_a_blank_line() {
        let (text, _) = render(&["```\na\n\nb\n```\nafter"]);
        assert_eq!(text, "a\n\nb\nafter");
    }

    #[test]
    fn colours_split_a_line_without_losing_any_of_it() {
        struct Stars;
        impl CodeColors for Stars {
            fn begin(&mut self, info: &str) {
                assert_eq!(info, "rust");
            }
            fn line(&mut self, line: &str, out: &mut Vec<(usize, Color)>) {
                if line.len() > 2 {
                    out.push((1, Color::WHITE));
                    out.push((2, Color::rgb(0x123456)));
                }
            }
        }
        let mut ctx = TextCtx::new(1.0);
        let mut md = MdStream::new(400.0, BODY, palette());
        md.set_code_colors(Box::new(Stars));
        md.push(&mut ctx, "```rust\nabcd\n世\n```");
        md.finish(&mut ctx);
        assert_eq!(md.doc.text(), "abcd\n世");
    }

    #[test]
    fn code_blocks_keep_their_content_verbatim() {
        let (text, _) = render(&["```\n**not bold** `x` # y\n- z\n```"]);
        assert!(text.contains("**not bold** `x` # y"), "{text:?}");
        assert!(text.contains("- z"), "{text:?}");
    }
}
