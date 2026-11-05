//! The read-only emulator behind a shell command's page: what upstream's xterm was there with `disableStdin` and
//! `convertEol`. These are pipes, not a pty: nothing turns a program's LF into CRLF on the way here.
//!
//! TODO(mewrk-term): the emulator there only comes with a PTY and its row painter is private, so this feeds
//! `alacritty_terminal` directly and paints runs itself. A pty-less `Terminal::viewer()` there could replace it.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll as GridScroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{viewport_to_point, Config, Osc52, MIN_COLUMNS, MIN_SCREEN_LINES};
use alacritty_terminal::vte::ansi::{Color as Ansi, NamedColor, Processor};
use alacritty_terminal::Term;
use fluent::theme::MONO_BODY;
use fluent::{rect, Color, CursorKind, Key, Rect, Scroll, Theme, TypeStyle, Ui, Weight};

const FONT: TypeStyle = TypeStyle { size: 12.0, line: 14.4, ..MONO_BODY };
const HISTORY: usize = 20_000;
const PAD_X: f32 = 8.0;
const PAD_Y: f32 = 4.0;
const GUTTER: f32 = 12.0;
const MAX_COLS: usize = 500;
const MAX_LINES: usize = 300;
const SCROLLBAR_SALT: u64 = 0x7E57_0002;
const DIM: f32 = 0.66;

/// stderr is dimmed rather than coloured red: a warning on stderr is not a failure.
const STDERR_PREFIX: &[u8] = b"\x1b[2m";
const STDERR_SUFFIX: &[u8] = b"\x1b[0m";

struct GridSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The same base colours as `mewrk-term`, so a build log does not change colour depending on who started it.
const DARK: [u32; 16] = [0x3C3C3C, 0xF1707B, 0x6CCB5F, 0xE3C341, 0x6CB6FF, 0xD68FE8, 0x4FC9D9, 0xCFCFCF, 0x8B8B8B, 0xFF99A4, 0x9BE58A, 0xFCE100, 0x99CCFF, 0xEFB5FF, 0x8BE8F0, 0xFFFFFF];
const LIGHT: [u32; 16] = [0x1F1F1F, 0xC42B1C, 0x0F7B0F, 0x9D5D00, 0x005FB8, 0x9A1F9A, 0x007C8A, 0x5C5C5C, 0x6E6E6E, 0xD13438, 0x107C10, 0x986F0B, 0x0078D4, 0xB82FB0, 0x038387, 0x3A3A3A];
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn resolve(color: Ansi, theme: &Theme, background: bool) -> Option<Color> {
    let table = if theme.dark { &DARK } else { &LIGHT };
    match color {
        Ansi::Spec(c) => Some(Color { r: c.r, g: c.g, b: c.b, a: 255 }),
        Ansi::Indexed(i) => Some(match i {
            0..=15 => Color::rgb(table[i as usize]),
            16..=231 => {
                let n = i as usize - 16;
                Color { r: CUBE[n / 36], g: CUBE[(n / 6) % 6], b: CUBE[n % 6], a: 255 }
            }
            _ => {
                let v = 8 + (i - 232) * 10;
                Color { r: v, g: v, b: v, a: 255 }
            }
        }),
        Ansi::Named(NamedColor::Background) => None,
        Ansi::Named(NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground | NamedColor::Cursor) => Some(theme.text_primary),
        Ansi::Named(n) => {
            let i = n as usize;
            if i < 16 { Some(Color::rgb(table[i])) } else if background { None } else { Some(theme.text_primary) }
        }
    }
}

struct RowRun {
    col: usize,
    text: String,
    color: Color,
    bold: bool,
    italic: bool,
    /// A wide or combining character whose advance need not be the grid's, drawn alone.
    cluster: Option<usize>,
}

struct RowFill {
    col: usize,
    len: usize,
    color: Color,
}

