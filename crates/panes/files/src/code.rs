//! Code on screen (upstream's `NumberedCode`, `MarkdownCodeBlock` and `NotebookCode`): lines wrapped to the box, coloured a
//! screenful at a time, selectable across lines. Painting only touches the rows in view, so a long file costs its index.

use mewrk_client::prelude::*;
use mewrk_panes::highlight::{self, Lang, State, Token};

const CODE_TY: TypeStyle = TypeStyle { size: 11.5, line: 18.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
/// Past this a line is not worth colouring or wrapping exactly, and nobody is reading it.
const MAX_LINE_BYTES: usize = 64 * 1024;
const TAB_UNITS: u32 = 6;

/// Splits on newlines the way the viewer shows a file: a final newline is not one more empty line.
pub fn split_code_lines(content: &str) -> Vec<(u32, u32)> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let bytes = content.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            let end = if i > start && bytes[i - 1] == b'\r' { i - 1 } else { i };
            lines.push((start as u32, end as u32));
            start = i + 1;
        }
    }
    if start < bytes.len() || lines.is_empty() {
        lines.push((start as u32, bytes.len() as u32));
    }
    lines
}

fn units(c: char) -> u32 {
    if c == '\t' {
        TAB_UNITS
    } else if fluent::text::is_cjk(c) || fluent::text::is_emoji(c) {
        5
    } else {
        3
    }
}

/// Byte ranges of `line` after wrapping at `cols` units, breaking anywhere (`overflow-wrap: anywhere`).
pub fn wrap_segments(line: &str, cols: u32) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let (mut start, mut used) = (0usize, 0u32);
    for (at, c) in line.char_indices() {
        let u = units(c);
        if used + u > cols && used > 0 {
            out.push((start, at));
            start = at;
            used = 0;
        }
        used += u;
    }
    out.push((start, line.len()));
    out
}

struct Layout {
    width: f32,
    cols: u32,
    advance: f32,
    /// The visual row each line starts at, and one past the end.
    starts: Vec<u32>,
}

/// What a code surface keeps between frames.
pub struct CodeState {
    content: Rc<str>,
    lines: Vec<(u32, u32)>,
    lang: Option<Lang>,
    tokens: Vec<Vec<(u32, Token)>>,
    hl_state: State,
    layout: Option<Layout>,
    /// Anchor and focus as (line, byte in line).
    selection: Option<((usize, usize), (usize, usize))>,
    scratch: Vec<(usize, Token)>,
}

impl CodeState {
    fn new(content: Rc<str>, lang: Option<Lang>) -> Self {
        let lines = split_code_lines(&content);
        Self { content, lines, lang, tokens: Vec::new(), hl_state: State::default(), layout: None, selection: None, scratch: Vec::new() }
    }

    fn line(&self, i: usize) -> &str {
        let (a, b) = self.lines[i];
        &self.content[a as usize..b as usize]
    }

    /// Colours are a state machine over the lines, so the lines above any one on screen are coloured first.
    fn tokens_up_to(&mut self, last: usize) {
        let Some(lang) = self.lang else { return };
        while self.tokens.len() <= last && self.tokens.len() < self.lines.len() {
            let i = self.tokens.len();
            let (a, b) = self.lines[i];
            let text = &self.content[a as usize..b as usize];
            highlight::line(lang, &mut self.hl_state, text, &mut self.scratch);
            self.tokens.push(self.scratch.iter().map(|(at, t)| (*at as u32, *t)).collect());
        }
    }

