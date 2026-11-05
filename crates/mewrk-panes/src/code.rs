//! A read-only document of numbered code rows, which is what a diff and a file preview both are.
//!
//! A document is built whole on a background thread, colours and word marks included, so that painting only copies the
//! rows in view however long it is. Text sits on a grid, where a character's position is arithmetic rather than shaping,
//! and that is what lets a row's height be known without laying it out. A CJK or emoji character takes 5 grid units and
//! anything else 3, which is the ratio of a 12px em to a 7.2px monospace advance: columns stay columns, and CJK keeps
//! its natural spacing.

use fluent::text::{is_cjk, is_emoji};
use fluent::theme::{CAPTION, FINE, MONO_BODY, MONO_SMALL};
use fluent::{rect, Color, Rect, Ui};

use crate::highlight::{self, Lang, State, Token};

const LINE_H: u32 = MONO_BODY.line as u32;
const BAND_H: u32 = 24;
const GUTTER_PAD: f32 = 8.0;
const TAB_STOP: u32 = 2;
/// Shown of any one line. Past this a line is nothing anyone reads, and every per-row cost is bounded by it.
const MAX_LINE_BYTES: usize = 32 * 1024;
/// A pane is never narrower than this many columns of code, however small it gets.
const MIN_COLS: u32 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowKind {
    Code,
    Add,
    Del,
    /// The break between hunks: `no` lines left out, and the hunk's heading as its text.
    Sep,
    Note,
}

#[derive(Clone, Copy)]
pub(crate) struct Row {
    start: u32,
    len: u32,
    /// Width on the grid.
    units: u32,
    tok: u32,
    n_tok: u32,
    mark: u32,
    n_mark: u32,
    no: u32,
    pub(crate) kind: RowKind,
    ascii: bool,
}

/// How a document is drawn, which the pane owns rather than the document.
#[derive(Clone, Copy)]
pub(crate) struct View {
    /// Width of one grid cell.
    pub(crate) cw: f32,
    pub(crate) wrap: bool,
    pub(crate) words: bool,
}

pub(crate) struct CodeDoc {
    text: String,
    pub(crate) rows: Vec<Row>,
    tokens: Vec<(u32, Token)>,
    marks: Vec<(u32, u32)>,
    max_no: u32,
    /// Whether rows keep a column for the `+` and `-` of a diff.
    markers: bool,
    pub(crate) additions: u64,
    pub(crate) deletions: u64,
    /// Top of each row in logical pixels, and the document's height last. Whole pixels, so that no row drifts however
    /// far down it is: at a few million pixels an `f32` no longer holds fractions.
    offsets: Vec<u32>,
    laid_out: Option<(u32, bool)>,
    cleaned: String,
    scanned: Vec<(usize, Token)>,
}

const NARROW: u32 = 3;
const WIDE: u32 = 5;

fn cell_width(c: char) -> u32 {
    match c as u32 {
        0x300..=0x36F | 0x200B..=0x200F | 0xFE00..=0xFE0F => 0,
        _ if is_cjk(c) || is_emoji(c) => WIDE,
        _ => NARROW,
    }
}

/// `text` with its tabs expanded and control characters blanked, which a font would draw as boxes.
fn cleaned<'a>(text: &'a str, buf: &'a mut String) -> &'a str {
    if !text.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return text;
    }
    buf.clear();
    let mut col = 0;
    for c in text.chars() {
        match c {
            '\t' => {
                let n = TAB_STOP - (col / NARROW) % TAB_STOP;
                buf.extend(std::iter::repeat_n(' ', n as usize));
                col += n * NARROW;
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                buf.push(' ');
                col += NARROW;
            }
            c => {
                buf.push(c);
                col += cell_width(c);
            }
        }
    }
    buf
}