pub struct AnsiView {
    term: Term<VoidListener>,
    parser: Processor,
    cols: usize,
    lines: usize,
    scroll: Scroll,
    seen_top: usize,
    dragging: bool,
    drag_at: (Point, Side),
    /// A selection, read at the time of the copy.
    selection: Option<SelectionRange>,
}

impl Default for AnsiView {
    fn default() -> Self {
        Self::new()
    }
}

impl AnsiView {
    pub fn new() -> Self {
        let (cols, lines) = (80, 24);
        let config = Config { scrolling_history: HISTORY, osc52: Osc52::Disabled, ..Config::default() };
        Self {
            term: Term::new(config, &GridSize { cols, lines }, VoidListener),
            parser: Processor::new(),
            cols,
            lines,
            scroll: Scroll::default(),
            seen_top: 0,
            dragging: false,
            drag_at: (Point::new(Line(0), Column(0)), Side::Left),
            selection: None,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.parser.advance(&mut self.term, b);
        }
    }

    /// Output of one pipe. A bare LF is "down one row" to an emulator, so without the CR every line would start where
    /// the previous one ended, the staircase upstream's `convertEol` is there to prevent.
    pub fn write(&mut self, text: &str, stderr: bool) {
        if text.is_empty() {
            return;
        }
        if stderr {
            self.feed(STDERR_PREFIX);
        }
        let mut last = 0u8;
        for &b in text.as_bytes() {
            if b == b'\n' && last != b'\r' {
                self.parser.advance(&mut self.term, b'\r');
            }
            self.parser.advance(&mut self.term, b);
            last = b;
        }
        if stderr {
            self.feed(STDERR_SUFFIX);
        }
    }

    pub fn has_selection(&self) -> bool {
        self.term.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    pub fn select_all(&mut self) {
        let (top, bottom) = (self.term.grid().topmost_line(), self.term.grid().bottommost_line());
        let last = Column(self.term.grid().columns() - 1);
        let mut selection = Selection::new(SelectionType::Simple, Point::new(top, Column(0)), Side::Left);
        selection.update(Point::new(bottom, last), Side::Right);
        self.term.selection = Some(selection);
    }

    /// The lines on screen, as text, for tests and for reading what is shown.
    pub fn visible_lines(&self) -> Vec<String> {
        let grid = self.term.grid();
        (0..grid.screen_lines() as i32)
            .map(|i| {
                let line = &grid[Line(i - grid.display_offset() as i32)];
                let mut s = String::new();
                for c in 0..grid.columns() {
                    let cell = &line[Column(c)];
                    if !cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                        s.push(if cell.c == '\0' { ' ' } else { cell.c });
                    }
                }
                s.trim_end().to_owned()
            })
            .collect()
    }

    /// Whether the cell at this row and column of the screen came from a pipe the page dims.
    pub fn is_dim(&self, row: usize, col: usize) -> bool {
        let grid = self.term.grid();
        grid[Line(row as i32 - grid.display_offset() as i32)][Column(col)].flags.contains(Flags::DIM)
    }

    pub fn resize(&mut self, cols: usize, lines: usize) {
        if (cols, lines) != (self.cols, self.lines) {
            self.term.resize(GridSize { cols, lines });
            self.cols = cols;
            self.lines = lines;
        }
    }

    fn hit(&self, x0: f32, y0: f32, cw: f32, ch: f32, p: (f32, f32)) -> (Point, Side) {
        let fx = ((p.0 - x0) / cw).max(0.0);
        let col = (fx as usize).min(self.cols - 1);
        let side = if fx < col as f32 + 0.5 { Side::Left } else { Side::Right };
        let line = (((p.1 - y0) / ch).max(0.0) as usize).min(self.lines - 1);
        (viewport_to_point(self.term.grid().display_offset(), Point::new(line, Column(col))), side)
    }