    fn relayout(&mut self, width: f32, gutter_digits: u32, numbered: bool, advance: f32) {
        if self.layout.as_ref().is_some_and(|l| (l.width - width).abs() < 0.5 && (l.advance - advance).abs() < 0.01) {
            return;
        }
        let gutter = if numbered { (gutter_digits + 1) as f32 * advance } else { 0.0 };
        let cols = (((width - gutter - 10.0).max(advance * 8.0)) / advance * 3.0).floor() as u32;
        let mut starts = Vec::with_capacity(self.lines.len() + 1);
        let mut row = 0u32;
        for i in 0..self.lines.len() {
            starts.push(row);
            let text = self.line(i);
            let rows = if text.len() > MAX_LINE_BYTES {
                (text.len() as u32 * 3 / cols.max(1)) + 1
            } else if text.is_ascii() && !text.contains('\t') {
                ((text.len() as u32 * 3).saturating_sub(1) / cols.max(1)) + 1
            } else {
                wrap_segments(text, cols).len() as u32
            };
            row += rows;
        }
        starts.push(row);
        self.layout = Some(Layout { width, cols, advance, starts });
    }

    /// The first visual row of a 1-based line, for a jump.
    pub fn row_of_line(&self, line: u32) -> Option<u32> {
        let layout = self.layout.as_ref()?;
        layout.starts.get(line.checked_sub(1)? as usize).copied().filter(|_| (line as usize) <= self.lines.len())
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line_at_row(&self, row: u32) -> usize {
        let Some(layout) = &self.layout else { return 0 };
        match layout.starts.binary_search(&row) {
            Ok(i) => i.min(self.lines.len().saturating_sub(1)),
            Err(i) => i.saturating_sub(1).min(self.lines.len().saturating_sub(1)),
        }
    }

    /// The (line, byte) a point in the surface falls on.
    fn hit(&self, x: f32, y: f32, gutter: f32) -> (usize, usize) {
        let Some(layout) = &self.layout else { return (0, 0) };
        let row = ((y / CODE_TY.line).floor().max(0.0) as u32).min(layout.starts.last().copied().unwrap_or(1).saturating_sub(1));
        let line = self.line_at_row(row);
        let text = self.line(line);
        let segments = wrap_segments(text, layout.cols);
        let seg = segments[((row - layout.starts[line]) as usize).min(segments.len() - 1)];
        let mut used = 0.0;
        let want = (x - gutter).max(0.0);
        for (at, c) in text[seg.0..seg.1].char_indices() {
            let w = units(c) as f32 / 3.0 * layout.advance;
            if used + w * 0.5 > want {
                return (line, seg.0 + at);
            }
            used += w;
        }
        (line, seg.1)
    }

    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        if lo == hi {
            return None;
        }
        let start = self.lines[lo.0].0 as usize + lo.1;
        let end = self.lines[hi.0].0 as usize + hi.1;
        Some(self.content[start..end].to_owned())
    }
}

/// The state behind a surface's key, for a caller that needs to know where a line is.
pub fn code_state(key: impl std::hash::Hash, content: &Rc<str>, language: Option<&str>, name: &str) -> Local<CodeState> {
    let lang = language.and_then(highlight::lang_of).or_else(|| if language.is_some() { highlight::lang_of(name) } else { None });
    let state = use_state(("code-state", key), || CodeState::new(content.clone(), lang));
    let stale = state.with(|s| !Rc::ptr_eq(&s.content, content) && *s.content != **content);
    if stale {
        state.set(CodeState::new(content.clone(), lang));
    }
    state
}

pub struct CodeOptions {
    pub numbered: bool,
    pub lit_line: Option<u32>,
    pub on_line_menu: Option<Rc<dyn Fn(&mut App, u32, (f32, f32))>>,
    pub fallback_color: Option<Color>,
}

impl Default for CodeOptions {
    fn default() -> Self {
        Self { numbered: false, lit_line: None, on_line_menu: None, fallback_color: None }
    }
}

