//! A terminal pane: `alacritty_terminal` for the emulator, drawn with Fluent. The far end is someone else's.
//!
//! Bytes come in through `feed`, from wherever the shell lives, and what the terminal has to say comes out through a
//! `Sink`: what the user typed, the emulator's answers to programs, and its new size. Upstream's bridge is the same
//! shape, with xterm.js where this has `Term`. A thread parses what `feed` queues into a `Term` behind a mutex. `show`
//! copies the visible cells out under that lock, only when something changed, and shapes and draws outside it. An idle
//! terminal has a thread parked on its queue, no frames, and no timers once its cursor stops blinking.

mod boxes;
mod keys;
mod palette;
mod row;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll as GridScroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::{Colors, COUNT};
use alacritty_terminal::term::{viewport_to_point, Config, Osc52, RenderableCursor, TermMode, MIN_COLUMNS, MIN_SCREEN_LINES};
use alacritty_terminal::vte::ansi::{self, Color as Ansi, CursorShape, CursorStyle, Handler, NamedColor};
use alacritty_terminal::Term;
use fluent::gpu::LAYER_POPUP;
use fluent::theme::MONO_BODY;
use fluent::{rect, Color, CursorKind, Key, Rect, Scroll, TypeStyle, Ui};
use parking_lot::Mutex;

use keys::{cursor_moves, encode_key, encode_paste, mouse_report, typed_text, wheel_arrows, MouseAction};
use palette::Palette;
use row::RowPaint;

const FONT: TypeStyle = MONO_BODY;
/// xterm's `scrollback` in upstream's interactive panel.
pub const HISTORY: usize = 10_000;
const PAD_X: f32 = 8.0;
const PAD_Y: f32 = 4.0;
/// Room for the scrollbar, kept whether or not it shows, so that history appearing does not rewrap the screen.
const GUTTER: f32 = 12.0;
const MAX_COLS: usize = 500;
const MAX_LINES: usize = 300;
const BLINK: f32 = 0.53;
/// A cursor nobody is using stops asking for frames.
const BLINK_FOR: f32 = 15.0;
/// Reflowing the whole history on every frame of a divider drag would stall it, and flood the shell with SIGWINCH.
const RESIZE_EVERY: f32 = 0.05;
const AUTOSCROLL_EVERY: f32 = 0.05;
/// xterm's default `wordSeparator`: what a double click stops at. A path or a URL is one word.
const WORD_SEPARATORS: &str = " ()[]{}',\"`";
/// `ALT_CLICK_MOVE_CURSOR_TIME`: a press held longer than this was going somewhere else.
const ALT_CLICK_SECONDS: f32 = 0.5;
const SCROLLBAR_SALT: u64 = 0x7E57_0001;
/// How much the parser takes under one lock of the `Term`, which is what a frame's copy of the screen may wait for.
const MAX_LOCKED_PARSE: usize = u16::MAX as usize;

/// A size for the emulator, which only wants the numbers.
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

/// What the terminal has to say to whatever is on the other end of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// The user typed or pasted this. Never produced while input is off.
    Input(String),
    /// The terminal's own traffic: answers to a program's queries, focus and mouse reports, wheel arrows. These pass
    /// while input is off, because a shell can wait on them during startup and no user is speaking.
    Reply(String),
    /// The grid is this big now.
    Resize { cols: u16, lines: u16 },
}

/// Called with what the terminal has to say, on the UI thread for what the user does and on the parser's thread for
/// answers to programs, so it must not touch the `Term` of the terminal calling it.
pub type Sink = Arc<dyn Fn(Output) + Send + Sync>;

/// What differs between an interactive terminal and a read-only page.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Lines of scrollback.
    pub history: usize,
    /// Whether a focused cursor blinks.
    pub blink: bool,
    /// Treat each LF as CRLF, which is xterm's `convertEol`: pipes have no line discipline to do it.
    pub convert_eol: bool,
    /// Cmd+A selects everything, for a page with nothing on the other end to send it to.
    pub select_all_chord: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { history: HISTORY, blink: true, convert_eol: false, select_all_chord: false }
    }
}

/// Whether a key is the copy chord of upstream's `isCopyChord`: Ctrl/Cmd+C with no other modifier. A panel declines it
/// over a selection, so it copies rather than interrupts. Ctrl+Insert would be one too; Fluent has no Insert key.
pub fn is_copy_chord(press: &fluent::KeyPress) -> bool {
    !press.alt && !press.shift && (press.ctrl || press.cmd) && matches!(press.key, Key::Char('c' | 'C'))
}

/// Whether a key is upstream's `isSelectAllChord`.
pub fn is_select_all_chord(press: &fluent::KeyPress) -> bool {
    !press.alt && !press.shift && (press.ctrl || press.cmd) && matches!(press.key, Key::Char('a' | 'A'))
}