    /// Draws into `r`. Nothing typed reaches anything: there is no process on the other end. Cmd+C copies a selection
    /// and Cmd+A selects everything, which is all a read-only page has keys for.
    pub fn show(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let measured = ui.measure("0", &FONT);
        let (cw, ch) = (if measured > 0.0 { measured } else { FONT.size * 0.6 }, FONT.line);
        let (x0, y0) = (r.x + PAD_X, r.y + PAD_Y);
        let (w, h) = (r.w - PAD_X - GUTTER, r.h - PAD_Y * 2.0);
        // xterm cannot measure a box that has no usable size yet, and neither can this.
        if w < cw * MIN_COLUMNS as f32 || h < ch {
            return;
        }
        self.resize(((w / cw) as usize).clamp(MIN_COLUMNS, MAX_COLS), ((h / ch) as usize).clamp(MIN_SCREEN_LINES, MAX_LINES));

        let input = ui.input;
        let hovered = ui.pointer_in(&r);
        if input.pressed {
            if hovered {
                ui.mem.focus = id;
            } else if ui.mem.focus == id {
                ui.mem.focus = 0;
            }
        }
        if let Some(p) = input.pointer {
            let over_grid = hovered && p.0 < r.right() - GUTTER;
            if over_grid {
                ui.cursor = CursorKind::Text;
            }
            if input.pressed && over_grid {
                let (point, side) = self.hit(x0, y0, cw, ch, p);
                let ty = if input.double_click { SelectionType::Semantic } else { SelectionType::Simple };
                self.term.selection = Some(Selection::new(ty, point, side));
                self.dragging = true;
                self.drag_at = (point, side);
            } else if self.dragging && input.down {
                let at = self.hit(x0, y0, cw, ch, p);
                if at != self.drag_at {
                    self.drag_at = at;
                    if let Some(selection) = &mut self.term.selection {
                        selection.update(at.0, at.1);
                    }
                }
            }
        }
        if input.released {
            self.dragging = false;
        }
        if ui.mem.focus == id {
            for press in &input.keys {
                if press.cmd && !press.alt && !press.shift {
                    match press.key {
                        Key::Char(c) if c.eq_ignore_ascii_case(&'c') => {
                            if let Some(text) = self.selection_text() {
                                ui.clipboard.set(&text);
                            }
                        }
                        Key::Char(c) if c.eq_ignore_ascii_case(&'a') => self.select_all(),
                        _ => {}
                    }
                }
            }
        }

        // `Scroll` owns the wheel and the scrollbar in pixels from the top of the history; the emulator owns the line
        // at the top of the view. This keeps the two agreeing, as the terminal pane does.
        let history = self.term.grid().history_size();
        let offset = self.term.grid().display_offset();
        let top = history - offset;
        if !self.scroll.stick && top != self.seen_top {
            self.scroll.shift((top as f32 - self.seen_top as f32) * ch);
        }
        self.seen_top = top;
        let area = rect(r.x, y0, r.w, self.lines as f32 * ch);
        let content = (history + self.lines) as f32 * ch;
        if self.scroll.stick {
            self.scroll.content = content;
            self.scroll.viewport = area.h;
            self.scroll.offset = self.scroll.max_offset();
        }
        self.scroll.update(ui, id ^ SCROLLBAR_SALT, area, content);
        let wanted = if self.scroll.stick { 0 } else { ((self.scroll.max_offset() - self.scroll.offset) / ch).round() as usize };
        if wanted != offset {
            self.term.scroll_display(GridScroll::Delta(wanted as i32 - offset as i32));
            self.seen_top = history - wanted;
        }
        self.selection = self.term.renderable_content().selection;
        ui.with_clip(r, |ui| self.paint(ui, x0, y0, cw, ch));
    }