thread_local! {
    static JUMP_RESULT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether the jump `numbered_code` was asked for this frame has landed, and on a line that exists. For the owner of the
/// request to clear it, once the view is done with the state.
pub fn take_jump_result() -> Option<bool> {
    JUMP_RESULT.with(|j| j.take())
}

/// The code itself, as tall as it needs. Put it in a scroller or in a document.
pub fn code_surface(key: impl std::hash::Hash + Clone, state: Local<CodeState>, options: CodeOptions) -> E {
    let id = hash_key(("code-surface", key.clone()));
    let digits = state.with(|s| s.lines.len().to_string().len() as u32);
    let CodeOptions { numbered, lit_line, on_line_menu, fallback_color } = options;
    let measure_state = state.clone();
    let menu_state = state.clone();
    // What is painted here says nothing to a reader by itself.
    let spoken = a11y_active().then(|| state.with(|s| s.content.to_string()));
    let surface = custom::<App>(move |_app, ui, r| {
        let theme = ui.theme;
        let mut guard = state.borrow_mut();
        let s = &mut *guard;
        let advance = ui.measure("0", &CODE_TY).max(1.0);
        s.relayout(r.w, digits, numbered, advance);
        let gutter = if numbered { (digits + 1) as f32 * advance } else { 0.0 };
        let line_h = CODE_TY.line;

        // Pointer: a press starts a selection, and a drag extends it.
        let resp = ui.interact(id, r);
        if resp.hovered {
            ui.cursor = CursorKind::Text;
        }
        if resp.hovered && ui.input.pressed
            && let Some((px, py)) = ui.input.pointer
        {
            let at = s.hit(px - r.x, py - r.y, gutter);
            s.selection = Some((at, at));
            ui.mem.focus = id;
        }
        if ui.mem.active == id && ui.input.down
            && let (Some((px, py)), Some((anchor, _))) = (ui.input.pointer, s.selection)
        {
            s.selection = Some((anchor, s.hit(px - r.x, py - r.y, gutter)));
            ui.animating = true;
        }
        if ui.mem.focus == id {
            for k in &ui.input.keys {
                match k.key {
                    Key::Char('c') | Key::Char('C') if k.cmd => {
                        if let Some(text) = s.selected_text() {
                            ui.clipboard.set(&text);
                        }
                    }
                    Key::Char('a') | Key::Char('A') if k.cmd => {
                        let last = s.lines.len() - 1;
                        s.selection = Some(((0, 0), (last, s.line(last).len())));
                    }
                    _ => {}
                }
            }
        }

        // Rows in view.
        let clip = ui.clip.intersect(&r);
        if clip.h <= 0.0 {
            return;
        }
        let Some(total) = s.layout.as_ref().and_then(|l| l.starts.last().copied()) else { return };
        let first_row = (((clip.y - r.y) / line_h).floor().max(0.0)) as u32;
        let last_row = (((clip.bottom() - r.y) / line_h).ceil() as u32).min(total);
        if first_row >= last_row {
            return;
        }
        let first_line = s.line_at_row(first_row);
        let last_line = s.line_at_row(last_row.saturating_sub(1));
        s.tokens_up_to(last_line);
        let cols = s.layout.as_ref().map_or(1, |l| l.cols);
        let selection = s.selection.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        let plain = fallback_color.unwrap_or(theme.text_primary);
        for line in first_line..=last_line {
            let row = s.layout.as_ref().map_or(0, |l| l.starts[line]);
            let y = r.y + row as f32 * line_h;
            let next_row = s.layout.as_ref().map_or(0, |l| l.starts[line + 1]);
            if lit_line == Some(line as u32 + 1) {
                ui.fill(rect(r.x, y, r.w, (next_row - row) as f32 * line_h), 0.0, theme.accent.with_alpha(0.18));
            }
            if numbered {
                let number = (line + 1).to_string();
                let w = number.len() as f32 * advance;
                ui.label(r.x + digits as f32 * advance - w, y, &number, &CODE_TY, theme.text_tertiary);
            }
            let (a, b) = s.lines[line];
            let text: &str = &s.content[a as usize..b as usize];
            if text.is_empty() {
                continue;
            }
            let segments = if text.len() > MAX_LINE_BYTES { vec![(0, text.len().min(MAX_LINE_BYTES))] } else { wrap_segments(text, cols) };
            for (si, (from, to)) in segments.iter().copied().enumerate() {
                let row_y = y + si as f32 * line_h;
                if row_y + line_h < clip.y || row_y > clip.bottom() {
                    continue;
                }
                let x0 = r.x + gutter;
                if let Some((lo, hi)) = selection
                    && line >= lo.0
                    && line <= hi.0
                {
                    let start = if line == lo.0 { lo.1.max(from) } else { from };
                    let end = if line == hi.0 { hi.1.min(to) } else { to };
                    if start < end || (line < hi.0 && to == text.len()) {
                        let width = |bytes: &str| bytes.chars().map(|c| units(c) as f32 / 3.0 * advance).sum::<f32>();
                        let sx = x0 + width(&text[from..start.min(end)]);
                        let sw = width(&text[start.min(end)..end]).max(2.0);
                        ui.fill(rect(sx, row_y, sw, line_h), 2.0, theme.selection);
                    }
                }
                let tokens = s.tokens.get(line).map(Vec::as_slice).unwrap_or(&[]);
                let mut x = x0;
                let mut at = from;
                // Positions come from the grid, not from what was drawn: a run of nothing but spaces draws nothing and still
                // has its width, which is what indentation is.
                let width_of = |text: &str| text.chars().map(|c| units(c) as f32 / 3.0 * advance).sum::<f32>();
                let mut draw = |ui: &mut Ui<'_>, piece: &str, color: Color| {
                    let shown = piece.trim_start();
                    let lead = width_of(&piece[..piece.len() - shown.len()]);
                    if !shown.is_empty() {
                        if shown.contains('\t') {
                            ui.label(x + lead, row_y, &shown.replace('\t', "  "), &CODE_TY, color);
                        } else {
                            ui.label(x + lead, row_y, shown, &CODE_TY, color);
                        }
                    }
                    x += width_of(piece);
                };
                for (i, (start, token)) in tokens.iter().enumerate() {
                    let start = *start as usize;
                    let end = tokens.get(i + 1).map_or(text.len(), |n| n.0 as usize);
                    if end <= from || start >= to {
                        continue;
                    }
                    let (piece_from, piece_to) = (start.max(from), end.min(to));
                    if piece_from > at {
                        draw(ui, &text[at..piece_from], plain);
                    }
                    let color = if *token == Token::Plain { plain } else { highlight::color(*token, &theme) };
                    draw(ui, &text[piece_from..piece_to], color);
                    at = piece_to;
                }
                if at < to {
                    draw(ui, &text[at..to], plain);
                }
            }
        }
    })
    .key(("code-surface", key))
    .min_w(0.0)
    .shrink0()
    .measure(move |_, ui, known, avail| {
        let w = known.unwrap_or(if avail.is_finite() { avail } else { 600.0 });
        let advance = ui.measure("0", &CODE_TY).max(1.0);
        let mut s = measure_state.borrow_mut();
        s.relayout(w, digits, numbered, advance);
        let rows = s.layout.as_ref().and_then(|l| l.starts.last().copied()).unwrap_or(1);
        (w, rows as f32 * CODE_TY.line)
    })
    .map(|e| match spoken {
        Some(words) => e.aria_label(words),
        None => e,
    });
    match on_line_menu {
        Some(menu) => surface.on_context_menu(move |app: &mut App, ev| {
            let line = {
                let s = menu_state.borrow();
                let row = ((ev.y - ev.rect.y) / CODE_TY.line).floor().max(0.0) as u32;
                s.line_at_row(row) + 1
            };
            menu(app, line as u32, (ev.x, ev.y));
        }),
        None => surface,
    }
}

