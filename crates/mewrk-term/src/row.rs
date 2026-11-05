//! One grid row as draw lists: background spans, runs of same-style text, and underlines. A row is rebuilt only
//! when its cells changed, so a steady screen costs a few cache lookups per frame.

use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use fluent::{rect, Color, TypeStyle, Ui, Weight};

use crate::boxes;
use crate::palette::{Palette, Style};
use crate::FONT;

const STYLE_FLAGS: Flags = Flags::ALL_UNDERLINES.union(Flags::INVERSE).union(Flags::BOLD).union(Flags::ITALIC).union(Flags::DIM).union(Flags::HIDDEN).union(Flags::STRIKEOUT);

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Fill {
    pub col: u16,
    pub len: u16,
    pub color: Color,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MarkKind {
    Underline,
    Double,
    Strike,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mark {
    pub col: u16,
    pub len: u16,
    pub kind: MarkKind,
    pub color: Color,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Run {
    pub col: u16,
    pub cells: u16,
    start: u32,
    end: u32,
    pub color: Color,
    pub bold: bool,
    pub italic: bool,
    /// A single character that may come from a fallback face, whose advance is not the grid's.
    pub cluster: bool,
}

impl Run {
    fn covers(&self, col: usize) -> bool {
        (self.col as usize..(self.col + self.cells) as usize).contains(&col)
    }

    fn face(&self) -> TypeStyle {
        TypeStyle { weight: if self.bold { Weight::Semibold } else { Weight::Regular }, italic: self.italic, ..FONT }
    }
}

/// A line-drawing or block character, which `boxes` draws instead of the font.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Drawn {
    pub col: u16,
    pub c: char,
    pub color: Color,
}

#[derive(Default)]
pub struct RowPaint {
    pub cells: Vec<Cell>,
    pub stale: bool,
    pub fills: Vec<Fill>,
    pub runs: Vec<Run>,
    pub marks: Vec<Mark>,
    pub drawn: Vec<Drawn>,
    text: String,
}

fn push_fill(fills: &mut Vec<Fill>, col: u16, color: Color) {
    match fills.last_mut() {
        Some(f) if f.color == color && f.col + f.len == col => f.len += 1,
        _ => fills.push(Fill { col, len: 1, color }),
    }
}

fn push_mark(marks: &mut Vec<Mark>, col: u16, kind: MarkKind, color: Color) {
    // An underline and a strikeout can alternate in the list, so look back past one.
    match marks.iter_mut().rev().take(2).find(|m| m.kind == kind && m.color == color && m.col + m.len == col) {
        Some(m) => m.len += 1,
        None => marks.push(Mark { col, len: 1, kind, color }),
    }
}

impl RowPaint {
    pub fn rebuild(&mut self, palette: &Palette, over: &Colors) {
        self.stale = false;
        self.fills.clear();
        self.runs.clear();
        self.marks.clear();
        self.drawn.clear();
        self.text.clear();
        let mut seen = None;
        let mut style = Style::default();
        // Blanks since the last character. Runs part at two or more: a column layout then shares cached runs
        // between rows instead of caching each spacing.
        let mut blanks = 0u16;
        for (i, cell) in self.cells.iter().enumerate() {
            let col = i as u16;
            let key = (cell.fg, cell.bg, cell.flags & STYLE_FLAGS);
            if seen != Some(key) {
                style = palette.style(cell, over);
                seen = Some(key);
            }
            if let Some(color) = style.bg {
                push_fill(&mut self.fills, col, color);
            }
            if !style.hidden {
                let color = cell.underline_color().map_or(style.fg, |c| palette.foreground(c, Flags::empty(), over));
                if cell.flags.contains(Flags::DOUBLE_UNDERLINE) {
                    push_mark(&mut self.marks, col, MarkKind::Double, color);
                } else if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                    push_mark(&mut self.marks, col, MarkKind::Underline, color);
                }
                if cell.flags.contains(Flags::STRIKEOUT) {
                    push_mark(&mut self.marks, col, MarkKind::Strike, style.fg);
                }
            }
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let zerowidth = cell.zerowidth().filter(|z| !z.is_empty());
            if style.hidden || (zerowidth.is_none() && matches!(cell.c, ' ' | '\t' | '\0')) {
                blanks += 1;
                continue;
            }
            if zerowidth.is_none() && boxes::is_drawn(cell.c) {
                self.drawn.push(Drawn { col, c: cell.c, color: style.fg });
                blanks = 0;
                continue;
            }
            let start = self.text.len() as u32;
            let run = Run { col, cells: 1, start, end: start, color: style.fg, bold: style.bold, italic: style.italic, cluster: false };
            if cell.c.is_ascii() && zerowidth.is_none() {
                // ASCII always comes from the primary face, one advance per cell, so it can share a run.
                let open = self.runs.last_mut().filter(|r| !r.cluster && blanks <= 1 && r.col + r.cells + blanks == col && (r.color, r.bold, r.italic) == (run.color, run.bold, run.italic));
                match open {
                    Some(r) => {
                        for _ in 0..blanks {
                            self.text.push(' ');
                        }
                        self.text.push(cell.c);
                        r.cells += blanks + 1;
                        r.end = self.text.len() as u32;
                    }
                    None => {
                        self.text.push(cell.c);
                        self.runs.push(Run { end: start + 1, ..run });
                    }
                }
            } else {
                self.text.push(cell.c);
                self.text.extend(zerowidth.into_iter().flatten());
                let cells = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
                self.runs.push(Run { cells, end: self.text.len() as u32, cluster: true, ..run });
            }
            blanks = 0;
        }
    }

    pub fn run_text(&self, run: &Run) -> &str {
        &self.text[run.start as usize..run.end as usize]
    }

    pub fn paint_fills(&self, ui: &mut Ui<'_>, x: f32, y: f32, cw: f32, ch: f32) {
        for f in &self.fills {
            ui.fill(rect(x + f.col as f32 * cw, y, f.len as f32 * cw, ch), 0.0, f.color);
        }
        for d in &self.drawn {
            boxes::paint(ui, d.c, x + d.col as f32 * cw, y, cw, ch, d.color);
        }
        for m in &self.marks {
            let (mx, w) = (x + m.col as f32 * cw, m.len as f32 * cw);
            match m.kind {
                MarkKind::Underline => ui.fill(rect(mx, y + ch - 2.0, w, 1.0), 0.0, m.color),
                MarkKind::Strike => ui.fill(rect(mx, y + (ch * 0.5).round(), w, 1.0), 0.0, m.color),
                MarkKind::Double => {
                    ui.fill(rect(mx, y + ch - 3.0, w, 1.0), 0.0, m.color);
                    ui.fill(rect(mx, y + ch - 1.0, w, 1.0), 0.0, m.color);
                }
            }
        }
    }

    /// Draws the row's text.
    pub fn paint_text(&self, ui: &mut Ui<'_>, x: f32, y: f32, cw: f32) {
        for run in &self.runs {
            let (face, text) = (run.face(), self.run_text(run));
            let at = x + run.col as f32 * cw;
            if run.cluster {
                paint_cluster(ui, at, y, run.cells as f32 * cw, text, &face, run.color);
            } else {
                ui.label(at, y, text, &face, run.color);
            }
        }
    }

    /// The character under a block cursor, in the cursor's text colour.
    pub fn paint_at(&self, ui: &mut Ui<'_>, x: f32, y: f32, cw: f32, col: usize, color: Color) {
        let Some(run) = self.runs.iter().find(|r| r.covers(col)) else { return };
        let (face, text) = (run.face(), self.run_text(run));
        let at = x + run.col as f32 * cw;
        if run.cluster {
            paint_cluster(ui, at, y, run.cells as f32 * cw, text, &face, color);
        } else {
            let k = col - run.col as usize;
            ui.label(at + k as f32 * cw, y, &text[k..k + 1], &face, color);
        }
    }
}

/// Centres a character in its cells, so that a fallback face's advance cannot move the columns after it.
fn paint_cluster(ui: &mut Ui<'_>, x: f32, y: f32, width: f32, text: &str, face: &TypeStyle, color: Color) {
    let w = ui.text.line(text, face, color).width / ui.scale;
    ui.label(x + ((width - w) * 0.5).max(0.0), y, text, face, color);
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Line;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;
    use fluent::Theme;

    use super::*;
    use crate::GridSize;

    /// The first row of a small terminal fed `bytes`.
    fn row_of(bytes: &[u8]) -> RowPaint {
        let mut term = Term::new(Config::default(), &GridSize { cols: 40, lines: 4 }, VoidListener);
        let mut parser: Processor = Processor::new();
        for b in bytes {
            parser.advance(&mut term, *b);
        }
        assert_eq!(term.columns(), 40);
        let mut row = RowPaint { cells: term.grid()[Line(0)][..].to_vec(), ..RowPaint::default() };
        row.rebuild(&Palette::from_theme(&Theme::dark()), &Colors::default());
        row
    }

    fn texts(row: &RowPaint) -> Vec<&str> {
        row.runs.iter().map(|r| row.run_text(r)).collect()
    }

    #[test]
    fn same_style_merges_and_style_changes_split() {
        let row = row_of(b"one two\x1b[31mred\x1b[0m\x1b[1mbold\x1b[0m\x1b[3mit\x1b[0m");
        assert_eq!(texts(&row), ["one two", "red", "bold", "it"]);
        assert_eq!(row.runs.iter().map(|r| r.col).collect::<Vec<_>>(), [0, 7, 10, 14]);
        assert!(row.runs[2].bold && row.runs[3].italic);
        assert_eq!(row.runs[1].color, Color::rgb(0xF1707B));
        assert!(row.fills.is_empty(), "the default background paints nothing");
    }

    #[test]
    fn wide_gaps_part_runs_so_columns_stay_cacheable() {
        let row = row_of(b"a b   c");
        assert_eq!(texts(&row), ["a b", "c"]);
        assert_eq!(row.runs[1].col, 6);
    }

    #[test]
    fn colour_depths_and_merged_backgrounds() {
        let row = row_of(b"\x1b[48;5;196mxy\x1b[48;2;1;2;3mz w\x1b[0m");
        assert_eq!(row.fills, [Fill { col: 0, len: 2, color: Color::rgb(0xFF0000) }, Fill { col: 2, len: 3, color: Color::rgb(0x010203) }]);
        let row = row_of(b"\x1b[38;2;250;100;0mt\x1b[38;5;46mu");
        assert_eq!(row.runs[0].color, Color::rgb(0xFA6400));
        assert_eq!(row.runs[1].color, Color::rgb(0x00FF00));
    }

    #[test]
    fn inverse_paints_the_blank_too() {
        let row = row_of(b"\x1b[7ma b\x1b[0m");
        assert_eq!(row.fills.len(), 1);
        assert_eq!((row.fills[0].col, row.fills[0].len), (0, 3));
        assert_eq!(texts(&row), ["a b"]);
    }

    #[test]
    fn wide_characters_take_two_cells_each() {
        let row = row_of("a世界b".as_bytes());
        assert_eq!(texts(&row), ["a", "世", "界", "b"]);
        let at: Vec<_> = row.runs.iter().map(|r| (r.col, r.cells, r.cluster)).collect();
        assert_eq!(at, [(0, 1, false), (1, 2, true), (3, 2, true), (5, 1, false)]);
    }

    #[test]
    fn combining_marks_stay_with_their_base() {
        let row = row_of("e\u{301}x".as_bytes());
        assert_eq!(texts(&row), ["e\u{301}", "x"]);
        assert!(row.runs[0].cluster);
    }

    #[test]
    fn underline_strikeout_and_hidden() {
        let row = row_of(b"\x1b[4mab\x1b[0m\x1b[9mcd\x1b[0m\x1b[8msecret\x1b[0m");
        assert_eq!(texts(&row), ["abcd"], "marks are drawn apart, so the text can share a run");
        let kinds: Vec<_> = row.marks.iter().map(|m| (m.col, m.len, m.kind)).collect();
        assert_eq!(kinds, [(0, 2, MarkKind::Underline), (2, 2, MarkKind::Strike)]);
        let row = row_of(b"\x1b[4:2mab");
        assert_eq!(row.marks[0].kind, MarkKind::Double);
    }

    #[test]
    fn line_drawing_leaves_the_text_runs() {
        let row = row_of("a│b".as_bytes());
        assert_eq!(texts(&row), ["a", "b"]);
        assert_eq!(row.drawn.len(), 1);
        assert_eq!((row.drawn[0].col, row.drawn[0].c), (1, '│'));
        assert_eq!(row.runs[1].col, 2);
    }

    #[test]
    fn dim_fades_the_text() {
        let row = row_of(b"\x1b[2mdim");
        assert!(row.runs[0].color.a < 255);
    }
}