    fn paint(&self, ui: &mut Ui<'_>, x0: f32, y0: f32, cw: f32, ch: f32) {
        let theme = ui.theme;
        let grid = self.term.grid();
        let offset = grid.display_offset() as i32;
        for i in 0..self.lines {
            let y = y0 + i as f32 * ch;
            let line = Line(i as i32 - offset);
            let row = &grid[line];
            let mut fills: Vec<RowFill> = Vec::new();
            let mut runs: Vec<RowRun> = Vec::new();
            for col in 0..self.cols {
                let cell = &row[Column(col)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                let inverse = cell.flags.contains(Flags::INVERSE);
                let (fg_src, bg_src) = if inverse { (cell.bg, cell.fg) } else { (cell.fg, cell.bg) };
                let mut fg = resolve(fg_src, &theme, false).unwrap_or(if inverse { theme.mica_base } else { theme.text_primary });
                if cell.flags.contains(Flags::DIM) {
                    fg = fg.fade(DIM);
                }
                if let Some(bg) = resolve(bg_src, &theme, true).or(inverse.then_some(theme.text_primary)) {
                    match fills.last_mut() {
                        Some(f) if f.color == bg && f.col + f.len == col => f.len += 1,
                        _ => fills.push(RowFill { col, len: 1, color: bg }),
                    }
                }
                if cell.flags.contains(Flags::HIDDEN) || matches!(cell.c, ' ' | '\t' | '\0') && cell.zerowidth().is_none() {
                    continue;
                }
                let (bold, italic) = (cell.flags.contains(Flags::BOLD), cell.flags.contains(Flags::ITALIC));
                let wide = cell.flags.contains(Flags::WIDE_CHAR);
                if cell.c.is_ascii() && cell.zerowidth().is_none() {
                    match runs.last_mut() {
                        Some(run) if run.cluster.is_none() && run.color == fg && run.bold == bold && run.italic == italic && run.col + run.text.len() <= col && col - (run.col + run.text.len()) <= 1 => {
                            if col > run.col + run.text.len() {
                                run.text.push(' ');
                            }
                            run.text.push(cell.c);
                        }
                        _ => runs.push(RowRun { col, text: cell.c.to_string(), color: fg, bold, italic, cluster: None }),
                    }
                } else {
                    let mut text = cell.c.to_string();
                    text.extend(cell.zerowidth().into_iter().flatten());
                    runs.push(RowRun { col, text, color: fg, bold, italic, cluster: Some(if wide { 2 } else { 1 }) });
                }
            }
            for f in &fills {
                ui.fill(rect(x0 + f.col as f32 * cw, y, f.len as f32 * cw, ch), 0.0, f.color);
            }
            if let Some(sel) = self.selection
                && line >= sel.start.line
                && line <= sel.end.line
            {
                let from = if sel.is_block || line == sel.start.line { sel.start.column.0 } else { 0 };
                let to = if sel.is_block || line == sel.end.line { sel.end.column.0 } else { self.cols - 1 };
                ui.fill(rect(x0 + from as f32 * cw, y, (to + 1 - from) as f32 * cw, ch), 0.0, theme.selection);
            }
            for run in &runs {
                let face = TypeStyle { weight: if run.bold { Weight::Semibold } else { Weight::Regular }, italic: run.italic, ..FONT };
                let at = x0 + run.col as f32 * cw;
                match run.cluster {
                    // Centred in its cells, so a fallback face's advance cannot move the columns after it.
                    Some(cells) => {
                        let tw = ui.measure(&run.text, &face);
                        ui.label(at + ((cells as f32 * cw - tw) * 0.5).max(0.0), y, &run.text, &face, run.color);
                    }
                    None => {
                        ui.label(at, y, &run.text, &face, run.color);
                    }
                }
            }
            for col in 0..self.cols {
                let cell = &row[Column(col)];
                if cell.flags.contains(Flags::HIDDEN) {
                    continue;
                }
                let fg = resolve(cell.fg, &theme, false).unwrap_or(theme.text_primary);
                let cx = x0 + col as f32 * cw;
                if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                    ui.fill(rect(cx, y + ch - 2.0, cw, 1.0), 0.0, fg);
                }
                if cell.flags.contains(Flags::STRIKEOUT) {
                    ui.fill(rect(cx, y + (ch * 0.5).round(), cw, 1.0), 0.0, fg);
                }
            }
        }
    }
}