/// A file as numbered text, scrolling inside itself.
pub fn numbered_code(key: String, content: &Rc<str>, language: Option<&str>, name: &str, lit_line: Option<u32>, on_line_menu: Option<Rc<dyn Fn(&mut App, u32, (f32, f32))>>, jump: Option<u32>) -> E {
    let state = code_state(key.as_str(), content, language, name);
    let scroller = ("numbered-code", key.clone());
    if let Some(line) = jump {
        // Centred once the rows and the scroller exist; a line past the end lands nowhere and lights nothing.
        let (laid_out, row, count) = state.with(|s| (s.layout.is_some(), s.row_of_line(line), s.line_count()));
        if laid_out && (line as usize) > count {
            JUMP_RESULT.with(|j| j.set(Some(false)));
        } else if let (Some(row), Some((_, _, viewport))) = (row, scroll_state(scroller.clone()))
            && viewport > 0.0
        {
            scroll_request(scroller.clone(), ScrollRequest::To((row as f32 * CODE_TY.line - (viewport - CODE_TY.line) * 0.5).max(0.0)));
            JUMP_RESULT.with(|j| j.set(Some(true)));
        }
    }
    let body = div().key(scroller).flex1().min_w(0.0).min_h(0.0).py(4.0).scroll_y().aria_label(name);
    body.child(code_surface(key, state, CodeOptions { numbered: true, lit_line, on_line_menu, fallback_color: None }))
}

