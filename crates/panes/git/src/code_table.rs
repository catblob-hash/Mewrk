//! The lines of a diff, painted. Upstream draws these as a CSS grid of spans; here one leaf paints only the rows in view,
//! so its cost has nothing to do with how long the file is, and a file expanded to 100,000 lines of context scrolls like
//! one of ten. Text sits on a grid where a character's position is arithmetic rather than shaping: a CJK or emoji
//! character takes 5 grid units and anything else 3, the ratio of a 12px em to a 7.2px monospace advance. That is what
//! lets a row's height be known without laying it out, and it is lifted from `mewrk_panes::code`.

use fluent::text::{is_cjk, is_emoji};
use mewrk_client::prelude::*;
use mewrk_panes::highlight::{self, Token};

use crate::diff_highlight::{highlight_diff_lines, mark_tokens, CodeToken, DiffSide};
use crate::unified_diff::{DiffHunk, DiffLine, DiffLineKind};
use crate::word_diff::{word_diff, WordDiffSegment};

const TY: TypeStyle = MONO_BODY;
const LINE_H: f32 = 17.0;
const BAND_H: f32 = 32.0;
const NOTE_H: f32 = 22.0;
const BOTTOM_PAD: f32 = 8.0;
/// `.diff-viewer__hunk-separator`'s inline padding, and the expand button with the gap after it.
const BAND_PAD: f32 = 6.0;
const BAND_BUTTON: f32 = 20.0;
const NARROW: u32 = 3;
const WIDE: u32 = 5;
const TAB_STOP: u32 = 2;
/// Shown of any one line. Past this a line is nothing anyone reads, and every per-row cost is bounded by it.
const MAX_LINE_BYTES: usize = 32 * 1024;
const MIN_COLS: u32 = 8;
/// The marker column and the space after it, and one more after the text: `padding-inline: 2ch 1ch`.
const TEXT_LEAD: f32 = 2.0;
const TEXT_TRAIL: f32 = 1.0;
const SCROLLBAR_H: f32 = 6.0;
/// `.diff-output__line`: `font: 10px/20px var(--mono)`.
const CARD_TY: TypeStyle = TypeStyle { size: 10.0, line: 20.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
/// `.diff-output__line--meta`.
const CARD_NOTE: TypeStyle = TypeStyle { italic: true, ..CARD_TY };
const CARD_LINE_H: f32 = 20.0;
/// `.diff-output__line-number`'s `border-right`, which the gutter's width counts.
const CARD_RULE: f32 = 1.0;
/// `.diff-output__content { padding: 0 9px 0 3px }`.
const CARD_LEAD: f32 = 3.0;
const CARD_TRAIL: f32 = 9.0;
/// `.diff-output__line--meta .diff-output__content { padding: 0 10px }`.
const CARD_NOTE_PAD: f32 = 10.0;

fn cell_width(c: char) -> u32 {
    match c as u32 {
        0x300..=0x36F | 0x200B..=0x200F | 0xFE00..=0xFE0F => 0,
        _ if is_cjk(c) || is_emoji(c) => WIDE,
        _ => NARROW,
    }
}

/// `text` with its tabs expanded and control characters blanked, which a font would draw as boxes.
fn cleaned(text: &str) -> String {
    let mut buf = String::with_capacity(text.len());
    let mut col = 0;
    let limit = if text.len() > MAX_LINE_BYTES {
        let mut end = MAX_LINE_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        end
    } else {
        text.len()
    };
    for c in text[..limit].chars() {
        match c {
            '\t' => {
                let n = TAB_STOP - (col / NARROW) % TAB_STOP;
                buf.extend(std::iter::repeat_n(' ', n as usize));
                col += n * NARROW;
            }
            '\r' => {}
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

/// One line's text with its colours and word marks, on the grid.
#[derive(Clone, Debug, Default)]
pub struct Cell {
    pub text: String,
    tokens: Vec<(u32, Token)>,
    marks: Vec<(u32, u32)>,
    units: u32,
    pub no_newline: bool,
}

impl Cell {
    fn new(raw: &str, tokens: Option<&[CodeToken]>, segments: Option<&[WordDiffSegment]>, no_newline: bool) -> Self {
        // Tabs and control characters change byte offsets, so colour and marks are carried across the cleaning by piece.
        let plain_tokens;
        let tokens = match tokens {
            Some(tokens) => tokens,
            None => {
                plain_tokens = vec![CodeToken { kind: Token::Plain, value: raw.to_owned() }];
                &plain_tokens[..]
            }
        };
        let segments_owned;
        let segments = match segments {
            Some(segments) => segments,
            None => {
                segments_owned = vec![WordDiffSegment { text: raw.to_owned(), changed: false }];
                &segments_owned[..]
            }
        };
        let mut text = String::with_capacity(raw.len());
        let mut out_tokens = Vec::new();
        let mut marks: Vec<(u32, u32)> = Vec::new();
        for run in mark_tokens(tokens, segments) {
            let start = text.len() as u32;
            for token in &run.tokens {
                if text.len() >= MAX_LINE_BYTES {
                    break;
                }
                out_tokens.push((text.len() as u32, token.kind));
                text.push_str(&cleaned(&token.value));
            }
            if run.changed && text.len() as u32 > start {
                marks.push((start, text.len() as u32));
            }
        }
        let units = text.chars().map(cell_width).sum();
        Self { text, tokens: out_tokens, marks, units, no_newline }
    }

    /// The cuts a row's text is painted in: runs sharing a colour, a mark and a grid width. Calls `f` with the byte range,
    /// which visual line it is on, the column it starts at, and its width in units.
    fn walk(&self, cols: u32, wrap: bool, mut f: impl FnMut(usize, usize, u32, u32, u32) -> bool) {
        let text = &self.text;
        let room = cols * NARROW;
        let (mut at, mut vis, mut col) = (0usize, 0u32, 0u32);
        let (mut ti, mut mi) = (0usize, 0usize);
        while at < text.len() {
            while self.tokens.get(ti + 1).is_some_and(|t| t.0 as usize <= at) {
                ti += 1;
            }
            while self.marks.get(mi).is_some_and(|m| m.1 as usize <= at) {
                mi += 1;
            }
            let tok_end = self.tokens.get(ti + 1).map_or(text.len(), |t| t.0 as usize);
            let mark_edge = self.marks.get(mi).map_or(usize::MAX, |m| if m.0 as usize <= at { m.1 as usize } else { m.0 as usize });
            let limit = tok_end.min(mark_edge).min(text.len());
            let (end, units) = run_at(text, at, limit, if wrap { room - col } else { u32::MAX });
            if end == at {
                vis += 1;
                col = 0;
                continue;
            }
            if !f(at, end, vis, col, units) {
                return;
            }
            at = end;
            col += units;
            if wrap && col >= room {
                vis += 1;
                col = 0;
            }
        }
    }

    /// What a reader hears of it, which ends in the same `↵` the eye gets.
    fn spoken(&self) -> String {
        if self.no_newline { format!("{}↵", self.text) } else { self.text.clone() }
    }

    fn visual_lines(&self, cols: u32) -> u32 {
        let room = cols * NARROW;
        if self.units <= room {
            return 1;
        }
        let mut lines = 1;
        self.walk(cols, true, |_, _, vis, _, _| {
            lines = lines.max(vis + 1);
            true
        });
        lines
    }

    fn kind_at(&self, at: usize) -> Token {
        let i = self.tokens.partition_point(|t| t.0 as usize <= at);
        if i == 0 { Token::Plain } else { self.tokens[i - 1].1 }
    }

    fn marked_at(&self, at: usize) -> bool {
        self.marks.iter().any(|m| (m.0 as usize) <= at && at < m.1 as usize)
    }

    /// The byte a point falls on, with `x` in grid units from the text's start and `line` the visual line.
    fn hit(&self, x: f32, line: u32, cols: u32, wrap: bool) -> usize {
        let mut found = self.text.len();
        let mut last_on_line = None;
        self.walk(cols, wrap, |start, end, vis, col, _| {
            if vis < line {
                return true;
            }
            if vis > line {
                found = last_on_line.unwrap_or(start);
                return false;
            }
            let mut c_col = col;
            for (i, c) in self.text[start..end].char_indices() {
                let w = cell_width(c);
                if x < (c_col + w / 2) as f32 {
                    found = start + i;
                    return false;
                }
                c_col += w;
            }
            last_on_line = Some(end);
            found = end;
            true
        });
        found
    }
}

/// The colour class at a byte of a cell, for a test that reads what a line was coloured as.
#[cfg(test)]
pub(crate) fn kind_at(cell: &Cell, at: usize) -> Token {
    cell.kind_at(at)
}

/// A cell for a line of a diff card.
pub fn cell_for_output(text: &str, tokens: Option<&[CodeToken]>, segments: Option<&[WordDiffSegment]>) -> Cell {
    Cell::new(text, tokens, segments, false)
}

/// The longest run from `at` that fits in `room` units and shares a grid width. Wide characters come one at a time, so
/// that a font whose CJK is not quite 5/3 of its Latin cannot run on into what follows.
fn run_at(text: &str, at: usize, limit: usize, room: u32) -> (usize, u32) {
    let Some(c) = text[at..limit].chars().next() else { return (at, 0) };
    if cell_width(c) == WIDE {
        return if room >= WIDE { (at + c.len_utf8(), WIDE) } else { (at, 0) };
    }
    let (mut end, mut units) = (at, 0u32);
    for c in text[at..limit].chars() {
        let w = cell_width(c);
        if w == WIDE || units.saturating_add(w) > room {
            break;
        }
        end += c.len_utf8();
        units += w;
    }
    (end, units)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Context,
    Addition,
    Deletion,
}

fn tone_of(kind: DiffLineKind) -> Tone {
    match kind {
        DiffLineKind::Context => Tone::Context,
        DiffLineKind::Addition => Tone::Addition,
        DiffLineKind::Deletion => Tone::Deletion,
    }
}

#[derive(Clone, Debug)]
pub enum TableRow {
    /// The break between hunks: how much was left out, an offer to fetch it, and the hunk's heading.
    Band { label: String, heading: String },
    /// A line of the unified layout, or of a card, with its one number.
    Line { tone: Tone, number: Option<u32>, cell: Cell },
    /// A row of the side-by-side layout. `number` is what the unified gutter shows; `tone` the band it continues.
    Pair { number: Option<u32>, tone: Tone, context: bool, left: Option<Cell>, right: Option<Cell> },
    /// Something said between lines, `… 12 diff lines omitted …`.
    Note(String),
}

/// A pair of lines opposite each other. Indices into the hunk's lines; a context line pairs with itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinePair {
    pub deletion: Option<usize>,
    pub addition: Option<usize>,
}

/// Pairs a hunk's changed lines so the two sides can sit opposite each other. A run of removals followed by a run of
/// additions is one replacement: the nth removal answers the nth addition, and whichever run is longer spills into rows
/// with an empty other side. Context lines pair with themselves.
pub fn pair_hunk_lines(lines: &[DiffLine]) -> Vec<LinePair> {
    let mut pairs = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].kind == DiffLineKind::Context {
            pairs.push(LinePair { deletion: Some(index), addition: Some(index) });
            index += 1;
            continue;
        }
        let (mut deletions, mut additions) = (Vec::new(), Vec::new());
        while index < lines.len() && lines[index].kind == DiffLineKind::Deletion {
            deletions.push(index);
            index += 1;
        }
        while index < lines.len() && lines[index].kind == DiffLineKind::Addition {
            additions.push(index);
            index += 1;
        }
        // A run of additions then removals is two runs; guard against never advancing on anything else.
        for row in 0..deletions.len().max(additions.len()) {
            pairs.push(LinePair { deletion: deletions.get(row).copied(), addition: additions.get(row).copied() });
        }
    }
    pairs
}

fn without_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Drops the changes that are only whitespace. A pair whose two sides are equal once whitespace is removed becomes
/// context: it is still the file, it is just not the change being read. A hunk with nothing left after that is dropped
/// entirely, the way `git diff -w` drops it.
pub fn suppress_whitespace_only(hunk: &DiffHunk) -> Option<DiffHunk> {
    let pairs = pair_hunk_lines(&hunk.lines);
    let mut kept = Vec::with_capacity(hunk.lines.len());
    let mut changed = false;
    for pair in pairs {
        match (pair.deletion, pair.addition) {
            (Some(d), Some(a)) if d != a => {
                let (deletion, addition) = (&hunk.lines[d], &hunk.lines[a]);
                if without_whitespace(&deletion.text) == without_whitespace(&addition.text) {
                    kept.push(DiffLine { kind: DiffLineKind::Context, old_line: deletion.old_line, ..addition.clone() });
                    continue;
                }
                changed = true;
                kept.push(deletion.clone());
                kept.push(addition.clone());
            }
            (Some(d), Some(_)) => kept.push(hunk.lines[d].clone()),
            (only_d, only_a) => {
                let Some(only) = only_d.or(only_a) else { continue };
                let only = &hunk.lines[only];
                // A lone added or removed line is a real change unless it is blank on its own.
                if without_whitespace(&only.text).is_empty() {
                    kept.push(DiffLine { kind: DiffLineKind::Context, ..only.clone() });
                    continue;
                }
                changed = true;
                kept.push(only.clone());
            }
        }
    }
    changed.then(|| DiffHunk { lines: kept, ..hunk.clone() })
}

/// The word segments of a hunk's paired lines. A lone add or remove has nothing to compare to.
fn word_segments_for(lines: &[DiffLine], pairs: &[LinePair], enabled: bool) -> Vec<Option<Vec<WordDiffSegment>>> {
    let mut segments = vec![None; lines.len()];
    if !enabled {
        return segments;
    }
    for pair in pairs {
        let (Some(d), Some(a)) = (pair.deletion, pair.addition) else { continue };
        if d == a {
            continue;
        }
        let result = word_diff(&lines[d].text, &lines[a].text);
        if result.bailed {
            continue;
        }
        segments[d] = Some(result.before);
        segments[a] = Some(result.after);
    }
    segments
}

fn side_of(line: &DiffLine) -> DiffSide {
    match line.kind {
        DiffLineKind::Addition => DiffSide::New,
        DiffLineKind::Deletion => DiffSide::Old,
        DiffLineKind::Context => DiffSide::Both,
    }
}

/// What is in a file's body, as rows.
#[derive(Clone, Debug, Default)]
pub struct CodeTable {
    pub rows: Vec<TableRow>,
    pub split: bool,
    /// Drawn as `.diff-output__line` rather than `.diff-viewer__line`: smaller and taller, a change said by its tint alone, and
    /// a rule beside the numbers.
    card: bool,
    max_no: u32,
    offsets: Vec<f32>,
    laid_out: Option<(u32, u32, bool)>,
    /// The width and the cell it was laid out at, for what is asked between frames.
    laid_at: (f32, f32),
    max_units: [u32; 2],
    can_scroll_x: bool,
}

/// Which side of the grid something is in. Unified has only the one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    One,
    Left,
    Right,
}