/// What the parser's thread is asked to do, in the order things arrived.
enum Work {
    Bytes(Vec<u8>),
    /// The emulator's own reset: screen, modes, parser state and history.
    Reset,
    /// Answers once everything before it has been parsed.
    Flush(Sender<()>),
}

/// What the parser's thread and the UI thread share besides the `Term`.
struct Shared {
    wake: Arc<dyn Fn() + Send + Sync>,
    sink: Sink,
    /// Set from the moment `wake` fires until a frame looks, which is what makes it fire once per frame at most.
    dirty: AtomicBool,
    title: Mutex<Option<String>>,
    answers: Mutex<(Palette, WindowSize)>,
}

impl Shared {
    fn notify(&self) {
        if !self.dirty.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }
}

/// Runs on the parser's thread with the `Term` locked, so it must never touch that lock.
struct Listener(Arc<Shared>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let s = &self.0;
        match event {
            Event::Wakeup => s.notify(),
            Event::Title(title) => {
                *s.title.lock() = Some(title);
                s.notify();
            }
            Event::ResetTitle => {
                *s.title.lock() = None;
                s.notify();
            }
            Event::PtyWrite(text) => (s.sink)(Output::Reply(text)),
            // Answered from the theme and not from colours the program set: those live in the locked `Term`.
            Event::ColorRequest(index, format) => {
                let rgb = s.answers.lock().0.query(index);
                (s.sink)(Output::Reply(format(rgb)));
            }
            Event::TextAreaSizeRequest(format) => {
                let size = s.answers.lock().1;
                (s.sink)(Output::Reply(format(size)));
            }
            _ => {}
        }
    }
}