/// A fenced block inside a document: `MarkdownCodeBlock`.
pub fn code_block(key: impl std::hash::Hash + Clone, code: &str, language: Option<&str>) -> E {
    let t_ = theme();
    let content: Rc<str> = Rc::from(code.strip_suffix('\n').unwrap_or(code));
    let state = code_state(key.clone(), &content, language, "");
    div().bg(t_.code_bg).radius(6.0).p(8.0).shrink0().child(code_surface(key, state, CodeOptions::default()))
}

/// Terminal text with its colours: `AnsiText`.
pub fn ansi_text(key: impl std::hash::Hash, source: &str, base: Color) -> E {
    use crate::format::{parse_ansi, AnsiColor};
    let t_ = theme();
    let named = |n: u8| -> Color {
        const LIGHT: [u32; 16] = [0x000000, 0xB02A37, 0x2E7D32, 0x8A6D00, 0x1F5FBF, 0x8E3FA8, 0x167F8A, 0x707070, 0x555555, 0xD6455A, 0x3C9A40, 0xA88600, 0x3A78D8, 0xA857C2, 0x1E9AA8, 0x202020];
        const DARK: [u32; 16] = [0x808080, 0xF2777A, 0x99CC99, 0xFFCC66, 0x6699CC, 0xCC99CC, 0x66CCCC, 0xD0D0D0, 0xA0A0A0, 0xFF8F8F, 0xB0E0B0, 0xFFE08A, 0x88B8E8, 0xE0B0E0, 0x88E0E0, 0xFFFFFF];
        Color::rgb(if t_.dark { DARK[n as usize & 15] } else { LIGHT[n as usize & 15] })
    };
    let ty = TypeStyle { size: 11.5, line: 17.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
    let parts: Vec<Span> = parse_ansi(source).into_iter().map(|run| {
        let color = match run.foreground {
            Some(AnsiColor::Named(n)) => named(n),
            Some(AnsiColor::Rgb(r, g, b)) => Color::rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32),
            None => base,
        };
        let weight = if run.bold { Weight::Semibold } else { Weight::Regular };
        Span { text: Arc::from(run.text), ty: TypeStyle { weight, italic: run.italic, ..ty }, color: Some(color) }
    }).collect();
    spans(parts).key(("ansi", key)).selectable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_final_newline_is_not_one_more_line() {
        assert_eq!(split_code_lines("a\nb\n").len(), 2);
        assert_eq!(split_code_lines("a\r\nb").len(), 2);
        assert_eq!(split_code_lines("").len(), 1);
    }

    #[test]
    fn wraps_at_the_box_and_counts_cjk_wider() {
        assert_eq!(wrap_segments("abcdef", 9), [(0, 3), (3, 6)]);
        assert_eq!(wrap_segments("", 9), [(0, 0)]);
        assert_eq!(wrap_segments("中文", 5), [(0, 3), (3, 6)]);
    }
}