/// What a point in the table is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableHit {
    Band(usize),
    Text { row: usize, column: Column, byte: usize },
    Nothing,
}

impl CodeTable {
    /// One file's hunks. Between two hunks sits a separator saying how many unmodified lines were left out, derived from
    /// the hunk headers: the old file's line numbers are contiguous, so the gap between where one hunk ends and the next
    /// begins is exactly what is missing. Before the first hunk the gap is everything above it; after the last one the
    /// file's length is unknown from a patch alone, so the separator says only that there may be more.
    pub fn of_hunks(hunks: &[DiffHunk], split: bool, words: bool, lang: Option<highlight::Lang>, can_expand: bool) -> Self {
        let mut rows = Vec::new();
        let band = |unmodified: Option<u32>, heading: &str| {
            let label = match unmodified {
                None => t("下面可能还有未改动的内容", "More unchanged context may be available").to_owned(),
                Some(count) => tf("{count} 行未改动", "{count} unmodified lines", &[("count", &count)]),
            };
            TableRow::Band { label, heading: heading.to_owned() }
        };
        for (index, hunk) in hunks.iter().enumerate() {
            let gap = match index.checked_sub(1).map(|i| &hunks[i]) {
                None => i64::from(hunk.old_start) - 1,
                Some(previous) => i64::from(hunk.old_start) - i64::from(previous.old_start) - i64::from(previous.old_lines),
            };
            if gap > 0 {
                rows.push(band(Some(gap as u32), &hunk.heading));
            } else if !hunk.heading.is_empty() {
                rows.push(band(Some(0), &hunk.heading));
            }
            let pairs = pair_hunk_lines(&hunk.lines);
            let segments = word_segments_for(&hunk.lines, &pairs, words);
            let excerpt: Vec<(DiffSide, &str)> = hunk.lines.iter().map(|l| (side_of(l), l.text.as_str())).collect();
            let tokens = highlight_diff_lines(lang, &excerpt);
            let cell = |i: usize, marked: bool| Cell::new(&hunk.lines[i].text, tokens[i].as_deref(), if marked { segments[i].as_deref() } else { None }, hunk.lines[i].no_newline);
            if split {
                for pair in &pairs {
                    let context = pair.deletion.is_some() && pair.deletion == pair.addition;
                    let number = pair.addition.and_then(|i| hunk.lines[i].new_line).or_else(|| pair.deletion.and_then(|i| hunk.lines[i].old_line));
                    let tone = if context {
                        Tone::Context
                    } else if pair.deletion.is_some() {
                        Tone::Deletion
                    } else {
                        Tone::Addition
                    };
                    rows.push(TableRow::Pair { number, tone, context, left: pair.deletion.map(|i| cell(i, !context)), right: pair.addition.map(|i| Cell { no_newline: !context && hunk.lines[i].no_newline, ..cell(i, !context) }) });
                }
            } else {
                for (i, line) in hunk.lines.iter().enumerate() {
                    rows.push(TableRow::Line { tone: tone_of(line.kind), number: line.new_line.or(line.old_line), cell: cell(i, line.kind != DiffLineKind::Context) });
                }
            }
        }
        if can_expand && !hunks.is_empty() {
            rows.push(band(None, ""));
        }
        Self::of_rows(rows, split)
    }