/// The part of `text` a row keeps, and whether anything was left out.
fn clipped(text: &str) -> (&str, bool) {
    if text.len() <= MAX_LINE_BYTES {
        return (text, false);
    }
    let mut end = MAX_LINE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

impl CodeDoc {
    pub(crate) fn new(markers: bool) -> Self {
        Self {
            text: String::new(),
            rows: Vec::new(),
            tokens: Vec::new(),
            marks: Vec::new(),
            max_no: 0,
            markers,
            additions: 0,
            deletions: 0,
            offsets: Vec::new(),
            laid_out: None,
            cleaned: String::new(),
            scanned: Vec::new(),
        }
    }

    /// A document of a whole file, one numbered row per line.
    pub(crate) fn of_text(text: &str, lang: Option<Lang>) -> Self {
        let mut doc = Self::new(false);
        doc.text.reserve(text.len());
        let mut state = State::default();
        for (n, line) in text.lines().enumerate() {
            doc.push_code(RowKind::Code, n as u32 + 1, line, lang, &mut state);
        }
        doc
    }

    /// A document that is only a sentence, for what has no lines to show.
    pub(crate) fn note(text: &str) -> Self {
        let mut doc = Self::new(true);
        doc.push_band(RowKind::Note, 0, text);
        doc
    }

    pub(crate) fn push_code(&mut self, kind: RowKind, no: u32, raw: &str, lang: Option<Lang>, state: &mut State) {
        let (raw, cut) = clipped(raw);
        let mut buf = std::mem::take(&mut self.cleaned);
        let mut scanned = std::mem::take(&mut self.scanned);
        let line = cleaned(raw, &mut buf);

        let tok = self.tokens.len() as u32;
        match lang {
            Some(lang) => {
                highlight::line(lang, state, line, &mut scanned);
                self.tokens.extend(scanned.iter().map(|&(at, t)| (at as u32, t)));
            }
            None => scanned.clear(),
        }
        let n_tok = self.tokens.len() as u32 - tok;

        let start = self.text.len() as u32;
        self.text.push_str(line);
        let ascii = line.is_ascii();
        let units = if ascii { line.len() as u32 * NARROW } else { line.chars().map(cell_width).sum() };
        self.rows.push(Row { start, len: line.len() as u32, units, tok, n_tok, mark: 0, n_mark: 0, no, kind, ascii });
        self.max_no = self.max_no.max(no);
        self.cleaned = buf;
        self.scanned = scanned;
        if cut {
            self.append_to_last(" …");
        }
        self.laid_out = None;
    }

    /// Adds a dim suffix to the row just pushed.
    pub(crate) fn append_to_last(&mut self, suffix: &str) {
        let at = self.text.len() as u32;
        self.text.push_str(suffix);
        let Some(row) = self.rows.last_mut() else { return };
        if row.n_tok == 0 && at > row.start {
            self.tokens.push((0, Token::Plain));
        }
        self.tokens.push((at - row.start, Token::Comment));
        row.n_tok = self.tokens.len() as u32 - row.tok;
        row.len += suffix.len() as u32;
        row.units += suffix.chars().map(cell_width).sum::<u32>();
        row.ascii = row.ascii && suffix.is_ascii();
        self.laid_out = None;
    }

    pub(crate) fn push_band(&mut self, kind: RowKind, no: u32, text: &str) {
        let (text, _) = clipped(text);
        let start = self.text.len() as u32;
        self.text.push_str(text);
        self.rows.push(Row { start, len: text.len() as u32, units: 0, tok: 0, n_tok: 0, mark: 0, n_mark: 0, no, kind, ascii: true });
        self.laid_out = None;
    }

    pub(crate) fn set_marks(&mut self, row: usize, marks: &[(u32, u32)]) {
        self.rows[row].mark = self.marks.len() as u32;
        self.rows[row].n_mark = marks.len() as u32;
        self.marks.extend_from_slice(marks);
    }

    pub(crate) fn row_text(&self, row: usize) -> &str {
        let r = &self.rows[row];
        &self.text[r.start as usize..(r.start + r.len) as usize]
    }

    #[cfg(test)]
    pub(crate) fn row_no(&self, row: usize) -> u32 {
        self.rows[row].no
    }

    #[cfg(test)]
    pub(crate) fn row_token_kinds(&self, row: usize) -> Vec<Token> {
        self.row_tokens(&self.rows[row]).iter().map(|t| t.1).collect()
    }

    #[cfg(test)]
    pub(crate) fn row_marks(&self, row: usize) -> &[(u32, u32)] {
        let r = &self.rows[row];
        &self.marks[r.mark as usize..(r.mark + r.n_mark) as usize]
    }

    fn row_tokens(&self, r: &Row) -> &[(u32, Token)] {
        &self.tokens[r.tok as usize..(r.tok + r.n_tok) as usize]
    }

    fn digits(&self) -> u32 {
        self.max_no.max(1).ilog10() + 1
    }

    fn code_x(&self, cw: f32) -> f32 {
        let gutter = if self.max_no > 0 { GUTTER_PAD + self.digits() as f32 * cw + GUTTER_PAD } else { GUTTER_PAD };
        gutter + if self.markers { 2.0 * cw } else { 0.0 }
    }

    /// Columns of code that fit in `w`.
    pub(crate) fn cols_for(&self, w: f32, cw: f32) -> u32 {
        (((w - self.code_x(cw)) / cw).floor() as i64).max(MIN_COLS as i64) as u32
    }

    fn visual_lines(&self, r: &Row, cols: u32) -> u32 {
        let room = cols * NARROW;
        if r.units <= room {
            return 1;
        }
        if r.ascii {
            return r.units.div_ceil(room);
        }
        // Greedy, exactly as `paint_text` breaks: a character that does not fit moves down whole.
        let (mut lines, mut col) = (1, 0);
        for c in self.text[r.start as usize..(r.start + r.len) as usize].chars() {
            let w = cell_width(c);
            if col + w > room {
                lines += 1;
                col = 0;
            }
            col += w;
        }
        lines
    }

    /// Measures every row for this width. Cheap enough to do when the width changes, and skipped when it has not.
    /// True when anything moved.
    pub(crate) fn layout(&mut self, cols: u32, wrap: bool) -> bool {
        let cols = cols.max(MIN_COLS);
        if self.laid_out == Some((cols, wrap)) {
            return false;
        }
        self.laid_out = Some((cols, wrap));
        let mut offsets = std::mem::take(&mut self.offsets);
        offsets.clear();
        offsets.reserve(self.rows.len() + 1);
        let mut y = 0u32;
        for r in &self.rows {
            offsets.push(y);
            y += match r.kind {
                RowKind::Sep | RowKind::Note => BAND_H,
                _ if wrap => self.visual_lines(r, cols) * LINE_H,
                _ => LINE_H,
            };
        }
        offsets.push(y);
        self.offsets = offsets;
        true
    }

    pub(crate) fn height(&self) -> u32 {
        self.offsets.last().copied().unwrap_or(0)
    }

    /// Paints the rows that intersect the current clip, with the document's top at screen height `top` and `w` of width.
    /// Only ever touches rows in view, so its cost has nothing to do with how long the document is.
    pub(crate) fn paint(&self, ui: &mut Ui<'_>, x: f32, w: f32, top: f64, view: View) {
        let View { cw, wrap, words } = view;
        let n = self.rows.len();
        if self.offsets.len() != n + 1 || n == 0 {
            return;
        }
        // Whole physical pixels, so that a fractional scroll position cannot open hairlines between row bands.
        let top = (top * ui.scale as f64).round() / ui.scale as f64;
        let clip = ui.clip;
        let seen = (clip.y as f64 - top).max(0.0) as u32;
        let first = self.offsets[..n].partition_point(|&o| o <= seen).saturating_sub(1);

        let t = ui.theme;
        let code_x = x + self.code_x(cw);
        let mut pen = Pen {
            colors: [Token::Plain, Token::Keyword, Token::String, Token::Number, Token::Comment, Token::Function, Token::Type, Token::Meta, Token::Tag, Token::Attribute, Token::Property, Token::Variable].map(|k| highlight::color(k, &t)),
            unit: cw / NARROW as f32,
            cols: self.laid_out.map_or(MIN_COLS, |l| l.0),
            wrap,
            words,
            mark: Color::TRANSPARENT,
        };
        for i in first..n {
            let y = (top + self.offsets[i] as f64) as f32;
            if y >= clip.bottom() {
                break;
            }
            let r = &self.rows[i];
            let h = (self.offsets[i + 1] - self.offsets[i]) as f32;
            match r.kind {
                RowKind::Sep => {
                    self.paint_sep(ui, r, rect(x, y, w, h));
                    continue;
                }
                RowKind::Note => {
                    let note = self.row_text(i);
                    ui.label_ellipsis(x + 10.0, y + (h - CAPTION.line) * 0.5, (w - 20.0).max(0.0), note, &CAPTION, t.text_tertiary);
                    continue;
                }
                _ => {}
            }
            let (number, marker) = match r.kind {
                RowKind::Add => {
                    ui.fill(rect(x, y, w, h), 0.0, t.success.with_alpha(0.13));
                    pen.mark = t.success.with_alpha(0.28);
                    (t.success, Some(("+", t.success)))
                }
                RowKind::Del => {
                    ui.fill(rect(x, y, w, h), 0.0, t.critical.with_alpha(0.13));
                    pen.mark = t.critical.with_alpha(0.28);
                    (t.critical, Some(("-", t.critical)))
                }
                _ => (t.text_tertiary, None),
            };
            if r.no > 0 {
                let mut buf = [0u8; 10];
                let digits = write_number(r.no, &mut buf);
                let at = x + GUTTER_PAD + (self.digits() - digits.len() as u32) as f32 * cw;
                ui.label(at, y, digits, &MONO_BODY, number);
            }
            if let Some((text, color)) = marker {
                ui.label(code_x - 2.0 * cw, y, text, &MONO_BODY, color);
            }
            self.paint_text(ui, r, code_x, y, &pen);
        }
    }

    fn paint_sep(&self, ui: &mut Ui<'_>, r: &Row, b: Rect) {
        let t = ui.theme;
        ui.fill(b, 0.0, t.subtle_hover);
        let mut x = b.x + 10.0;
        if r.no > 0 {
            x += ui.label(x, b.y + (b.h - FINE.line) * 0.5, &format!("{} 行未改动", r.no), &FINE, t.text_secondary) + 8.0;
        }
        let heading = &self.text[r.start as usize..(r.start + r.len) as usize];
        if !heading.is_empty() {
            ui.label_ellipsis(x, b.y + (b.h - MONO_SMALL.line) * 0.5, (b.right() - x - 14.0).max(0.0), heading, &MONO_SMALL, t.text_tertiary);
        }
    }

    /// Walks the row in runs that share a colour, a mark and a grid width, and draws those in view. Plain ASCII starts
    /// directly at its first visible byte. Anything else has to be walked from the row's start, which `MAX_LINE_BYTES`
    /// keeps short.
    fn paint_text(&self, ui: &mut Ui<'_>, r: &Row, x: f32, y: f32, pen: &Pen) {
        let text = &self.text[r.start as usize..(r.start + r.len) as usize];
        if text.is_empty() {
            return;
        }
        let tokens = self.row_tokens(r);
        let marks = if pen.words { &self.marks[r.mark as usize..(r.mark + r.n_mark) as usize] } else { &[][..] };
        let line_h = LINE_H as f32;
        let clip = ui.clip;
        let (first, last) = if pen.wrap { (((clip.y - y) / line_h).floor().max(0.0) as u32, ((clip.bottom() - y) / line_h).ceil().max(0.0) as u32) } else { (0, 1) };

        let room = pen.cols * NARROW;
        let (mut at, mut vis, mut col) = (0usize, 0u32, 0u32);
        if r.ascii {
            vis = first;
            at = (first as usize).saturating_mul(pen.cols as usize);
        }
        let mut ti = tokens.partition_point(|t| t.0 as usize <= at).saturating_sub(1);
        let mut mi = marks.partition_point(|m| m.1 as usize <= at);
        while at < text.len() && vis < last {
            let tok_end = tokens.get(ti + 1).map_or(text.len(), |t| t.0 as usize);
            let mark = marks.get(mi).copied();
            let mark_edge = mark.map_or(usize::MAX, |m| if m.0 as usize <= at { m.1 as usize } else { m.0 as usize });
            let limit = tok_end.min(mark_edge).min(text.len());
            let (end, units) = if r.ascii {
                let end = limit.min(at + ((room - col) / NARROW) as usize);
                (end, (end - at) as u32 * NARROW)
            } else {
                run_at(text, at, limit, room - col)
            };
            if end == at {
                // A character that does not fit. Unwrapped, nothing further along can be in view either.
                if !pen.wrap {
                    break;
                }
                vis += 1;
                col = 0;
                continue;
            }
            if vis >= first {
                let piece = &text[at..end];
                let (px, py) = (x + col as f32 * pen.unit, y + vis as f32 * line_h);
                if let Some(m) = mark
                    && m.0 as usize <= at
                {
                    // Rounded only at the mark's own ends, or the seams between colours would show through it.
                    let (left, right) = (if m.0 as usize == at { 2.0 } else { 0.0 }, if m.1 as usize == end { 2.0 } else { 0.0 });
                    ui.fill_radii(rect(px, py, units as f32 * pen.unit, line_h), [left, right, right, left], pen.mark);
                }
                if !piece.bytes().all(|b| b == b' ') {
                    let kind = tokens.get(ti).map_or(Token::Plain, |t| t.1);
                    ui.label(px, py, piece, &MONO_BODY, pen.colors[kind as usize]);
                }
            }
            at = end;
            col += units;
            if col >= room {
                if !pen.wrap {
                    break;
                }
                vis += 1;
                col = 0;
            }
            while tokens.get(ti + 1).is_some_and(|t| t.0 as usize <= at) {
                ti += 1;
            }
            while marks.get(mi).is_some_and(|m| m.1 as usize <= at) {
                mi += 1;
            }
        }
    }
}

struct Pen {
    colors: [Color; 12],
    /// Width of one grid unit.
    unit: f32,
    cols: u32,
    wrap: bool,
    words: bool,
    mark: Color,
}

/// The longest run from `at` that fits in `room` units and shares a grid width. Wide characters come one at a time, so
/// that a font whose CJK is not quite 5/3 of its Latin cannot run on into what follows.
fn run_at(text: &str, at: usize, limit: usize, room: u32) -> (usize, u32) {
    let Some(c) = text[at..limit].chars().next() else { return (at, 0) };
    if cell_width(c) == WIDE {
        return if room >= WIDE { (at + c.len_utf8(), WIDE) } else { (at, 0) };
    }
    let (mut end, mut units) = (at, 0);
    for c in text[at..limit].chars() {
        let w = cell_width(c);
        if w == WIDE || units + w > room {
            break;
        }
        end += c.len_utf8();
        units += w;
    }
    (end, units)
}

fn write_number(mut v: u32, buf: &mut [u8; 10]) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    // Only ASCII digits were written.
    std::str::from_utf8(&buf[i..]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_and_controls_are_cleaned_up_front() {
        let mut buf = String::new();
        assert_eq!(cleaned("plain", &mut buf), "plain");
        assert_eq!(cleaned("\ta\tb", &mut buf), "  a b");
        assert_eq!(cleaned("x\u{1b}[0m", &mut buf), "x [0m");
    }

    #[test]
    fn rows_wrap_on_the_grid() {
        let mut doc = CodeDoc::new(false);
        let mut st = State::default();
        doc.push_code(RowKind::Code, 1, &"a".repeat(25), None, &mut st);
        doc.push_code(RowKind::Code, 2, "", None, &mut st);
        doc.push_code(RowKind::Code, 3, &"中".repeat(5), None, &mut st);
        doc.layout(10, true);
        assert_eq!(doc.offsets, vec![0, 3 * LINE_H, 4 * LINE_H, 5 * LINE_H]);
        doc.layout(8, true);
        // Twenty-five units of CJK against 24 of room: four characters fit, and the fifth moves down whole.
        assert_eq!(doc.offsets[3], 4 * LINE_H + LINE_H + 2 * LINE_H);
        doc.layout(8, false);
        assert_eq!(doc.height(), 3 * LINE_H);
    }

    #[test]
    fn a_very_long_line_is_cut_at_a_character_boundary() {
        let mut doc = CodeDoc::new(false);
        doc.push_code(RowKind::Code, 1, &"中".repeat(MAX_LINE_BYTES), crate::highlight::lang_of("rs"), &mut State::default());
        assert!(doc.row_text(0).len() < MAX_LINE_BYTES + 8);
        assert!(doc.row_text(0).ends_with(" …"));
    }

    #[test]
    fn numbers_write_without_allocating() {
        let mut buf = [0u8; 10];
        assert_eq!(write_number(0, &mut buf), "0");
        assert_eq!(write_number(4_294_967_295, &mut buf), "4294967295");
    }
}