/// The parser's thread: drains its queue into the `Term`. Nothing queued means nothing to do but wait, unless a
/// synchronized update (DEC 2026) is holding back a redraw, which has a deadline.
fn parse(rx: Receiver<Work>, term: Arc<FairMutex<Term<Listener>>>, shared: Arc<Shared>, convert_eol: bool) {
    let mut parser = ansi::Processor::<ansi::StdSyncHandler>::new();
    let mut pending: Option<Work> = None;
    loop {
        let work = match pending.take() {
            Some(work) => work,
            None => match parser.sync_timeout().sync_timeout().map(|at| at.saturating_duration_since(Instant::now())) {
                Some(wait) => match rx.recv_timeout(wait) {
                    Ok(work) => work,
                    Err(RecvTimeoutError::Timeout) => {
                        parser.stop_sync(&mut *term.lock());
                        shared.notify();
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(work) => work,
                    Err(_) => return,
                },
            },
        };
        match work {
            Work::Bytes(bytes) => {
                // Everything already queued goes under one lock and one wake, up to a limit that keeps a frame from
                // waiting long for the `Term` while a flood arrives.
                let mut parsed = 0;
                let mut locked = term.lock();
                let mut next = Some(bytes);
                while let Some(bytes) = next.take() {
                    parsed += bytes.len();
                    for &byte in &bytes {
                        // A CR ahead of each LF, the way a line discipline would have put it there.
                        if convert_eol && byte == b'\n' {
                            parser.advance(&mut *locked, b'\r');
                        }
                        parser.advance(&mut *locked, byte);
                    }
                    if parsed >= MAX_LOCKED_PARSE {
                        break;
                    }
                    match rx.try_recv() {
                        Ok(Work::Bytes(more)) => next = Some(more),
                        Ok(other) => pending = Some(other),
                        Err(_) => {}
                    }
                }
                drop(locked);
                // Bytes held back by a synchronized update change nothing on screen yet.
                if parser.sync_bytes_count() < parsed && parsed > 0 {
                    shared.notify();
                }
            }
            Work::Reset => {
                parser = ansi::Processor::new();
                let mut locked = term.lock();
                locked.reset_state();
                locked.grid_mut().clear_history();
                locked.selection = None;
                drop(locked);
                shared.notify();
            }
            Work::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// The screen as of the last look at the `Term`.
struct View {
    cols: usize,
    lines: usize,
    rows: Vec<RowPaint>,
    colors: Colors,
    mode: TermMode,
    cursor: RenderableCursor,
    blinking: bool,
    selection: Option<SelectionRange>,
    /// Lines the viewport sits above the bottom.
    offset: usize,
    history: usize,
}

struct Geometry {
    r: Rect,
    x: f32,
    y: f32,
    cw: f32,
    ch: f32,
}

/// One terminal: its screen, history and selection, and a thread parsing what it is fed.
pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    shared: Arc<Shared>,
    tx: Option<Sender<Work>>,
    parser: Option<JoinHandle<()>>,
    options: Options,
    /// Whether keystrokes and pastes are taken: a shell being killed, a Git write in progress and a read-only page all
    /// say no, while copying and everything the emulator says on its own go on.
    input_enabled: bool,
    view: View,
    palette: Palette,
    scroll: Scroll,
    /// Lines above the viewport at the last look, to tell when the emulator moved it.
    seen_top: usize,
    stale: bool,
    focused: bool,
    blink_start: f32,
    dragging: bool,
    drag_at: (Point, Side),
    wheel: f32,
    resized_at: f32,
    /// The program asked for the mouse, and a press went to it, so the release has to as well.
    mouse_held: bool,
    /// How many presses in a row the last one was: a word on the second, the line on the third.
    clicks: u8,
    pressed_at: f32,
    /// Where a right click landed, until whoever draws the menu takes it.
    context_menu: Option<(f32, f32)>,
}

impl Terminal {
    /// An empty terminal. `wake` is called (coalesced, from any thread) when new output needs a frame.
    pub fn new(options: Options, sink: Sink, wake: Arc<dyn Fn() + Send + Sync>) -> Terminal {
        let (cols, lines) = (80, 24);
        let size = WindowSize { num_lines: lines as u16, num_cols: cols as u16, cell_width: 8, cell_height: 17 };
        let palette = Palette::from_theme(&fluent::Theme::dark());
        let shared = Arc::new(Shared { wake, sink, dirty: AtomicBool::new(false), title: Mutex::new(None), answers: Mutex::new((palette, size)) });
        // OSC 52 stays off: a file someone `cat`s should not be able to fill the clipboard they paste from next.
        let config = Config { scrolling_history: options.history, semantic_escape_chars: WORD_SEPARATORS.to_owned(), default_cursor_style: CursorStyle { shape: CursorShape::Block, blinking: options.blink }, osc52: Osc52::Disabled, ..Config::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &GridSize { cols, lines }, Listener(shared.clone()))));
        let (tx, rx) = mpsc::channel();
        let parser = {
            let (term, shared) = (term.clone(), shared.clone());
            thread::Builder::new().name("terminal parser".to_owned()).spawn(move || parse(rx, term, shared, options.convert_eol)).expect("thread")
        };
        let view = View {
            cols,
            lines,
            rows: Vec::new(),
            colors: Colors::default(),
            mode: TermMode::empty(),
            cursor: RenderableCursor { shape: CursorShape::Hidden, point: Point::new(Line(0), Column(0)) },
            blinking: options.blink,
            selection: None,
            offset: 0,
            history: 0,
        };
        Terminal {
            term,
            shared,
            tx: Some(tx),
            parser: Some(parser),
            options,
            input_enabled: true,
            view,
            palette,
            scroll: Scroll::default(),
            seen_top: 0,
            stale: true,
            focused: false,
            blink_start: 0.0,
            dragging: false,
            drag_at: (Point::new(Line(0), Column(0)), Side::Left),
            wheel: 0.0,
            resized_at: f32::NEG_INFINITY,
            mouse_held: false,
            clicks: 0,
            pressed_at: 0.0,
            context_menu: None,
        }
    }

    fn queue(&self, work: Work) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(work);
        }
    }

    /// What came from the shell. A multi-byte character may span two calls, as a chunk from the host often does.
    pub fn feed(&self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        if !bytes.is_empty() {
            self.queue(Work::Bytes(bytes));
        }
    }

    /// Back to a blank terminal with no history, for a new session: xterm's `reset` and `clear`. Bytes fed after this
    /// land on the blank one, however far behind the parser is.
    pub fn reset(&mut self) {
        self.queue(Work::Reset);
        *self.shared.title.lock() = None;
        self.scroll.scroll_to_end();
        self.stale = true;
    }

    /// Blocks until everything fed so far has been parsed. For tests, and for reading the grid at a known point.
    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        self.queue(Work::Flush(tx));
        let _ = rx.recv();
    }

    /// Whether keystrokes and pastes reach the far end. Selecting and copying work either way.
    pub fn set_input_enabled(&mut self, on: bool) {
        self.input_enabled = on;
    }

    /// Columns and lines. The grid takes its real size from the first `show` that has room.
    pub fn size(&self) -> (u16, u16) {
        (self.view.cols as u16, self.view.lines as u16)
    }

    /// The text of the screen, one string per line without trailing blanks, including lines scrolled off the top.
    pub fn text(&self) -> Vec<String> {
        let term = self.term.lock();
        let grid = term.grid();
        let top = -(grid.history_size() as i32);
        (top..grid.screen_lines() as i32)
            .map(|i| {
                let row = &grid[Line(i)][..];
                let text: String = row.iter().filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER)).map(|c| c.c).collect();
                text.trim_end().to_owned()
            })
            .collect()
    }

    /// The title a program set.
    pub fn title(&self) -> Option<String> {
        self.shared.title.lock().clone().filter(|t| !t.is_empty())
    }

    pub fn has_selection(&self) -> bool {
        self.term.lock().selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    /// The selected text, if anything is selected.
    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// xterm's `selectAll`: every line, scrollback included.
    pub fn select_all(&mut self) {
        let mut term = self.term.lock();
        let (top, bottom, last) = {
            let grid = term.grid();
            (-(grid.history_size() as i32), grid.screen_lines() as i32 - 1, grid.columns() - 1)
        };
        let mut selection = Selection::new(SelectionType::Simple, Point::new(Line(top), Column(0)), Side::Left);
        selection.update(Point::new(Line(bottom), Column(last)), Side::Right);
        term.selection = Some(selection);
        drop(term);
        self.stale = true;
    }

    /// Where a right click landed since this was last asked. The panel answers with the menu xterm leaves to the system: its
    /// Copy and Paste are what a terminal offers there.
    pub fn take_context_menu(&mut self) -> Option<(f32, f32)> {
        self.context_menu.take()
    }

    /// Types into the terminal as if pasted.
    pub fn paste(&mut self, text: &str) {
        if !self.input_enabled {
            return;
        }
        let mode = *self.term.lock().mode();
        self.input(encode_paste(text, mode));
    }

    /// The terminal's own traffic, as opposed to the user's.
    fn write(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            (self.shared.sink)(Output::Reply(String::from_utf8_lossy(&bytes).into_owned()));
        }
    }

    /// Something the user typed: it goes to the shell, and brings the view back to where the shell is.
    fn input(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() || !self.input_enabled {
            return;
        }
        (self.shared.sink)(Output::Input(String::from_utf8_lossy(&bytes).into_owned()));
        self.scroll.scroll_to_end();
        if self.view.selection.is_some() {
            self.term.lock().selection = None;
            self.stale = true;
        }
    }

    /// Draws into `r` and handles input when focused (press inside takes `ui.mem.focus = id`). Says so through the sink
    /// when the grid size changes.
    pub fn show(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let measured = ui.measure("0", &FONT);
        let (cw, ch) = (if measured > 0.0 { measured } else { FONT.size * 0.6 }, FONT.line);
        let g = Geometry { r, x: r.x + PAD_X, y: r.y + PAD_Y, cw, ch };
        let (w, h) = (r.w - PAD_X - GUTTER, r.h - PAD_Y * 2.0);
        // A collapsed pane keeps its grid: reflowing to nothing and back would rewrap all history, twice.
        if w < cw * MIN_COLUMNS as f32 || h < ch {
            return;
        }
        self.adopt_theme(ui);
        // Cleared before the copy, so that output arriving during it asks for another frame.
        if self.shared.dirty.swap(false, Ordering::AcqRel) {
            self.stale = true;
            self.blink_start = ui.time;
        }
        self.fit(ui, w, h, &g);
        if self.stale {
            self.sync();
        }
        self.pointer(ui, id, &g);
        let focused = ui.mem.focus == id;
        if focused != self.focused {
            self.focused = focused;
            self.blink_start = ui.time;
            if self.view.mode.contains(TermMode::FOCUS_IN_OUT) {
                self.write(if focused { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() });
            }
        }
        if focused {
            self.keyboard(ui);
        }
        self.wheel_in_app(ui, &g);
        if self.stale {
            self.sync();
        }
        self.follow_scroll(ui, id, &g);
        ui.with_clip(r, |ui| self.paint(ui, &g, focused));
    }

    fn adopt_theme(&mut self, ui: &Ui<'_>) {
        let palette = Palette::from_theme(&ui.theme);
        if palette != self.palette {
            self.palette = palette;
            self.shared.answers.lock().0 = palette;
            self.view.rows.iter_mut().for_each(|row| row.stale = true);
            self.stale = true;
        }
    }

    fn fit(&mut self, ui: &mut Ui<'_>, w: f32, h: f32, g: &Geometry) {
        let cols = ((w / g.cw) as usize).clamp(MIN_COLUMNS, MAX_COLS);
        let lines = ((h / g.ch) as usize).clamp(MIN_SCREEN_LINES, MAX_LINES);
        if (cols, lines) == (self.view.cols, self.view.lines) {
            return;
        }
        let since = ui.time - self.resized_at;
        if since < RESIZE_EVERY {
            ui.wake_after(RESIZE_EVERY - since);
            return;
        }
        self.resized_at = ui.time;
        self.term.lock().resize(GridSize { cols, lines });
        let (cw, ch) = (g.cw * ui.scale, g.ch * ui.scale);
        self.shared.answers.lock().1 = WindowSize { num_lines: lines as u16, num_cols: cols as u16, cell_width: cw as u16, cell_height: ch as u16 };
        (self.shared.sink)(Output::Resize { cols: cols as u16, lines: lines as u16 });
        self.view.cols = cols;
        self.view.lines = lines;
        self.stale = true;
    }

    /// The one place that reads the `Term` for drawing. Under the lock it only compares and copies cells: shaping
    /// happens after it, so the I/O thread never waits on text.
    fn sync(&mut self) {
        self.stale = false;
        let v = &mut self.view;
        let term = self.term.lock();
        let content = term.renderable_content();
        v.mode = content.mode;
        v.offset = content.display_offset;
        v.selection = content.selection;
        v.cursor = content.cursor;
        let recolor = (0..COUNT).any(|i| content.colors[i] != v.colors[i]);
        if recolor {
            v.colors = *content.colors;
        }
        v.blinking = term.cursor_style().blinking;
        let grid = term.grid();
        v.history = grid.history_size();
        v.rows.resize_with(grid.screen_lines(), RowPaint::default);
        for (i, row) in v.rows.iter_mut().enumerate() {
            let cells = &grid[Line(i as i32 - v.offset as i32)][..];
            if row.cells.as_slice() != cells {
                row.cells.clear();
                row.cells.extend_from_slice(cells);
                row.stale = true;
            }
        }
        drop(term);
        let (palette, colors) = (&self.palette, &v.colors);
        for row in v.rows.iter_mut().filter(|row| row.stale || recolor) {
            row.rebuild(palette, colors);
        }
    }

    fn hit(&self, g: &Geometry, p: (f32, f32)) -> (Point, Side) {
        let fx = ((p.0 - g.x) / g.cw).max(0.0);
        let col = (fx as usize).min(self.view.cols - 1);
        let side = if fx < col as f32 + 0.5 { Side::Left } else { Side::Right };
        let line = (((p.1 - g.y) / g.ch).max(0.0) as usize).min(self.view.lines - 1);
        (viewport_to_point(self.view.offset, Point::new(line, Column(col))), side)
    }

    fn pointer(&mut self, ui: &mut Ui<'_>, id: u64, g: &Geometry) {
        let input = ui.input;
        let hovered = ui.pointer_in(&g.r);
        if input.pressed {
            if hovered {
                ui.mem.focus = id;
            } else if ui.mem.focus == id {
                ui.mem.focus = 0;
            }
        }
        if let Some(p) = input.pointer {
            let over_grid = hovered && p.0 < g.r.right() - GUTTER;
            if over_grid {
                ui.cursor = CursorKind::Text;
            }
            if input.right_pressed && over_grid {
                ui.mem.focus = id;
                self.context_menu = Some(p);
                self.select_word_under_right_click(g, p);
            }
            if self.report_mouse(ui, g, p, over_grid) {
                return;
            }
            if input.pressed && over_grid {
                let (point, side) = self.hit(g, p);
                self.clicks = if input.double_click { self.clicks.max(1).saturating_add(1) } else { 1 };
                self.pressed_at = ui.time;
                let mut term = self.term.lock();
                match term.selection.as_mut().filter(|_| input.shift) {
                    // Shift moves the far end of what is selected, and leaves the near one where it was.
                    Some(selection) => selection.update(point, side),
                    None => {
                        let ty = match self.clicks {
                            1 => SelectionType::Simple,
                            2 => SelectionType::Semantic,
                            _ => SelectionType::Lines,
                        };
                        term.selection = Some(Selection::new(ty, point, side));
                    }
                }
                drop(term);
                self.dragging = true;
                self.drag_at = (point, side);
                self.stale = true;
            } else if self.dragging && input.down {
                self.drag(ui, g, p);
            }
            if input.released && self.dragging && input.alt {
                self.move_cursor_to(ui, g, p);
            }
        }
        if input.released {
            self.dragging = false;
        }
    }

    /// xterm's `rightClickSelectsWord`: off the selection, a right click takes the word under it, so that the menu's Copy has
    /// something to act on. A program that asked for the mouse gets the press and keeps what it has.
    fn select_word_under_right_click(&mut self, g: &Geometry, p: (f32, f32)) {
        if self.view.mode.intersects(TermMode::MOUSE_MODE) {
            return;
        }
        let (point, side) = self.hit(g, p);
        if self.view.selection.is_some_and(|selection| selection.contains(point)) {
            return;
        }
        self.term.lock().selection = Some(Selection::new(SelectionType::Semantic, point, side));
        self.stale = true;
    }

    /// xterm's `altClickMovesCursor`: a quick Option click with nothing selected walks the shell's cursor there with arrow keys.
    fn move_cursor_to(&mut self, ui: &Ui<'_>, g: &Geometry, p: (f32, f32)) {
        let selected = self.term.lock().selection_to_string().is_some_and(|text| text.chars().count() > 1);
        if selected || ui.time - self.pressed_at >= ALT_CLICK_SECONDS || self.view.offset != 0 {
            return;
        }
        let (target, _) = self.hit(g, p);
        let cursor = self.term.lock().grid().cursor.point;
        let mode = self.view.mode;
        self.term.lock().selection = None;
        self.stale = true;
        self.input(cursor_moves((cursor.column.0, cursor.line.0), (target.column.0, target.line.0), self.view.cols, mode));
    }

    /// A program that asked for the mouse gets it, and then the pointer selects nothing. Answers whether it went there.
    fn report_mouse(&mut self, ui: &mut Ui<'_>, g: &Geometry, p: (f32, f32), over_grid: bool) -> bool {
        let input = ui.input;
        let mode = self.view.mode;
        if !mode.intersects(TermMode::MOUSE_MODE) {
            self.mouse_held = false;
            return false;
        }
        let fx = ((p.0 - g.x) / g.cw).max(0.0);
        let col = (fx as usize).min(self.view.cols - 1);
        let line = (((p.1 - g.y) / g.ch).max(0.0) as usize).min(self.view.lines - 1);
        let action = if input.pressed && over_grid {
            self.mouse_held = true;
            Some(MouseAction::Press(0))
        } else if input.right_pressed && over_grid {
            Some(MouseAction::Press(2))
        } else if input.released && self.mouse_held {
            self.mouse_held = false;
            Some(MouseAction::Release(0))
        } else if input.down && self.mouse_held && mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
            Some(MouseAction::Drag(0))
        } else {
            None
        };
        if let Some(action) = action {
            self.write(mouse_report(action, col, line, mode));
        }
        // The press is the app's, so the wheel over the grid is as well.
        if over_grid && input.scroll != 0.0 {
            self.wheel += input.scroll;
            let lines = (self.wheel / g.ch) as i32;
            self.wheel -= lines as f32 * g.ch;
            for _ in 0..lines.unsigned_abs().min(self.view.lines as u32) {
                self.write(mouse_report(MouseAction::Wheel(lines > 0), col, line, mode));
            }
        }
        true
    }

    fn drag(&mut self, ui: &mut Ui<'_>, g: &Geometry, p: (f32, f32)) {
        // Held past an edge, the view keeps moving with no pointer events to drive it.
        let bottom = g.y + self.view.lines as f32 * g.ch;
        let past = if p.1 < g.y { 1 } else if p.1 >= bottom { -1 } else { 0 };
        if past != 0 && self.view.history > 0 {
            self.scroll_by(past, g.ch);
            ui.wake_after(AUTOSCROLL_EVERY);
        }
        let at = self.hit(g, p);
        if at != self.drag_at || past != 0 {
            self.drag_at = at;
            if let Some(selection) = &mut self.term.lock().selection {
                selection.update(at.0, at.1);
            }
            self.stale = true;
        }
    }

    fn keyboard(&mut self, ui: &mut Ui<'_>) {
        let input = ui.input;
        if !input.keys.is_empty() {
            self.blink_start = ui.time;
        }
        if self.input_enabled {
            if input.ime_preedit.as_deref().is_none_or(str::is_empty) {
                let typed = typed_text(input);
                self.input(typed.into_bytes());
            }
            if let Some(commit) = &input.ime_commit {
                self.input(commit.clone().into_bytes());
            }
        }
        for press in &input.keys {
            // Over a selection the chord copies rather than interrupts, gated or not: reading a transcript is not input.
            if is_copy_chord(press) && self.has_selection() {
                self.copy(ui);
                continue;
            }
            if self.options.select_all_chord && is_select_all_chord(press) {
                self.select_all();
                continue;
            }
            // Control is the shell's on Windows, so its terminals paste and copy with Shift held too, as Windows Terminal does.
            if cfg!(windows) && press.ctrl && press.shift && !press.alt {
                match press.key {
                    Key::Char(c) if c.eq_ignore_ascii_case(&'v') => {
                        if let Some(text) = ui.clipboard.get() {
                            self.paste(&text);
                        }
                        continue;
                    }
                    Key::Char(c) if c.eq_ignore_ascii_case(&'c') => {
                        self.copy(ui);
                        continue;
                    }
                    _ => {}
                }
            }
            if press.cmd && !press.ctrl {
                match press.key {
                    Key::Char(c) if c.eq_ignore_ascii_case(&'v') => {
                        if let Some(text) = ui.clipboard.get() {
                            self.paste(&text);
                        }
                    }
                    Key::Char(c) if c.eq_ignore_ascii_case(&'k') => self.clear(),
                    _ => {}
                }
                continue;
            }
            let page = matches!(press.key, Key::PageUp | Key::PageDown) && press.shift && !press.alt && !press.ctrl;
            if page && !self.view.mode.contains(TermMode::ALT_SCREEN) {
                let lines = self.view.lines as i32;
                self.scroll_by(if press.key == Key::PageUp { lines } else { -lines }, FONT.line);
                continue;
            }
            if let Some(bytes) = encode_key(press, self.view.mode) {
                self.input(bytes);
            }
        }
    }

    fn copy(&self, ui: &mut Ui<'_>) {
        let text = self.term.lock().selection_to_string();
        if let Some(text) = text {
            ui.clipboard.set(&text);
        }
    }

    /// Empties screen and history, keeping the line the cursor is on, which is usually the prompt. A full-screen app
    /// owns its screen and is left alone.
    fn clear(&mut self) {
        let mut term = self.term.lock();
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        term.scroll_display(GridScroll::Bottom);
        term.selection = None;
        let grid = term.grid_mut();
        let lines = grid.screen_lines() as i32;
        let at = grid.cursor.point.line.0;
        if at > 0 {
            grid.scroll_up::<Ansi>(&(Line(0)..Line(lines)), at as usize);
            grid.cursor.point.line = Line(0);
        }
        if lines > 1 {
            grid.reset_region::<Ansi, _>(Line(1)..);
        }
        grid.clear_history();
        drop(term);
        self.scroll.scroll_to_end();
        self.stale = true;
    }

    fn wheel_in_app(&mut self, ui: &Ui<'_>, g: &Geometry) {
        let input = ui.input;
        let mode = self.view.mode;
        if input.scroll == 0.0 || !ui.pointer_in(&g.r) || mode.intersects(TermMode::MOUSE_MODE) || !mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            return;
        }
        self.wheel += input.scroll;
        let lines = (self.wheel / g.ch) as i32;
        self.wheel -= lines as f32 * g.ch;
        if lines != 0 {
            self.write(wheel_arrows(lines > 0, lines.unsigned_abs().min(self.view.lines as u32) as usize, mode));
        }
    }

    /// Moves the view up by `lines` at once. A page key or an edge drag should not ease.
    fn scroll_by(&mut self, lines: i32, ch: f32) {
        let max = self.scroll.max_offset();
        let to = (self.scroll.offset - lines as f32 * ch).clamp(0.0, max);
        if to >= max - 1.0 {
            self.scroll.scroll_to_end();
        } else {
            self.scroll.scroll_to(to);
        }
        self.scroll.offset = to;
    }

    /// `Scroll` owns the wheel, the scrollbar and the easing, in pixels from the top of the history. The emulator
    /// owns which line is at the top of the view. This keeps the two agreeing.
    fn follow_scroll(&mut self, ui: &mut Ui<'_>, id: u64, g: &Geometry) {
        let v = &self.view;
        let top = v.history - v.offset;
        // When history is full, new output moves the view within it. Following that keeps the text on screen still.
        if !self.scroll.stick && top != self.seen_top {
            self.scroll.shift((top as f32 - self.seen_top as f32) * g.ch);
        }
        self.seen_top = top;
        if v.mode.contains(TermMode::ALT_SCREEN) {
            self.scroll.stick = true;
        }
        let area = rect(g.r.x, g.y, g.r.w, v.lines as f32 * g.ch);
        let content = (v.history + v.lines) as f32 * g.ch;
        if self.scroll.stick {
            // A burst can add more than `Scroll` would jump unanimated, and a terminal must never lag its own bottom.
            self.scroll.content = content;
            self.scroll.viewport = area.h;
            self.scroll.offset = self.scroll.max_offset();
        }
        self.scroll.update(ui, id ^ SCROLLBAR_SALT, area, content);
        let wanted = if self.scroll.stick { 0 } else { ((self.scroll.max_offset() - self.scroll.offset) / g.ch).round() as usize };
        if wanted != self.view.offset {
            self.term.lock().scroll_display(GridScroll::Delta(wanted as i32 - self.view.offset as i32));
            self.sync();
            self.seen_top = self.view.history - self.view.offset;
        }
    }

    fn paint(&self, ui: &mut Ui<'_>, g: &Geometry, focused: bool) {
        let v = &self.view;
        let ground = NamedColor::Background as usize;
        if v.colors[ground].is_some() {
            ui.fill(g.r, 0.0, self.palette.slot(ground, &v.colors));
        }
        let y_of = |i: usize| g.y + i as f32 * g.ch;
        for (i, row) in v.rows.iter().enumerate() {
            row.paint_fills(ui, g.x, y_of(i), g.cw, g.ch);
        }
        self.paint_selection(ui, g);

        let line = v.cursor.point.line.0 + v.offset as i32;
        let col = v.cursor.point.column.0;
        let on_screen = (0..v.rows.len() as i32).contains(&line) && col < v.cols;
        let preedit = ui.input.ime_preedit.as_deref().filter(|p| focused && !p.is_empty());
        let shape = if on_screen && preedit.is_none() { v.cursor.shape } else { CursorShape::Hidden };

        // Blinking schedules exactly the frames that change the cursor, and stops once nobody is using it.
        let elapsed = (ui.time - self.blink_start).max(0.0);
        let blinking = focused && v.blinking && shape != CursorShape::Hidden && elapsed < BLINK_FOR;
        if blinking {
            ui.wake_after(BLINK - elapsed % BLINK);
        }
        let lit = !blinking || ((elapsed / BLINK) as u32).is_multiple_of(2);

        for (i, row) in v.rows.iter().enumerate() {
            row.paint_text(ui, g.x, y_of(i), g.cw);
        }

        let cursor_row = line.clamp(0, v.rows.len().saturating_sub(1) as i32) as usize;
        let (x, y) = (g.x + col as f32 * g.cw, y_of(cursor_row));
        if shape != CursorShape::Hidden {
            let wide = v.rows[cursor_row].cells.get(col).is_some_and(|c| c.flags.contains(Flags::WIDE_CHAR));
            let cell = rect(x, y, if wide { g.cw * 2.0 } else { g.cw }, g.ch);
            let color = self.palette.slot(NamedColor::Cursor as usize, &v.colors);
            if !focused || shape == CursorShape::HollowBlock {
                let band = text_band(ui, cell);
                paint_cursor_mark(ui, band, color, true);
            } else if lit {
                match shape {
                    CursorShape::Block => {
                        // Over the character's own colour, which still shows where it leaves the mark, as a descender does.
                        let band = text_band(ui, cell);
                        paint_cursor_mark(ui, band, color, false);
                        ui.with_clip(band, |ui| v.rows[cursor_row].paint_at(ui, g.x, y, g.cw, col, self.palette.cursor_text));
                    }
                    CursorShape::Underline => ui.fill(rect(x, y + g.ch - 2.0, cell.w, 2.0), 0.0, color),
                    CursorShape::Beam => ui.fill(rect(x, y, 1.5, g.ch), 0.0, color),
                    _ => {}
                }
            }
        }
        if focused {
            ui.ime_area = Some(rect(x, y, 2.0, g.ch));
        }
        if let Some(text) = preedit {
            self.paint_preedit(ui, g, x, y, text);
        }
    }

    fn paint_selection(&self, ui: &mut Ui<'_>, g: &Geometry) {
        let v = &self.view;
        let Some(sel) = v.selection else { return };
        for i in 0..v.rows.len() {
            let line = i as i32 - v.offset as i32;
            if line < sel.start.line.0 || line > sel.end.line.0 {
                continue;
            }
            let from = if sel.is_block || line == sel.start.line.0 { sel.start.column.0 } else { 0 };
            let to = if sel.is_block || line == sel.end.line.0 { sel.end.column.0 } else { v.cols - 1 };
            let span = rect(g.x + from as f32 * g.cw, g.y + i as f32 * g.ch, (to + 1 - from) as f32 * g.cw, g.ch);
            ui.fill(span, 0.0, ui.theme.selection);
        }
    }

    /// The composition sits over the grid's own text, and a layer's rects draw below its glyphs, so it needs a layer
    /// of its own to hide what it covers.
    fn paint_preedit(&self, ui: &mut Ui<'_>, g: &Geometry, x: f32, y: f32, text: &str) {
        let color = ui.theme.accent_text;
        let w = ui.text.line(text, &FONT, color).width / ui.scale;
        let x = x.min(g.r.right() - GUTTER - w).max(g.x);
        let saved = std::mem::replace(&mut ui.layer, LAYER_POPUP);
        ui.fill(rect(x, y, w, g.ch), 0.0, self.palette.ground);
        ui.label(x, y, text, &FONT, color);
        ui.fill(rect(x, y + g.ch - 2.0, w, 1.0), 0.0, color);
        ui.layer = saved;
    }
}