    pub fn of_rows(rows: Vec<TableRow>, split: bool) -> Self {
        let mut max_no = 0;
        let mut max_units = [0, 0];
        for row in &rows {
            match row {
                TableRow::Line { number, cell, .. } => {
                    max_no = max_no.max(number.unwrap_or(0));
                    max_units[0] = max_units[0].max(cell.units);
                }
                TableRow::Pair { number, left, right, .. } => {
                    max_no = max_no.max(number.unwrap_or(0));
                    max_units[0] = max_units[0].max(left.as_ref().map_or(0, |c| c.units));
                    max_units[1] = max_units[1].max(right.as_ref().map_or(0, |c| c.units));
                }
                _ => {}
            }
        }
        Self { rows, split, card: false, max_no, offsets: Vec::new(), laid_out: None, laid_at: (0.0, 0.0), max_units, can_scroll_x: false }
    }

    /// The lines of a card: `DiffOutput`'s, and the read view that shares its look.
    pub fn of_card_rows(rows: Vec<TableRow>) -> Self {
        Self { card: true, ..Self::of_rows(rows, false) }
    }

    fn ty(&self) -> &'static TypeStyle {
        if self.card { &CARD_TY } else { &TY }
    }

    fn line_h(&self) -> f32 {
        if self.card { CARD_LINE_H } else { LINE_H }
    }

    /// Between the gutter and a line's text.
    fn lead(&self, cw: f32) -> f32 {
        if self.card { CARD_LEAD } else { TEXT_LEAD * cw }
    }

    fn trail(&self, cw: f32) -> f32 {
        if self.card { CARD_TRAIL } else { TEXT_TRAIL * cw }
    }

    /// Exactly as wide as the file's widest number.
    fn gutter(&self, cw: f32) -> f32 {
        let digits = if self.max_no == 0 { 1 } else { self.max_no.ilog10() + 1 };
        digits as f32 * cw + if self.card { CARD_RULE } else { 0.0 }
    }

    fn cols_for(&self, width: f32, cw: f32) -> u32 {
        let code = width - self.gutter(cw);
        let side = if self.split { code * 0.5 } else { code };
        (((side - self.lead(cw) - self.trail(cw)) / cw).floor() as i64).max(MIN_COLS as i64) as u32
    }

    /// Measures every row for this width. Cheap enough to do when the width changes, and skipped when it has not.
    pub fn layout(&mut self, width: f32, wrap: bool, cw: f32) -> f32 {
        let cols = self.cols_for(width, cw);
        let key = (cols, (width * 2.0) as u32, wrap);
        if self.laid_out != Some(key) {
            self.laid_out = Some(key);
            self.laid_at = (width, cw);
            let line_h = self.line_h();
            let note_h = if self.card { CARD_LINE_H } else { NOTE_H };
            let mut offsets = Vec::with_capacity(self.rows.len() + 1);
            let mut y = 0.0;
            for row in &self.rows {
                offsets.push(y);
                y += match row {
                    TableRow::Band { .. } => BAND_H,
                    TableRow::Note(_) => note_h,
                    TableRow::Line { cell, .. } if wrap => cell.visual_lines(cols) as f32 * line_h,
                    TableRow::Pair { left, right, .. } if wrap => left.as_ref().map_or(1, |c| c.visual_lines(cols)).max(right.as_ref().map_or(1, |c| c.visual_lines(cols))) as f32 * line_h,
                    _ => line_h,
                };
            }
            offsets.push(y);
            self.offsets = offsets;
            self.can_scroll_x = !wrap && self.max_scroll_x(width, cw) > 0.0;
        }
        self.offsets.last().copied().unwrap_or(0.0) + if self.card { 0.0 } else { BOTTOM_PAD } + if self.can_scroll_x { SCROLLBAR_H } else { 0.0 }
    }

    /// What a side's longest line asks for.
    fn needs(&self, side: usize, cw: f32) -> f32 {
        self.max_units[side] as f32 * (cw / NARROW as f32) + self.lead(cw) + self.trail(cw)
    }

    /// How far the code can move sideways.
    fn max_scroll_x(&self, width: f32, cw: f32) -> f32 {
        let lead = self.gutter(cw);
        if self.split {
            let (left, right) = self.side_widths(width, false, cw);
            (lead + left + right - width).max(0.0)
        } else {
            (lead + self.needs(0, cw) - width).max(0.0)
        }
    }

    fn row_at(&self, y: f32) -> Option<usize> {
        if self.offsets.len() != self.rows.len() + 1 || y < 0.0 {
            return None;
        }
        let i = self.offsets.partition_point(|o| *o <= y).checked_sub(1)?;
        (i < self.rows.len()).then_some(i)
    }

    /// What a point, relative to the table's corner, is on.
    pub fn hit(&self, x: f32, y: f32, width: f32, wrap: bool, cw: f32, scroll_x: f32) -> TableHit {
        let Some(row) = self.row_at(y) else { return TableHit::Nothing };
        let cols = self.cols_for(width, cw);
        let unit = cw / NARROW as f32;
        let line = ((y - self.offsets[row]) / self.line_h()).max(0.0) as u32;
        let lead = self.gutter(cw) - scroll_x;
        let text_lead = self.lead(cw);
        match &self.rows[row] {
            TableRow::Band { .. } => TableHit::Band(row),
            TableRow::Note(_) => TableHit::Nothing,
            TableRow::Line { cell, .. } => {
                let tx = (x - lead - text_lead) / unit;
                TableHit::Text { row, column: Column::One, byte: cell.hit(tx.max(0.0), line, cols, wrap) }
            }
            TableRow::Pair { left, right, .. } => {
                let (side, _) = self.side_widths(width, wrap, cw);
                let (column, cell, start) = if x < lead + side { (Column::Left, left, lead) } else { (Column::Right, right, lead + side) };
                match cell {
                    Some(cell) => TableHit::Text { row, column, byte: cell.hit(((x - start - text_lead) / unit).max(0.0), line, cols, wrap) },
                    None => TableHit::Nothing,
                }
            }
        }
    }

    /// Side by side, the two sides split what is left evenly. Wrapped they always do; otherwise a side whose lines run
    /// longer than its half takes their length, and the other what remains: upstream's `minmax(max-content, 1fr)` tracks.
    fn side_widths(&self, width: f32, wrap: bool, cw: f32) -> (f32, f32) {
        let code = (width - self.gutter(cw)).max(0.0);
        let half = code * 0.5;
        if wrap {
            return (half, half);
        }
        let (left, right) = (self.needs(0, cw), self.needs(1, cw));
        if left + right >= code {
            (left, right)
        } else if left > half {
            (left, code - left)
        } else if right > half {
            (code - right, right)
        } else {
            (half, half)
        }
    }

    /// The line number a row shows, for the right-click menu: `data-line`.
    pub fn line_of(&self, row: usize) -> Option<u32> {
        match self.rows.get(row)? {
            TableRow::Line { number, .. } | TableRow::Pair { number, .. } => *number,
            _ => None,
        }
    }

    fn cell_of(&self, row: usize, column: Column) -> Option<&Cell> {
        match (self.rows.get(row)?, column) {
            (TableRow::Line { cell, .. }, _) => Some(cell),
            (TableRow::Pair { left, .. }, Column::Left) => left.as_ref(),
            (TableRow::Pair { right, .. }, Column::Right) => right.as_ref(),
            _ => None,
        }
    }

    /// A double click: the word around a byte, or the run of spaces or punctuation it is in.
    pub fn word_at(&self, row: usize, column: Column, byte: usize) -> Option<(usize, usize)> {
        let text = &self.cell_of(row, column)?.text;
        let at = byte.min(text.len());
        let class = |c: char| if c.is_alphanumeric() || c == '_' { 0 } else if c.is_whitespace() { 1 } else { 2 };
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let index = chars.iter().position(|(i, c)| at < i + c.len_utf8()).or(chars.len().checked_sub(1))?;
        let kind = class(chars[index].1);
        let start = chars[..index].iter().rposition(|(_, c)| class(*c) != kind).map_or(0, |i| i + 1);
        let end = chars[index..].iter().position(|(_, c)| class(*c) != kind).map_or(chars.len(), |i| index + i);
        Some((chars[start].0, chars.get(end).map_or(text.len(), |(i, _)| *i)))
    }

    /// Select All: every line of the side a selection is on, and nothing past this file.
    pub fn select_all(&self, column: Column) -> Option<Selection> {
        let has = |row: &usize| self.cell_of(*row, column).is_some();
        let first = (0..self.rows.len()).find(has)?;
        let last = (0..self.rows.len()).rev().find(has)?;
        Some(Selection { column, anchor: (first, 0), head: (last, self.cell_of(last, column)?.text.len()) })
    }

    /// The text between two points of one column: side by side, each side is selected on its own, and copies as that
    /// side's lines.
    pub fn selected_text(&self, selection: &Selection) -> String {
        let (start, end) = selection.ordered();
        let mut out = String::new();
        for row in start.0..=end.0 {
            let Some(cell) = self.cell_of(row, selection.column) else { continue };
            let from = if row == start.0 { start.1.min(cell.text.len()) } else { 0 };
            let to = if row == end.0 { end.1.min(cell.text.len()) } else { cell.text.len() };
            if !out.is_empty() || row > start.0 {
                out.push('\n');
            }
            out.push_str(&cell.text[from..to.max(from)]);
        }
        out
    }

    /// Paints the rows that intersect the clip.
    #[allow(clippy::too_many_arguments)]
    pub fn paint(&self, ui: &mut Ui<'_>, r: Rect, wrap: bool, cw: f32, scroll_x: f32, selection: Option<&Selection>, can_expand: bool, expanding: bool, hovered_band: Option<usize>) {
        let n = self.rows.len();
        if self.offsets.len() != n + 1 || n == 0 {
            return;
        }
        let t = ui.theme;
        let clip = ui.clip;
        // Whole physical pixels, so that a fractional scroll position cannot open hairlines between row bands.
        let top = (r.y * ui.scale).round() / ui.scale;
        let first = self.offsets[..n].partition_point(|o| *o <= clip.y - top).saturating_sub(1);
        let ground = if t.code.adaptive { None } else { Some(t.code.background) };
        if let Some(g) = ground {
            ui.fill(r, 0.0, g);
        }
        let colors: [Color; 12] = [Token::Plain, Token::Keyword, Token::String, Token::Number, Token::Comment, Token::Function, Token::Type, Token::Meta, Token::Tag, Token::Attribute, Token::Property, Token::Variable].map(|k| highlight::color(k, &t));
        let ty = self.ty();
        let pen = Pen { colors, ty, line_h: self.line_h(), unit: cw / NARROW as f32, cw, cols: self.cols_for(r.w, cw), wrap, mark_added: t.code.added.with_alpha(0.28), mark_removed: t.code.removed.with_alpha(0.28) };
        let gutter = self.gutter(cw);
        let number_x = r.x;
        let code_x = r.x + gutter - scroll_x;
        let (left_w, right_w) = self.side_widths(r.w, wrap, cw);
        let number_ground = ground.unwrap_or(if t.dark { Color::rgb(0x202020) } else { Color::rgb(0xf3f3f3) });
        let added_band = t.code.added.with_alpha(0.13);
        let removed_band = t.code.removed.with_alpha(0.13);

        for i in first..n {
            let y = top + self.offsets[i];
            if y >= clip.bottom() {
                break;
            }
            let h = self.offsets[i + 1] - self.offsets[i];
            match &self.rows[i] {
                TableRow::Band { label, heading } => self.paint_band(ui, rect(r.x, y, r.w, h), label, heading, can_expand, expanding, hovered_band == Some(i)),
                TableRow::Note(note) if self.card => {
                    ui.fill(rect(r.x, y, r.w, h), 0.0, t.caution.with_alpha(0.08));
                    ui.label_ellipsis(r.x + CARD_NOTE_PAD, y, (r.w - CARD_NOTE_PAD * 2.0).max(0.0), note, &CARD_NOTE, t.text_tertiary);
                }
                TableRow::Note(note) => {
                    ui.label_ellipsis(r.x + 8.0, y + (h - FINE.line) * 0.5, (r.w - 16.0).max(0.0), note, &FINE, t.text_tertiary);
                }
                TableRow::Line { tone, number, cell } => {
                    let (band, marker, number_color) = match tone {
                        Tone::Addition => (Some(added_band), Some(("+", t.code.added)), t.code.added),
                        Tone::Deletion => (Some(removed_band), Some(("-", t.code.removed)), t.code.removed),
                        Tone::Context => (None, None, t.code.line_number),
                    };
                    if let Some(band) = band {
                        ui.fill(rect(r.x, y, r.w, h), 0.0, band);
                    }
                    let text_x = code_x + self.lead(cw);
                    // A card's change is said by its tint and its number's hue.
                    let marker = marker.filter(|_| !self.card);
                    ui.with_clip(rect(r.x + gutter, y, (r.w - gutter).max(0.0), h).intersect(&clip), |ui| {
                        if let Some(sel) = selection.filter(|s| s.column == Column::One && s.covers(i)) {
                            paint_selection(ui, cell, sel, i, text_x, y, &pen);
                        }
                        if let Some((text, color)) = marker {
                            ui.label(code_x, y, text, ty, color);
                        }
                        paint_cell(ui, cell, text_x, y, &pen, *tone);
                    });
                    self.paint_number(ui, number_x, y, h, gutter, cw, *number, number_color, scroll_x > 0.0, number_ground, band);
                }
                TableRow::Pair { number, tone, context, left, right } => {
                    let number_color = match tone {
                        Tone::Addition => t.code.added,
                        Tone::Deletion => t.code.removed,
                        Tone::Context => t.code.line_number,
                    };
                    let left_x = code_x;
                    let right_x = code_x + left_w;
                    let row_clip = rect(r.x + gutter, y, (r.w - gutter).max(0.0), h).intersect(&clip);
                    ui.with_clip(row_clip, |ui| {
                        for (column, x, w, cell, tone) in [(Column::Left, left_x, left_w, left, if *context { Tone::Context } else { Tone::Deletion }), (Column::Right, right_x, right_w, right, if *context { Tone::Context } else { Tone::Addition })] {
                            let area = rect(x, y, w, h);
                            match cell {
                                None => paint_absent(ui, area),
                                Some(cell) => {
                                    match tone {
                                        Tone::Addition => {
                                            ui.fill(area, 0.0, added_band);
                                            ui.label(x, y, "+", ty, t.code.added);
                                        }
                                        Tone::Deletion => {
                                            ui.fill(area, 0.0, removed_band);
                                            ui.label(x, y, "-", ty, t.code.removed);
                                        }
                                        Tone::Context => {}
                                    }
                                    let text_x = x + self.lead(cw);
                                    if let Some(sel) = selection.filter(|s| s.column == column && s.covers(i)) {
                                        paint_selection(ui, cell, sel, i, text_x, y, &pen);
                                    }
                                    paint_cell(ui, cell, text_x, y, &pen, tone);
                                }
                            }
                        }
                        // With no number column between them, a rule is what tells the two sides apart.
                        ui.fill(rect(right_x, y, 1.0, h), 0.0, t.divider);
                    });
                    let band = match tone {
                        Tone::Addition => Some(added_band),
                        Tone::Deletion => Some(removed_band),
                        Tone::Context => None,
                    };
                    self.paint_number(ui, number_x, y, h, gutter, cw, *number, number_color, scroll_x > 0.0, number_ground, band);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_number(&self, ui: &mut Ui<'_>, x: f32, y: f32, h: f32, gutter: f32, cw: f32, number: Option<u32>, color: Color, scrolled: bool, ground: Color, band: Option<Color>) {
        // The number column stays put while the code moves under it, and only then needs a ground of its own.
        if scrolled {
            ui.fill(rect(x, y, gutter, h), 0.0, ground);
            if let Some(band) = band {
                ui.fill(rect(x, y, gutter, h), 0.0, band);
            }
        }
        let rule = if self.card { CARD_RULE } else { 0.0 };
        if self.card {
            let divider = ui.theme.divider;
            ui.fill(rect(x + gutter - rule, y, rule, h), 0.0, divider);
        }
        if let Some(number) = number {
            let digits = number.to_string();
            let at = x + gutter - rule - digits.len() as f32 * cw;
            ui.label(at, y, &digits, self.ty(), color);
        }
    }

    /// Where a band's two controls end, from the band's left edge: the expand button and the label beside it. The heading
    /// past them only reads.
    fn band_controls_width(&self, ui: &mut Ui<'_>, row: usize, can_expand: bool) -> f32 {
        let TableRow::Band { label, .. } = &self.rows[row] else { return 0.0 };
        BAND_PAD + if can_expand { BAND_BUTTON } else { 0.0 } + ui.measure(label, &FINE)
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_band(&self, ui: &mut Ui<'_>, b: Rect, label: &str, heading: &str, can_expand: bool, expanding: bool, hovered: bool) {
        let t = ui.theme;
        ui.fill(b, 0.0, t.subtle_hover);
        let ty = FINE;
        let mut x = b.x + BAND_PAD;
        let cy = b.y + b.h * 0.5;
        if can_expand {
            ui.named_icon("UnfoldVertical", x + 8.0, cy, 12.0, if expanding { t.text_disabled } else if hovered { t.text_primary } else { t.text_tertiary }, 0.0);
            x += BAND_BUTTON;
        }
        let color = if expanding {
            t.text_disabled
        } else if hovered && can_expand {
            t.text_primary
        } else {
            t.text_secondary
        };
        let label_y = b.y + (b.h - ty.line) * 0.5;
        let label_w = ui.label(x, label_y, label, &ty, color);
        if hovered && can_expand && !expanding {
            ui.fill(rect(x, label_y + ty.line - 1.0, label_w, 1.0), 0.0, color);
        }
        x += label_w + 4.0;
        if !heading.is_empty() {
            ui.label_ellipsis(x, b.y + (b.h - MONO_SMALL.line) * 0.5, (b.right() - x - 6.0).max(0.0), heading, &MONO_SMALL, t.text_tertiary);
        }
    }
}

struct Pen {
    colors: [Color; 12],
    ty: &'static TypeStyle,
    line_h: f32,
    unit: f32,
    cw: f32,
    cols: u32,
    wrap: bool,
    mark_added: Color,
    mark_removed: Color,
}

fn paint_cell(ui: &mut Ui<'_>, cell: &Cell, x: f32, y: f32, pen: &Pen, tone: Tone) {
    let clip = ui.clip;
    let mark = if tone == Tone::Addition { pen.mark_added } else { pen.mark_removed };
    let mut end_at = (0u32, 0u32);
    cell.walk(pen.cols, pen.wrap, |start, end, vis, col, units| {
        end_at = (vis, col + units);
        let (px, py) = (x + col as f32 * pen.unit, y + vis as f32 * pen.line_h);
        if py >= clip.bottom() {
            return false;
        }
        if py + pen.line_h <= clip.y {
            return true;
        }
        if !pen.wrap && px >= clip.right() {
            return false;
        }
        let piece = &cell.text[start..end];
        if cell.marked_at(start) {
            ui.fill(rect(px, py, units as f32 * pen.unit, pen.line_h), 2.0, mark);
        }
        if !piece.bytes().all(|b| b == b' ') {
            ui.label(px, py, piece, pen.ty, pen.colors[cell.kind_at(start) as usize]);
        }
        true
    });
    if cell.no_newline {
        let t = ui.theme;
        ui.label(x + end_at.1 as f32 * pen.unit + 4.0, y + end_at.0 as f32 * pen.line_h, "↵", pen.ty, t.text_tertiary);
    }
}

/// A side with no counterpart is not an empty line, it is the absence of one, and the hatch says so.
fn paint_absent(ui: &mut Ui<'_>, area: Rect) {
    let t = ui.theme;
    let c = t.text_primary.with_alpha(0.05);
    let mut y = area.y;
    while y < area.bottom() {
        ui.fill(rect(area.x, y, area.w, 2.0_f32.min(area.bottom() - y)), 0.0, c);
        y += 4.0;
    }
}

fn paint_selection(ui: &mut Ui<'_>, cell: &Cell, selection: &Selection, row: usize, x: f32, y: f32, pen: &Pen) {
    let (start, end) = selection.ordered();
    let from = if row == start.0 { start.1 } else { 0 };
    let to = if row == end.0 { end.1 } else { cell.text.len() };
    let sel = ui.theme.selection;
    if cell.text.is_empty() && row != end.0 {
        ui.fill(rect(x, y, pen.cw, pen.line_h), 0.0, sel);
        return;
    }
    cell.walk(pen.cols, pen.wrap, |a, b, vis, col, _| {
        if b <= from || a >= to {
            return true;
        }
        let (mut c_col, mut left, mut right) = (col, None, 0u32);
        for (i, c) in cell.text[a..b].char_indices() {
            let w = cell_width(c);
            let at = a + i;
            if at >= from && at < to {
                left.get_or_insert(c_col);
                right = c_col + w;
            }
            c_col += w;
        }
        if let Some(left) = left {
            ui.fill(rect(x + left as f32 * pen.unit, y + vis as f32 * pen.line_h, (right - left) as f32 * pen.unit, pen.line_h), 0.0, sel);
        }
        true
    });
}

/// A drag across lines of one column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub column: Column,
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Selection {
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }

    fn covers(&self, row: usize) -> bool {
        let (start, end) = self.ordered();
        row >= start.0 && row <= end.0
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// What a table keeps between frames besides its rows.
#[derive(Default)]
pub struct TableUi {
    pub scroll_x: f32,
    selecting: bool,
    pub selection: Option<Selection>,
    /// How many presses in a row the last one was: a word on the second, the line on the third.
    clicks: u8,
    pressed_band: Option<usize>,
    dragging_bar: Option<(f32, f32)>,
}

pub type LineMenu = Rc<dyn Fn(&mut App, u32, (f32, f32))>;

/// The leaf: measures to the table's height, paints the rows in view, and takes the pointer. `state` holds the rows
/// across frames, for as long as a view keeps asking for them; `key` names its other state.
pub fn table_leaf(key: u64, table: Local<CodeTable>, wrap: bool, can_expand: bool, expanding: bool, on_expand: Option<Handler<App>>, on_line_menu: Option<LineMenu>) -> E {
    let ui_state = use_state(("code-table-ui", key), TableUi::default);
    let measure_table = table.clone();
    let spoken = table.clone();
    let on_expand_key = on_expand.clone();
    let leaf = custom::<App>(move |app, ui, r| {
        let cw = cell_of_font(ui, table.borrow().ty());
        let mut guard = ui_state.borrow_mut();
        let st = &mut *guard;
        let tbl = table.borrow();
        let max_x = tbl.max_scroll_x(r.w, cw);
        if wrap || max_x <= 0.0 {
            st.scroll_x = 0.0;
        }
        st.scroll_x = st.scroll_x.min(max_x);
        let local = ui.input.pointer.map(|p| (p.0 - r.x, p.1 - r.y));
        let hovering = ui.pointer_in(&r);
        let hit = if hovering { local.map(|(x, y)| tbl.hit(x, y, r.w, wrap, cw, st.scroll_x)) } else { None };
        let bar_h = if tbl.can_scroll_x { SCROLLBAR_H } else { 0.0 };
        let bar = rect(r.x, r.bottom() - bar_h, r.w, bar_h);
        let on_bar = bar_h > 0.0 && hovering && ui.pointer_in(&bar);

        // Only the expand button and the label beside it answer: the heading past them only reads.
        let hovered_band = match (hit, local) {
            (Some(TableHit::Band(row)), Some((x, _))) if x <= tbl.band_controls_width(ui, row, can_expand) => Some(row),
            _ => None,
        };
        if hovered_band.is_some() && can_expand && !expanding {
            ui.cursor = CursorKind::Hand;
        } else if matches!(hit, Some(TableHit::Text { .. })) && !on_bar {
            ui.cursor = CursorKind::Text;
        }

        // The press.
        if ui.input.pressed {
            if hovering {
                if on_bar {
                    if let Some((x, _)) = local {
                        st.dragging_bar = Some((x, st.scroll_x));
                    }
                } else {
                    match hit {
                        // The expand button is an element of its own.
                        Some(TableHit::Band(_)) if can_expand && local.is_some_and(|(x, _)| x < BAND_PAD + BAND_BUTTON) => {}
                        Some(TableHit::Band(row)) => st.pressed_band = Some(row),
                        Some(TableHit::Text { row, column, byte }) => {
                            st.clicks = if ui.input.double_click { st.clicks.saturating_add(1) } else { 1 };
                            let extended = ui.input.shift.then_some(st.selection).flatten().filter(|sel| sel.column == column);
                            st.selection = Some(match (extended, st.clicks) {
                                (Some(sel), _) => Selection { head: (row, byte), ..sel },
                                (None, 1) => Selection { column, anchor: (row, byte), head: (row, byte) },
                                (None, 2) => {
                                    let (start, end) = tbl.word_at(row, column, byte).unwrap_or((byte, byte));
                                    Selection { column, anchor: (row, start), head: (row, end) }
                                }
                                (None, _) => Selection { column, anchor: (row, 0), head: (row, tbl.cell_of(row, column).map_or(0, |cell| cell.text.len())) },
                            });
                            // A word or a line is already what was asked for, and a drag from it would throw that away.
                            st.selecting = extended.is_some() || st.clicks == 1;
                        }
                        _ => st.selection = None,
                    }
                }
            } else {
                // A press anywhere else drops what was selected here.
                st.selection = None;
            }
        }
        if st.selecting && ui.input.down
            && let (Some((x, y)), Some(sel)) = (local, st.selection.as_mut())
            && let TableHit::Text { row, column, byte } = tbl.hit(x, y.clamp(0.0, (r.h - 1.0).max(0.0)), r.w, wrap, cw, st.scroll_x)
            && column == sel.column
        {
            sel.head = (row, byte);
        }
        if let (Some((start_x, start_scroll)), Some((x, _))) = (st.dragging_bar, local)
            && ui.input.down
            && max_x > 0.0
        {
            let track = r.w.max(1.0);
            st.scroll_x = (start_scroll + (x - start_x) * (max_x + track) / track).clamp(0.0, max_x);
        }
        let mut clicked_band = false;
        if ui.input.released {
            st.selecting = false;
            st.dragging_bar = None;
            if let Some(row) = st.pressed_band.take()
                && hovered_band == Some(row)
                && can_expand
                && !expanding
            {
                clicked_band = true;
            }
        }
        // The wheel's sideways half: the box scrolls under a trackpad as an `overflow-x: auto` grid does.
        if hovering && ui.input.scroll_x != 0.0 && !wrap && max_x > 0.0 {
            st.scroll_x = (st.scroll_x - ui.input.scroll_x).clamp(0.0, max_x);
        }
        // Select All stays inside the file the selection is in; with none here it is someone else's.
        let select_all = ui.input.keys.iter().any(|k| (k.cmd || k.ctrl) && !k.alt && !k.shift && matches!(k.key, Key::Char('a' | 'A')));
        if select_all && let Some(all) = st.selection.and_then(|sel| tbl.select_all(sel.column)) {
            st.selection = Some(all);
        }
        let cmd_c = ui.input.keys.iter().any(|k| k.cmd && !k.alt && !k.shift && matches!(k.key, Key::Char('c' | 'C')));
        if cmd_c
            && let Some(sel) = st.selection.filter(|s| !s.is_empty())
        {
            let copied = tbl.selected_text(&sel);
            ui.clipboard.set(&copied);
        }

        tbl.paint(ui, r, wrap, cw, st.scroll_x, st.selection.as_ref(), can_expand, expanding, hovered_band);

        // The way sideways for a mouse with one wheel.
        if bar_h > 0.0 && max_x > 0.0 {
            let t = ui.theme;
            let thumb_w = (r.w * r.w / (r.w + max_x)).max(24.0);
            let thumb_x = r.x + (r.w - thumb_w) * (st.scroll_x / max_x);
            ui.fill(rect(thumb_x, bar.y + 1.0, thumb_w, SCROLLBAR_H - 2.0), 3.0, t.control_strong_stroke);
        }

        let right = ui.input.right_pressed && hovering;
        // Anywhere on a row answers with its number, the side with nothing on it included: `data-line` is on the row.
        let menu_line = if right { local.and_then(|(_, y)| tbl.row_at(y)).and_then(|row| tbl.line_of(row)) } else { None };
        let point = ui.input.pointer.unwrap_or((0.0, 0.0));
        drop(tbl);
        drop(guard);
        if clicked_band
            && let Some(f) = &on_expand
        {
            f(app);
        }
        if let (Some(line), Some(f)) = (menu_line, &on_line_menu) {
            f(app, line, point);
        }
    })
    .key(("code-table-leaf", key))
    .w_full()
    .measure(move |_, ui, known, avail| {
        let w = known.unwrap_or(if avail.is_finite() { avail } else { 600.0 });
        let ty = measure_table.borrow().ty();
        let cw = cell_of_font(ui, ty);
        let h = measure_table.borrow_mut().layout(w, wrap, cw);
        (w, h)
    });
    // What only a pointer finds in the paint: the expand button, which a keyboard reaches too upstream, and for a screen reader the
    // lines. With neither there is nothing to add.
    let keyboard = can_expand && on_expand_key.is_some();
    if !keyboard && !a11y_active() {
        return leaf;
    }
    let beside = spoken.with(|tbl| tbl.beside_rows(key, keyboard, expanding, on_expand_key.as_ref()));
    div().w_full().child(leaf).children(beside)
}

fn cell_of_font(ui: &mut Ui<'_>, ty: &TypeStyle) -> f32 {
    let measured = ui.measure("0", ty);
    if measured > 0.0 { measured } else { ty.size * 0.6 }
}

impl CodeTable {
    /// What goes over the rows near the view, none of which paints. The lines are for the accessibility tree: upstream's are text
    /// in the DOM with their numbers `aria-hidden`, which is all a reader gets of them, and a line's being added or removed is only
    /// its colour there too. A window's worth either side is plenty, since what scrolls out steps out of the tree, and it keeps a
    /// file of 100,000 lines from costing a node a line a frame.
    fn beside_rows(&self, key: u64, can_expand: bool, expanding: bool, on_expand: Option<&Handler<App>>) -> Vec<E> {
        let n = self.rows.len();
        if self.offsets.len() != n + 1 {
            // Nothing is laid out before the first measure, so the rows wait for the next frame.
            repaint();
            return Vec::new();
        }
        let speak = a11y_active();
        let top = rect_of(("code-table-leaf", key)).map_or(0.0, |r| r.y);
        let window = window_size().1;
        let first = self.offsets[..n].partition_point(|o| top + *o < -window).saturating_sub(1);
        let last = self.offsets[..n].partition_point(|o| top + *o < window * 2.0);
        let (width, cw) = self.laid_at;
        let gutter = self.gutter(cw);
        let (left_w, right_w) = self.side_widths(width, self.laid_out.is_some_and(|(_, _, wrap)| wrap), cw);
        let mut out = Vec::new();
        for i in first..last {
            let (y, h) = (self.offsets[i], self.offsets[i + 1] - self.offsets[i]);
            let spot = |e: E, x: f32, w: f32| e.absolute().top(y).left(x).w(w.max(0.0)).h(h);
            let said = |id: (&'static str, u64, usize), words: String, x: f32, w: f32| spot(div().key(id).pass_through().aria_label(words), x, w);
            match &self.rows[i] {
                TableRow::Line { cell, .. } if speak && !cell.text.trim().is_empty() => out.push(said(("code-table-line", key, i), cell.spoken(), gutter, width - gutter)),
                TableRow::Pair { left, right, .. } if speak => {
                    if let Some(cell) = left.as_ref().filter(|cell| !cell.text.trim().is_empty()) {
                        out.push(said(("code-table-old", key, i), cell.spoken(), gutter, left_w));
                    }
                    if let Some(cell) = right.as_ref().filter(|cell| !cell.text.trim().is_empty()) {
                        out.push(said(("code-table-new", key, i), cell.spoken(), gutter + left_w, right_w));
                    }
                }
                TableRow::Note(note) if speak => out.push(said(("code-table-note", key, i), note.clone(), 0.0, width)),
                TableRow::Band { label, heading } => {
                    let expand_label = t("展开更多上下文", "Expand more context");
                    // The one real button of the band: the leaf leaves a press here to it, and it has a tooltip to show.
                    if let Some(on_expand) = on_expand.filter(|_| can_expand) {
                        let press = on_expand.clone();
                        let button = div().key(("code-table-expand", key, i)).tooltip(expand_label).cursor(CursorKind::Hand).disabled(expanding).when(!expanding, move |e| e.on_click(move |app: &mut App| press(app)));
                        out.push(spot(button, BAND_PAD, BAND_BUTTON));
                    }
                    if speak {
                        let x = BAND_PAD + if can_expand { BAND_BUTTON } else { 0.0 };
                        match on_expand.filter(|_| can_expand) {
                            Some(on_expand) => {
                                let press = on_expand.clone();
                                out.push(spot(div().key(("code-table-band", key, i)).pass_through().role(Role::Button).aria_label(label.clone()).disabled(expanding).on_click(move |app: &mut App| press(app)), x, width - x));
                            }
                            None => out.push(said(("code-table-band", key, i), label.clone(), x, width - x)),
                        }
                        if !heading.is_empty() {
                            out.push(said(("code-table-heading", key, i), heading.clone(), x, width - x));
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unified_diff::parse_unified_diff;

    fn line(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine { kind, old_line: Some(1), new_line: Some(1), text: text.into(), no_newline: false }
    }

    #[test]
    fn pairs_a_replacement_and_spills_the_longer_run() {
        use DiffLineKind::*;
        let lines = [line(Context, "a"), line(Deletion, "b"), line(Deletion, "c"), line(Addition, "d"), line(Context, "e"), line(Addition, "f")];
        assert_eq!(pair_hunk_lines(&lines), [
            LinePair { deletion: Some(0), addition: Some(0) },
            LinePair { deletion: Some(1), addition: Some(3) },
            LinePair { deletion: Some(2), addition: None },
            LinePair { deletion: Some(4), addition: Some(4) },
            LinePair { deletion: None, addition: Some(5) },
        ]);
    }

    fn hunk_of(patch: &str) -> DiffHunk {
        parse_unified_diff(patch, None).remove(0).hunks.remove(0)
    }

    #[test]
    fn whitespace_only_hunks_are_dropped_and_real_changes_stay() {
        let only_ws = hunk_of("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +1,2 @@\n-  foo\n+\tfoo\n bar\n");
        assert_eq!(suppress_whitespace_only(&only_ws), None);
        let real = hunk_of("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,3 +1,3 @@\n-  foo\n+\tfoo\n-bar\n+baz\n ctx\n");
        let kept = suppress_whitespace_only(&real).unwrap();
        assert_eq!(kept.lines.iter().map(|l| (l.kind, l.text.as_str())).collect::<Vec<_>>(), [(DiffLineKind::Context, "\tfoo"), (DiffLineKind::Deletion, "bar"), (DiffLineKind::Addition, "baz"), (DiffLineKind::Context, "ctx")]);
        // A blank line added on its own is not a change either.
        let blank = hunk_of("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1,2 @@\n ctx\n+  \n");
        assert_eq!(suppress_whitespace_only(&blank), None);
    }

    fn table(patch: &str, split: bool, can_expand: bool) -> CodeTable {
        let file = parse_unified_diff(patch, None).remove(0);
        CodeTable::of_hunks(&file.hunks, split, true, highlight::lang_of("x.ts"), can_expand)
    }

    const TWO_HUNKS: &str = "diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -5,2 +5,2 @@ function a() {\n-const old = 1;\n+const next = 1;\n keep\n@@ -20,1 +20,2 @@\n ctx\n+more\n";

    #[test]
    fn hunk_separators_count_what_was_left_out_and_offer_more_at_the_end() {
        let t = table(TWO_HUNKS, false, true);
        let bands: Vec<&str> = t.rows.iter().filter_map(|r| if let TableRow::Band { label, .. } = r { Some(label.as_str()) } else { None }).collect();
        assert_eq!(bands, ["4 行未改动", "13 行未改动", "下面可能还有未改动的内容"]);
        let t = table(TWO_HUNKS, false, false);
        assert_eq!(t.rows.iter().filter(|r| matches!(r, TableRow::Band { .. })).count(), 2, "no tail when there is nothing more to ask for");
    }

    #[test]
    fn a_heading_with_no_gap_still_gets_its_band() {
        let t = table("diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -1,1 +1,1 @@ fn head\n-a\n+b\n", false, false);
        assert!(matches!(&t.rows[0], TableRow::Band { label, heading } if label == "0 行未改动" && heading == "fn head"));
    }

    #[test]
    fn unified_rows_number_new_then_old_and_mark_changed_words() {
        let t = table(TWO_HUNKS, false, false);
        let lines: Vec<(Tone, Option<u32>)> = t.rows.iter().filter_map(|r| if let TableRow::Line { tone, number, .. } = r { Some((*tone, *number)) } else { None }).collect();
        assert_eq!(&lines[..3], [(Tone::Deletion, Some(5)), (Tone::Addition, Some(5)), (Tone::Context, Some(6))]);
        let TableRow::Line { cell, .. } = &t.rows[2] else { panic!() };
        assert_eq!(cell.text, "const next = 1;");
        assert_eq!(cell.marks.iter().map(|m| &cell.text[m.0 as usize..m.1 as usize]).collect::<Vec<_>>(), ["next"]);
    }

    #[test]
    fn split_rows_tone_the_number_by_the_side_that_changed_and_leave_the_absent_side_empty() {
        let t = table("diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -1,3 +1,3 @@\n-gone\n ctx\n+new\n-also gone\n", true, false);
        let pairs: Vec<(Tone, bool, bool)> = t.rows.iter().filter_map(|r| if let TableRow::Pair { tone, left, right, .. } = r { Some((*tone, left.is_some(), right.is_some())) } else { None }).collect();
        assert_eq!(pairs, [(Tone::Deletion, true, false), (Tone::Context, true, true), (Tone::Addition, false, true), (Tone::Deletion, true, false)]);
    }

    #[test]
    fn colours_survive_tab_expansion_and_a_grammarless_file_is_plain() {
        let file = parse_unified_diff("diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -0,0 +1 @@\n+\tconst a = 1;\n", None).remove(0);
        let t = CodeTable::of_hunks(&file.hunks, false, false, highlight::lang_of("x.ts"), false);
        let TableRow::Line { cell, .. } = &t.rows[0] else { panic!() };
        assert_eq!(cell.text, "  const a = 1;");
        assert_eq!(cell.kind_at(2), Token::Keyword);
        let t = CodeTable::of_hunks(&file.hunks, false, false, None, false);
        let TableRow::Line { cell, .. } = &t.rows[0] else { panic!() };
        assert_eq!(cell.kind_at(2), Token::Plain);
    }

    #[test]
    fn a_table_wraps_on_the_grid_and_lays_out_only_when_the_width_changes() {
        let long = "x".repeat(200);
        let file = parse_unified_diff(&format!("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -0,0 +1 @@\n+{long}\n"), None).remove(0);
        let mut t = CodeTable::of_hunks(&file.hunks, false, false, None, false);
        let wrapped = t.layout(400.0, true, 7.2);
        let cols = t.cols_for(400.0, 7.2);
        let expected = (200u32.div_ceil(cols)) as f32 * LINE_H;
        assert_eq!(wrapped, expected + BOTTOM_PAD);
        let flat = t.layout(400.0, false, 7.2);
        assert_eq!(flat, LINE_H + BOTTOM_PAD + SCROLLBAR_H);
        assert!(t.max_scroll_x(400.0, 7.2) > 0.0);
    }

    #[test]
    fn side_by_side_splits_evenly_until_a_side_needs_more() {
        let patch = |left: &str, right: &str| format!("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-{left}\n+{right}\n");
        let table = |left: &str, right: &str| {
            let file = parse_unified_diff(&patch(left, right), None).remove(0);
            CodeTable::of_hunks(&file.hunks, true, false, None, false)
        };
        let cw = 7.2;
        let width = 800.0;
        let code = width - table("a", "b").gutter(cw);
        assert_eq!(table("a", "b").side_widths(width, false, cw), (code / 2.0, code / 2.0));
        // The long side takes its length, and the short one what remains, with nothing to scroll.
        let long = "x".repeat(70);
        let t = table("a", &long);
        let (left, right) = t.side_widths(width, false, cw);
        assert!((right - t.needs(1, cw)).abs() < 0.01);
        assert!((left + right - code).abs() < 0.01);
        assert_eq!(t.max_scroll_x(width, cw), 0.0);
        // Past the room for both, each takes its own and the grid scrolls as one.
        let t = table(&"y".repeat(200), &"z".repeat(10));
        let (left, right) = t.side_widths(width, false, cw);
        assert!((left - t.needs(0, cw)).abs() < 0.01 && (right - t.needs(1, cw)).abs() < 0.01);
        assert!(t.max_scroll_x(width, cw) > 0.0);
        // Wrapped, the two sides are always exactly half each.
        assert_eq!(t.side_widths(width, true, cw), (code / 2.0, code / 2.0));
    }

    #[test]
    fn hits_find_the_line_the_column_and_the_byte() {
        let mut t = table(TWO_HUNKS, false, false);
        t.layout(600.0, false, 7.2);
        // Past the band, on the deletion, at the fourth character of the text.
        let x = t.gutter(7.2) + TEXT_LEAD * 7.2 + 3.5 * 7.2;
        match t.hit(x, BAND_H + 2.0, 600.0, false, 7.2, 0.0) {
            TableHit::Text { row, column, byte } => {
                assert_eq!((row, column, byte), (1, Column::One, 4));
                assert_eq!(t.line_of(row), Some(5));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.hit(10.0, 2.0, 600.0, false, 7.2, 0.0), TableHit::Band(0));
    }

    #[test]
    fn a_double_click_takes_the_word_and_select_all_takes_the_side() {
        let mut t = table("diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -1,2 +1,2 @@\n-const one_two = 1;\n-two\n+ONE\n+TWO\n", true, false);
        t.layout(800.0, false, 7.2);
        assert_eq!(t.word_at(0, Column::Left, 8), Some((6, 13)), "`_` is part of a word");
        assert_eq!(t.word_at(0, Column::Left, 5), Some((5, 6)), "the space it fell on");
        let all = t.select_all(Column::Right).unwrap();
        assert_eq!(t.selected_text(&all), "ONE\nTWO");
        assert_eq!(t.select_all(Column::Left).map(|s| t.selected_text(&s)).as_deref(), Some("const one_two = 1;\ntwo"));
    }

    #[test]
    fn a_selection_copies_as_its_own_sides_lines() {
        let mut t = table("diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -1,2 +1,2 @@\n-one\n-two\n+ONE\n+TWO\n", true, false);
        t.layout(800.0, false, 7.2);
        let sel = Selection { column: Column::Left, anchor: (0, 1), head: (1, 2) };
        assert_eq!(t.selected_text(&sel), "ne\ntw");
        let sel = Selection { column: Column::Right, anchor: (1, 3), head: (0, 0) };
        assert_eq!(t.selected_text(&sel), "ONE\nTWO");
    }
}