/// The part of a cell the text stands in: from the top of a capital down to the baseline, on whole pixels so that its edges meet the
/// text's. The block cursor stands exactly there, level with the characters on either side of it.
fn text_band(ui: &mut Ui<'_>, cell: Rect) -> Rect {
    let run = ui.text.line("H", &FONT, Color::WHITE);
    let baseline = run.baseline();
    let above = run.ink().map_or(FONT.size * 0.72 * ui.scale, |ink| ink[1]).round();
    let snap = |value: f32| (value * ui.scale).round() / ui.scale;
    rect(snap(cell.x), snap(cell.y) + (baseline - above) / ui.scale, snap(cell.w), above / ui.scale)
}

/// The block cursor: the brand's mark cut to `band`, or its outline while the keyboard is elsewhere. It goes in with the glyphs, in
/// their order: it covers the text drawn before it, and what is drawn after it lands on top.
fn paint_cursor_mark(ui: &mut Ui<'_>, band: Rect, color: Color, hollow: bool) {
    let Color { r, g, b, .. } = color;
    let hex = format!("#{r:02x}{g:02x}{b:02x}");
    // An outline's line is centred on the path, so the path comes in by half of it to keep the line inside the band.
    let inset = if hollow { 0.5 } else { 0.0 };
    let path = fluent::mark::cell_path(band.w - inset * 2.0, band.h - inset * 2.0);
    let paint = if hollow { format!(r#"fill="none" stroke="{hex}" stroke-width="1""#) } else { format!(r#"fill="{hex}""#) };
    // A picture is square and placed by its centre, so the mark starts at its corner and keeps the band's whole pixels.
    let side = band.w.max(band.h);
    let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {side} {side}"><path d="{path}" transform="translate({inset} {inset})" {paint}/></svg>"#);
    let key = format!("term-cursor:{}x{}:{hex}:{hollow}", band.w, band.h);
    ui.svg_icon(&key, &svg, band.x + side * 0.5, band.y + side * 0.5, side);
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // No frame will clear this again, so whatever is still queued wakes nobody.
        self.shared.dirty.store(true, Ordering::Release);
        // Closing the queue ends the parser once it has caught up. It holds nothing the UI thread waits for, so this
        // does not join it: a flood being parsed must not stall the frame that drops the terminal.
        self.tx = None;
        drop(self.parser.take());
    }
}
