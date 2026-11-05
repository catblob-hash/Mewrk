//! Text field with selection, clipboard and IME composition: multi-line by default, one line for an `<input>`.

use std::ops::Range;

use crate::spell::Spelling;
use crate::text::Doc;
use crate::theme::*;
use crate::ui::*;

pub struct TextEdit {
    pub text: String,
    caret: usize,
    anchor: usize,
    preedit: String,
    doc: Doc,
    dirty: bool,
    blink_start: f32,
    drag: bool,
    scroll: f32,
    /// How far a single line is scrolled sideways, in logical pixels.
    scroll_x: f32,
    last_w: f32,
    obscured: bool,
    read_only: bool,
    /// One line that never wraps: it scrolls sideways and sits in the middle of its box.
    single_line: bool,
    had_focus: bool,
    /// How many presses in a row have landed here: the second selects a word and the third the line.
    clicks: u8,
    /// Ranges of `text` that behave as one character: the caret never rests inside one and a selection never ends in one.
    atoms: Vec<Range<usize>>,
    atom_fill: Option<Color>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Typing joins the undo step before it, as a textarea's does.
    typing: bool,
    revision: u64,
    /// What the caret was last brought into view for, so the wheel can move the text away from it.
    revealed: (u64, usize, usize, u32),
    /// `spellCheck`. `None` with it off, when nothing is asked and nothing is drawn.
    spelling: Option<Spelling>,
    /// Where the text was last drawn and at what scale, for telling which offset a point is at.
    drawn: (Rect, f32),
    /// What a menu pressed on the field's behalf, heard with the next frame's keys.
    menu_keys: Vec<KeyPress>,
    /// Ink of its own, in place of the theme's text colour.
    ink: Option<Color>,
    /// `text-align: right` on one line that fits.
    align_end: bool,
}

/// One step of the undo history.
struct Snapshot {
    text: String,
    caret: usize,
    anchor: usize,
}

/// How many steps a field remembers.
const UNDO_LIMIT: usize = 200;

/// What a field's owner decides about its input before the field acts on it.
pub trait EditHooks {
    /// A key press. `true` takes it, and the field leaves it alone.
    fn key(&mut self, _edit: &mut TextEdit, _press: &KeyPress) -> bool {
        false
    }

    /// What a copy or a cut of `range` puts on the clipboard, when it is not those bytes.
    fn copied(&mut self, _edit: &TextEdit, _range: Range<usize>) -> Option<String> {
        None
    }

    /// A paste, with the clipboard's text if it has any. `true` takes it.
    fn paste(&mut self, _edit: &mut TextEdit, _text: Option<String>) -> bool {
        false
    }

    /// The ranges of `text` that behave as one character, asked again whenever the text may have changed.
    fn atoms(&mut self, _text: &str) -> Vec<Range<usize>> {
        Vec::new()
    }
}

struct NoHooks;

impl EditHooks for NoHooks {}

/// How soon to look again for what the spell checker was asked, in seconds.
const SPELLING_POLL: f32 = 0.05;
/// How far above the foot of the line a squiggle's top lies, in logical pixels.
const SQUIGGLE_LIFT: f32 = 3.0;

/// A wavy line along `x..x + w`, its top at `y`: the mark of a misspelt word. Up and down, a pixel high, in steps of two.
fn squiggle(ui: &mut Ui<'_>, x: f32, y: f32, w: f32, color: Color) {
    const STEP: f32 = 2.0;
    const HEIGHT: f32 = 1.0;
    let mut at = x;
    let mut rising = false;
    while at < x + w {
        let next = (at + STEP).min(x + w);
        let (from, to) = if rising { (y + HEIGHT, y) } else { (y, y + HEIGHT) };
        ui.line(at, from, next, to, 1.0, color);
        at = next;
        rising = !rising;
    }
}

/// What an obscured character is drawn as, and its length in the laid-out text.
const BULLET: &str = "\u{2022}";

/// Whether Command with an arrow or Backspace means the whole line, as on a Mac; elsewhere that key is Control, which means a word.
const COMMAND_MEANS_LINE: bool = cfg!(target_os = "macos");

/// The width a single line is laid out at, in physical pixels: wider than any line that fits in memory, so it never wraps.
const NO_WRAP: f32 = 1.0e8;

/// The same bytes with line ends as spaces, so a value set from outside stays on its one line with its offsets intact.
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct EditResponse {
    pub submitted: bool,
    pub changed: bool,
}

fn prev_char(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map_or(0, |(p, _)| p)
}

fn next_char(s: &str, i: usize) -> usize {
    s[i..].chars().next().map_or(i, |c| i + c.len_utf8())
}

fn prev_word(s: &str, i: usize) -> usize {
    let head = s[..i].trim_end();
    head.rfind(|c: char| c.is_whitespace()).map_or(0, |p| p + head[p..].chars().next().map_or(1, char::len_utf8))
}

fn next_word(s: &str, i: usize) -> usize {
    let rest = &s[i..];
    let skip = rest.len() - rest.trim_start().len();
    let word = rest[skip..].find(char::is_whitespace).unwrap_or(rest.len() - skip);
    i + skip + word
}

impl TextEdit {
    pub fn new() -> Self {
        Self { text: String::new(), caret: 0, anchor: 0, preedit: String::new(), doc: Doc::new(1.0), dirty: true, blink_start: 0.0, drag: false, scroll: 0.0, scroll_x: 0.0, last_w: 0.0, obscured: false, read_only: false, single_line: false, had_focus: false, clicks: 0, atoms: Vec::new(), atom_fill: None, undo: Vec::new(), redo: Vec::new(), typing: false, revision: 0, revealed: (u64::MAX, 0, 0, 0), spelling: None, drawn: (rect(0.0, 0.0, 0.0, 0.0), 1.0), menu_keys: Vec::new(), ink: None, align_end: false }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.caret = self.text.len();
        self.anchor = self.caret;
        self.dirty = true;
        // Text set from outside is another document: undoing into the last one would be a surprise.
        self.undo.clear();
        self.redo.clear();
        self.typing = false;
        self.revision += 1;
    }

    pub fn clear(&mut self) {
        self.set_text(String::new());
    }

    /// Inserts at the caret, replacing any selection, as one undo step.
    pub fn insert(&mut self, s: &str) {
        self.remember(false);
        self.replace_selection(s);
    }

    /// The selection as a byte range: empty at the caret when nothing is selected.
    pub fn selection_range(&self) -> Range<usize> {
        self.selection()
    }

    /// The caret's byte offset, which is the selection's moving end.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Takes no input and no focus: `disabled` on a textarea.
    pub fn set_read_only(&mut self, on: bool) {
        self.read_only = on;
    }

    /// The colour the ranges `EditHooks::atoms` names are drawn on.
    /// Its one line against the far end of the box, while it fits.
    pub fn set_align_end(&mut self, on: bool) {
        self.align_end = on;
    }

    /// Its text in this colour rather than the theme's.
    pub fn set_ink(&mut self, ink: Option<Color>) {
        if self.ink != ink {
            self.ink = ink;
            self.dirty = true;
        }
    }

    pub fn set_atom_fill(&mut self, fill: Option<Color>) {
        self.atom_fill = fill;
    }

    /// How far the text is scrolled, in logical pixels.
    pub fn scroll_offset(&self) -> f32 {
        self.scroll
    }

    /// Whether composition is under way: what is shown is not text yet, and keys belong to the input method.
    pub fn is_composing(&self) -> bool {
        !self.preedit.is_empty()
    }

    /// Selects a byte range of the text, caret at its end. A rename selects the name without its extension.
    pub fn select_range(&mut self, from: usize, to: usize) {
        let clamp = |at: usize| {
            let mut at = at.min(self.text.len());
            while !self.text.is_char_boundary(at) {
                at -= 1;
            }
            at
        };
        self.anchor = clamp(from);
        self.caret = clamp(to);
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// The press under way stops moving the caret, so a selection made for it survives until the release.
    pub fn end_drag(&mut self) {
        self.drag = false;
    }

    /// Draws every character as a bullet: `type="password"`.
    pub fn set_obscured(&mut self, on: bool) {
        if self.obscured != on {
            self.obscured = on;
            self.dirty = true;
        }
    }

    /// One line, `<input>` rather than `<textarea>`: it scrolls sideways, sits in the middle of its box, and a line end
    /// cannot get in.
    pub fn set_single_line(&mut self, on: bool) {
        if self.single_line != on {
            self.single_line = on;
            self.dirty = true;
        }
    }

    /// `spellCheck`: a wavy line under each misspelt word once it is finished. An obscured field never has one.
    pub fn set_spell_check(&mut self, on: bool) {
        if on != self.spelling.is_some() {
            self.spelling = on.then(Spelling::default);
        }
    }

    /// The words drawn as misspelt, as byte ranges of the text.
    pub fn misspelt(&self) -> &[Range<usize>] {
        self.spelling.as_ref().map_or(&[], Spelling::misspelt)
    }

    /// Which offset of the text lies under a point of the window, as of the last frame.
    pub fn offset_at(&self, x: f32, y: f32) -> usize {
        let (inner, scale) = self.drawn;
        let doc_y = if self.single_line { 0.0 } else { (y - inner.y + self.scroll) * scale };
        self.from_doc(self.doc.hit((x - inner.x + self.scroll_x) * scale, doc_y))
    }

    /// A right press, for the menu it opens. A press on the selection leaves it, a press on a misspelt word selects the word, and
    /// any other puts the caret there. Answers with the misspelt word under the press.
    pub fn press_for_menu(&mut self, x: f32, y: f32) -> Option<Range<usize>> {
        let at = self.offset_at(x, y);
        let word = self.misspelt().iter().find(|word| word.start <= at && at <= word.end).cloned();
        let selection = self.selection();
        if selection.is_empty() || at < selection.start || selection.end < at {
            match &word {
                Some(word) => self.select_range(word.start, word.end),
                None => {
                    self.select_range(at, at);
                    self.snap_to_atoms(None);
                }
            }
        }
        self.typing = false;
        self.drag = false;
        word
    }

    /// Replaces a range of the text as one undo step, with the caret after what went in: a menu's correction.
    pub fn replace_range(&mut self, range: Range<usize>, with: &str) {
        self.remember(false);
        self.select_range(range.start, range.end);
        self.replace_selection(with);
    }

    /// A key a menu pressed: it is heard with the next frame's keys, as long as the field has the focus by then.
    pub fn press_from_menu(&mut self, press: KeyPress) {
        self.menu_keys.push(press);
    }

    /// Text arriving from a paste or an input method, as this field can hold it.
    fn admissible<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        if self.single_line && s.contains(['\n', '\r']) {
            s.replace(['\n', '\r'], "").into()
        } else {
            s.into()
        }
    }

    /// An offset into the text as an offset into what was laid out, which differ while obscured.
    fn to_doc(&self, i: usize) -> usize {
        if self.obscured {
            self.text[..i.min(self.text.len())].chars().count() * BULLET.len()
        } else if i > self.caret {
            // What is being composed sits in the laid-out text at the caret, and pushes what follows along.
            i + self.preedit.len()
        } else {
            i
        }
    }

    fn from_doc(&self, i: usize) -> usize {
        if self.obscured {
            self.text.char_indices().nth(i / BULLET.len()).map_or(self.text.len(), |(at, _)| at)
        } else if i >= self.caret + self.preedit.len() {
            i - self.preedit.len()
        } else if i > self.caret {
            self.caret
        } else {
            i
        }
    }

    /// Lays out at `width` if needed and reports the height the text wants, in logical pixels.
    pub fn measure(&mut self, ui: &mut Ui<'_>, ty: &TypeStyle, width: f32) -> f32 {
        self.relayout(ui, ty, width);
        self.content_height(ui.scale, ty)
    }

    fn selection(&self) -> std::ops::Range<usize> {
        self.caret.min(self.anchor)..self.caret.max(self.anchor)
    }

    fn replace_selection(&mut self, with: &str) {
        let r = self.selection();
        self.text.replace_range(r.clone(), with);
        self.caret = r.start + with.len();
        self.anchor = self.caret;
        self.dirty = true;
        self.revision += 1;
    }

    /// Files the text as it stands before a change. A run of typing is one step.
    fn remember(&mut self, typing: bool) {
        if self.obscured || (typing && self.typing) {
            self.typing = typing;
            return;
        }
        self.typing = typing;
        self.redo.clear();
        self.undo.push(Snapshot { text: self.text.clone(), caret: self.caret, anchor: self.anchor });
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }

    /// Steps through the history. Answers whether anything changed.
    fn step_history(&mut self, back: bool) -> bool {
        let (from, to) = if back { (&mut self.undo, &mut self.redo) } else { (&mut self.redo, &mut self.undo) };
        let Some(step) = from.pop() else { return false };
        to.push(Snapshot { text: std::mem::replace(&mut self.text, step.text), caret: self.caret, anchor: self.anchor });
        self.caret = step.caret.min(self.text.len());
        self.anchor = step.anchor.min(self.text.len());
        self.typing = false;
        self.dirty = true;
        self.revision += 1;
        true
    }

    fn atom_around(&self, at: usize) -> Option<Range<usize>> {
        self.atoms.iter().find(|atom| atom.start < at && at < atom.end).cloned()
    }

    /// Widens the selection so neither end stops partway through an atom.
    fn select_whole_atoms(&mut self) {
        let sel = self.selection();
        let from = self.atom_around(sel.start).map_or(sel.start, |atom| atom.start);
        let to = self.atom_around(sel.end).map_or(sel.end, |atom| atom.end);
        if self.caret < self.anchor {
            self.caret = from;
            self.anchor = to;
        } else {
            self.anchor = from;
            self.caret = to;
        }
    }

    /// A caret never rests inside an atom. One a key moved in carries on through in the direction it came from; one a
    /// click put there goes to the nearer edge.
    fn snap_to_atoms(&mut self, came_from: Option<usize>) {
        if !self.selection().is_empty() {
            return self.select_whole_atoms();
        }
        let Some(atom) = self.atom_around(self.caret) else { return };
        let to = match came_from {
            Some(from) if from <= atom.start => atom.end,
            Some(from) if from >= atom.end => atom.start,
            _ if self.caret - atom.start < atom.end - self.caret => atom.start,
            _ => atom.end,
        };
        self.caret = to;
        self.anchor = to;
    }

    fn refresh_atoms(&mut self, hooks: &mut dyn EditHooks) {
        self.atoms = hooks.atoms(&self.text);
    }

    /// Height the text wants at the current width, in logical pixels.
    pub fn content_height(&self, scale: f32, ty: &TypeStyle) -> f32 {
        (self.doc.height() / scale).max(ty.line)
    }

    fn relayout(&mut self, ui: &mut Ui<'_>, ty: &TypeStyle, width: f32) {
        let w = if self.single_line { NO_WRAP } else { width * ui.scale };
        if !self.dirty && (self.last_w - w).abs() < 0.5 && self.doc.generation != 0 {
            return;
        }
        self.dirty = false;
        self.last_w = w;
        let color = self.ink.unwrap_or(ui.theme.text_primary);
        self.doc = Doc::new(w);
        if self.obscured {
            let bullets = BULLET.repeat(self.text.chars().count());
            self.doc.push(ui.text, &bullets, *ty, color);
        } else if self.preedit.is_empty() {
            if self.single_line && self.text.contains(['\n', '\r']) {
                self.doc.push(ui.text, &one_line(&self.text), *ty, color);
            } else {
                self.doc.push(ui.text, &self.text, *ty, color);
            }
        } else {
            let at = self.caret.min(self.text.len());
            if self.single_line {
                self.doc.push(ui.text, &one_line(&self.text[..at]), *ty, color);
                self.doc.push(ui.text, &one_line(&self.preedit), *ty, ui.theme.accent_text);
                self.doc.push(ui.text, &one_line(&self.text[at..]), *ty, color);
            } else {
                self.doc.push(ui.text, &self.text[..at], *ty, color);
                self.doc.push(ui.text, &self.preedit, *ty, ui.theme.accent_text);
                self.doc.push(ui.text, &self.text[at..], *ty, color);
            }
        }
    }

    /// Draws the field and handles its input. `submit_on_enter`: Enter submits, and Shift+Enter breaks the line.
    pub fn show(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect, placeholder: &str, ty: &TypeStyle, submit_on_enter: bool) -> EditResponse {
        self.show_inner(ui, id, r, placeholder, ty, submit_on_enter, true)
    }

    /// The same field with no box of its own, for a surface that draws its own: `r` is the text area itself.
    pub fn show_bare(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect, placeholder: &str, ty: &TypeStyle, submit_on_enter: bool) -> EditResponse {
        self.show_inner(ui, id, r, placeholder, ty, submit_on_enter, false)
    }

    /// `show_bare` with an owner that hears keys, copies and pastes first, and can make ranges of the text atomic.
    #[allow(clippy::too_many_arguments)]
    pub fn show_with(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect, placeholder: &str, ty: &TypeStyle, submit_on_enter: bool, hooks: &mut dyn EditHooks) -> EditResponse {
        self.show_hooked(ui, id, r, placeholder, ty, submit_on_enter, false, hooks)
    }

    #[allow(clippy::too_many_arguments)]
    fn show_inner(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect, placeholder: &str, ty: &TypeStyle, submit_on_enter: bool, chrome: bool) -> EditResponse {
        self.show_hooked(ui, id, r, placeholder, ty, submit_on_enter, chrome, &mut NoHooks)
    }

    #[allow(clippy::too_many_arguments)]
    fn show_hooked(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect, placeholder: &str, ty: &TypeStyle, submit_on_enter: bool, chrome: bool, hooks: &mut dyn EditHooks) -> EditResponse {
        let mut out = EditResponse::default();
        let hovered = !self.read_only && ui.pointer_in(&r);
        if ui.input.pressed {
            if hovered {
                ui.mem.focus = id;
            } else if ui.mem.focus == id && !ui.mem.press_keeps_focus {
                ui.mem.focus = 0;
            }
        }
        if self.read_only && ui.mem.focus == id {
            ui.mem.focus = 0;
        }
        let focused = ui.mem.focus == id;
        ui.mem.text_input_focused |= focused && !self.read_only;
        // However focus got here, the caret starts lit, or it could spend its first half second dark.
        if focused && !self.had_focus {
            self.blink_start = ui.time;
        }
        self.had_focus = focused;
        if crate::el::a11y_active() {
            self.take_a11y_edits(id, &mut out);
        }
        self.refresh_atoms(hooks);
        if hovered {
            ui.cursor = CursorKind::Text;
        }

        let inner = if chrome { rect(r.x + 11.0, r.y + 6.0, r.w - 22.0, r.h - 12.0) } else { r };
        // A single line sits in the middle of whatever box it was given, and only its sides clip it.
        let (inner, clip) = if self.single_line { (rect(inner.x, (r.y + (r.h - ty.line) * 0.5).round(), inner.w, ty.line), rect(inner.x, r.y, inner.w, r.h)) } else { (inner, inner) };
        let t = ui.theme;

        if chrome {
            // Fluent's input: a filled control whose bottom edge turns into an accent line on focus.
            let hover_t = ui.animate(id, if hovered { 1.0 } else { 0.0 }, motion::FASTER);
            let fill = if focused { if t.dark { Color::argb(0x4D000000) } else { Color::WHITE } } else { t.control_fill.mix(t.control_fill_hover, hover_t) };
            ui.control(r, CONTROL_RADIUS, fill, t.control_stroke, if focused { Color::TRANSPARENT } else { t.control_strong_stroke.fade(0.6) });
            if focused {
                ui.fill_radii(rect(r.x + 1.0, r.bottom() - 2.0, r.w - 2.0, 2.0), [0.0, 0.0, CONTROL_RADIUS, CONTROL_RADIUS], t.accent);
            }
        }

        if focused {
            self.handle_composition(ui, &mut out, hooks);
            if !self.is_composing() {
                self.handle_keys(ui, &mut out, submit_on_enter, hooks);
            }
        } else if self.is_composing() {
            // Focus left mid-composition: nothing was committed, so nothing of it stays on screen.
            self.preedit.clear();
            self.dirty = true;
        }
        self.menu_keys.clear();
        self.relayout(ui, ty, inner.w);
        self.drawn = (inner, ui.scale);
        if !self.obscured
            && let Some(spelling) = &mut self.spelling {
                spelling.refresh(&self.text, self.revision, self.caret, !self.preedit.is_empty(), focused, &self.atoms);
                if spelling.waiting() {
                    ui.wake_after(SPELLING_POLL);
                }
            }

        // Pointer selection.
        let single_line = self.single_line;
        let to_doc = |ui: &Ui<'_>, scroll_x: f32, scroll: f32, p: (f32, f32)| ((p.0 - inner.x + scroll_x) * ui.scale, if single_line { 0.0 } else { (p.1 - inner.y + scroll) * ui.scale });
        if let Some(p) = ui.input.pointer {
            if ui.input.pressed && hovered {
                let (x, y) = to_doc(ui, self.scroll_x, self.scroll, p);
                self.caret = self.from_doc(self.doc.hit(x, y));
                self.anchor = self.caret;
                self.drag = true;
                self.blink_start = ui.time;
                self.typing = false;
                self.clicks = if ui.input.double_click { self.clicks.saturating_add(1) } else { 1 };
                // A second press takes the word and a third the line; dragging after that would only undo them.
                if !self.obscured && self.clicks >= 2 {
                    let (from, to) = if self.clicks == 2 { self.word_around(self.caret) } else { self.line_around(self.caret) };
                    self.anchor = from;
                    self.caret = to;
                    self.drag = false;
                }
                // What is being composed is laid out at the caret.
                self.dirty |= self.is_composing();
            } else if self.drag && ui.input.down {
                let (x, y) = to_doc(ui, self.scroll_x, self.scroll, p);
                self.caret = self.from_doc(self.doc.hit(x, y));
            }
        }
        if ui.input.released {
            if self.drag {
                self.snap_to_atoms(None);
            }
            self.drag = false;
        }

        // The wheel moves the text. The caret is brought back into view only when it or the text changes, or the wheel
        // would have nowhere to go.
        let max_scroll = if self.single_line { 0.0 } else { (self.doc.height() / ui.scale - inner.h).max(0.0) };
        if hovered && ui.input.scroll != 0.0 && max_scroll > 0.0 {
            self.scroll -= ui.input.scroll;
        }

        // Keep the caret in view. An empty field has no line to measure, and its caret is as tall as a line would be.
        let (cx, cy, ch) = self.doc.caret_pos(self.preedit_caret());
        let (cx, cy, ch) = (cx / ui.scale, cy / ui.scale, if ch > 0.0 { ch / ui.scale } else { ty.line });
        let reveal_state = (self.revision, self.caret, self.preedit.len(), (if self.single_line { inner.w } else { inner.h }).to_bits());
        if focused && reveal_state != self.revealed {
            self.revealed = reveal_state;
            if self.single_line {
                // The caret's own pixel has to stay inside the clip at the far end.
                if cx - self.scroll_x > inner.w - 1.0 {
                    self.scroll_x = cx - inner.w + 1.0;
                } else if cx < self.scroll_x {
                    self.scroll_x = cx;
                }
            } else if cy - self.scroll + ch > inner.h {
                self.scroll = cy + ch - inner.h;
            } else if cy < self.scroll {
                self.scroll = cy;
            }
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        if self.single_line {
            let (end_x, _, _) = self.doc.caret_pos(usize::MAX);
            let max_x = (end_x / ui.scale + 1.0 - inner.w).max(0.0);
            if hovered && (ui.input.scroll_x != 0.0 || ui.input.scroll != 0.0) {
                self.scroll_x -= if ui.input.scroll_x != 0.0 { ui.input.scroll_x } else { ui.input.scroll };
            }
            // Out of focus an `<input>` shows where its text starts.
            self.scroll_x = if focused { self.scroll_x.clamp(0.0, max_x) } else { 0.0 };
            // Scrolled to before its start, which every point on the text already goes through.
            if self.align_end {
                let room = inner.w - 1.0 - end_x / ui.scale;
                if room > 0.0 {
                    self.scroll_x = -room;
                }
            }
        }

        let (scroll, scroll_x) = (self.scroll, self.scroll_x);
        ui.with_clip(clip, |ui| {
            let scale = ui.scale;
            if self.text.is_empty() && self.preedit.is_empty() {
                ui.label(inner.x, inner.y, placeholder, ty, t.text_tertiary);
            }
            if let Some(fill) = self.atom_fill {
                let mut rects = Vec::new();
                for atom in &self.atoms {
                    if atom.end <= self.text.len() && self.text.is_char_boundary(atom.start) && self.text.is_char_boundary(atom.end) {
                        self.doc.selection_rects(self.to_doc(atom.start)..self.to_doc(atom.end), 0.0..f32::MAX, &mut rects);
                    }
                }
                for [x, y, w, h] in rects {
                    ui.fill(rect(inner.x - scroll_x + x / scale, inner.y - scroll + y / scale, w / scale, h / scale), 4.0, fill);
                }
            }
            let sel = self.selection();
            if !sel.is_empty() {
                let mut rects = Vec::new();
                self.doc.selection_rects(self.to_doc(sel.start)..self.to_doc(sel.end), 0.0..f32::MAX, &mut rects);
                for [x, y, w, h] in rects {
                    ui.fill(rect(inner.x - scroll_x + x / scale, inner.y - scroll + y / scale, w / scale, h / scale), 2.0, t.selection);
                }
            }
            if !self.obscured && !self.misspelt().is_empty() {
                let mut rects = Vec::new();
                for word in self.misspelt() {
                    self.doc.selection_rects(self.to_doc(word.start)..self.to_doc(word.end), scroll * scale..(scroll + inner.h) * scale, &mut rects);
                }
                for [x, y, w, h] in rects {
                    squiggle(ui, inner.x - scroll_x + x / scale, inner.y - scroll + (y + h) / scale - SQUIGGLE_LIFT, w / scale, t.critical);
                }
            }
            ui.doc(&mut self.doc, inner.x - scroll_x, inner.y - scroll);
            if focused {
                // Blinking schedules exactly the frames that change the caret, and an idle field does no work.
                let elapsed = (ui.time - self.blink_start).max(0.0);
                let visible = ((elapsed / 0.53) as u32).is_multiple_of(2);
                if visible {
                    ui.fill(rect(inner.x - scroll_x + cx, inner.y - scroll + cy + 2.0, 1.0, ch - 4.0), 0.0, t.text_primary);
                }
                ui.wake_after(0.53 - elapsed % 0.53);
                ui.ime_area = Some(rect(inner.x - scroll_x + cx, inner.y - scroll + cy, 2.0, ch));
            }
            // A thin mark of where the view is, once there is more text than room.
            if max_scroll > 0.0 {
                let total = self.doc.height() / scale;
                let thumb = (inner.h * inner.h / total).max(16.0);
                let y = inner.y + (inner.h - thumb) * (scroll / max_scroll);
                ui.fill(rect(inner.right() - 2.0, y, 2.0, thumb), 1.0, t.text_tertiary.fade(0.5));
            }
        });
        if crate::el::a11y_active() {
            let mut input = self.a11y_input(id, placeholder, !self.single_line, self.obscured);
            input.rect = Some(r);
            input.lines = self.a11y_lines(inner, ui.scale);
            crate::el::a11y_note_text_input(input);
        }
        out
    }

    /// The field as a screen reader gets it. The obscured and multiline flags are the caller's, since a field that has not painted
    /// yet has not been told them, and a password must not go out in the clear for that one frame.
    pub(crate) fn a11y_input(&self, id: u64, placeholder: &str, multiline: bool, obscured: bool) -> crate::el::TextInput {
        let chars = |bytes: usize| self.text.get(..bytes).map_or(0, |head| head.chars().count());
        let value = if obscured { BULLET.repeat(self.text.chars().count()) } else { self.text.clone() };
        crate::el::TextInput { edit: id, value, caret: chars(self.caret), anchor: chars(self.anchor), multiline, obscured, read_only: self.read_only, placeholder: placeholder.into(), ..Default::default() }
    }

    /// Where each line of the text is, for as many lines as make this cheap: finding one is a walk over the laid-out lines.
    fn a11y_lines(&self, inner: Rect, scale: f32) -> Vec<Rect> {
        const MOST: usize = 64;
        if self.single_line || self.obscured || !self.preedit.is_empty() || self.text.is_empty() {
            return Vec::new();
        }
        let mut lines = Vec::new();
        let mut start = 0;
        for line in self.text.split_inclusive('\n').take(MOST) {
            let end = start + line.trim_end_matches('\n').len();
            let (_, top, _) = self.doc.caret_pos(start);
            let (_, last, h) = self.doc.caret_pos(end);
            lines.push(rect(inner.x, inner.y - self.scroll + top / scale, inner.w, (last + h - top) / scale));
            start += line.len();
        }
        lines
    }

    /// What a screen reader asked of this field since the last frame: the edits typing makes, so whoever owns it hears of them the same way.
    fn take_a11y_edits(&mut self, id: u64, out: &mut EditResponse) {
        for edit in crate::el::a11y_take_edits(id) {
            match edit {
                _ if self.read_only => {}
                crate::el::A11yEdit::SetValue(text) => {
                    let text = self.admissible(&text).into_owned();
                    self.select_all();
                    self.insert(&text);
                    out.changed = true;
                }
                crate::el::A11yEdit::ReplaceSelected(text) => {
                    let text = self.admissible(&text).into_owned();
                    self.insert(&text);
                    out.changed = true;
                }
                crate::el::A11yEdit::Select { anchor, focus } => {
                    let byte = |chars: usize| self.text.char_indices().nth(chars).map_or(self.text.len(), |(at, _)| at);
                    (self.anchor, self.caret) = (byte(anchor), byte(focus));
                    self.snap_to_atoms(None);
                    self.typing = false;
                }
            }
        }
    }

    /// Where the caret is drawn: after what is being composed.
    fn preedit_caret(&self) -> usize {
        if self.obscured { self.to_doc(self.caret) } else { self.caret + self.preedit.len() }
    }

    /// The word the caret is in, or the run of spaces or punctuation when it is not in one.
    fn word_around(&self, at: usize) -> (usize, usize) {
        let class = |c: char| if c.is_alphanumeric() || c == '_' { if c.is_ascii() { 0 } else { 1 } } else if c.is_whitespace() { 2 } else { 3 };
        let head = &self.text[..at];
        let tail = &self.text[at..];
        // At the end of a word a double click still means that word.
        let Some(here) = tail.chars().next().filter(|c| *c != '\n').or_else(|| head.chars().next_back()).map(class) else { return (at, at) };
        let from = head.char_indices().rev().take_while(|(_, c)| *c != '\n' && class(*c) == here).last().map_or(at, |(i, _)| i);
        let to = at + tail.chars().take_while(|c| *c != '\n' && class(*c) == here).map(char::len_utf8).sum::<usize>();
        (from, to)
    }

    fn line_around(&self, at: usize) -> (usize, usize) {
        (self.text[..at].rfind('\n').map_or(0, |i| i + 1), self.text[at..].find('\n').map_or(self.text.len(), |i| at + i))
    }

    /// The input method's half: what it commits becomes text, once, and what it is composing is only shown.
    fn handle_composition(&mut self, ui: &mut Ui<'_>, out: &mut EditResponse, hooks: &mut dyn EditHooks) {
        if let Some(commit) = &ui.input.ime_commit {
            self.preedit.clear();
            self.dirty = true;
            self.remember(false);
            let commit = self.admissible(commit).into_owned();
            self.replace_selection(&commit);
            self.refresh_atoms(hooks);
            out.changed = true;
        }
        let composing = ui.input.ime_preedit.as_deref().unwrap_or("");
        if composing != self.preedit {
            // Composition replaces a selection the way typing does, and it does so as it starts, so what is shown is
            // what committing would leave.
            if self.preedit.is_empty() && !self.selection().is_empty() {
                self.remember(false);
                self.replace_selection("");
                self.refresh_atoms(hooks);
                out.changed = true;
            }
            self.preedit = composing.to_owned();
            self.dirty = true;
            self.blink_start = ui.time;
        }
    }

    fn handle_keys(&mut self, ui: &mut Ui<'_>, out: &mut EditResponse, submit_on_enter: bool, hooks: &mut dyn EditHooks) {
        if !ui.input.text.is_empty() {
            let typed: String = ui.input.text.chars().filter(|c| !c.is_control()).collect();
            if !typed.is_empty() {
                self.remember(true);
                self.replace_selection(&typed);
                self.refresh_atoms(hooks);
                self.blink_start = ui.time;
                out.changed = true;
            }
        }
        let from_menu = std::mem::take(&mut self.menu_keys);
        for press in ui.input.keys.iter().chain(&from_menu) {
            self.blink_start = ui.time;
            // The key that confirms a composition arrives as the composition ends. It belongs to the input method, or a
            // confirmed word would send the message or break the line.
            if press.key == Key::Enter && ui.input.ime_commit.is_some() {
                continue;
            }
            let revision = self.revision;
            if hooks.key(self, press) {
                if self.revision != revision {
                    self.typing = false;
                    self.refresh_atoms(hooks);
                    out.changed = true;
                }
                continue;
            }
            let moved = |edit: &mut Self, to: usize, shift: bool| {
                edit.caret = to;
                if !shift {
                    edit.anchor = to;
                }
            };
            match press.key {
                Key::Enter => {
                    if submit_on_enter && (!press.shift || self.single_line) {
                        out.submitted = true;
                    } else if !self.single_line {
                        self.remember(false);
                        self.replace_selection("\n");
                        self.refresh_atoms(hooks);
                        out.changed = true;
                    }
                }
                Key::Backspace => {
                    if self.selection().is_empty() && self.caret > 0 {
                        self.anchor = if press.cmd && COMMAND_MEANS_LINE {
                            self.text[..self.caret].rfind('\n').map_or(0, |i| i + 1)
                        } else if press.alt || press.cmd {
                            prev_word(&self.text, self.caret)
                        } else {
                            prev_char(&self.text, self.caret)
                        };
                        if let Some(atom) = self.atoms.iter().find(|atom| atom.start < self.caret && self.caret <= atom.end) {
                            self.anchor = self.anchor.min(atom.start);
                        }
                    }
                    if !self.selection().is_empty() {
                        self.select_whole_atoms();
                        self.remember(false);
                        self.replace_selection("");
                        self.refresh_atoms(hooks);
                        out.changed = true;
                    }
                }
                Key::Delete => {
                    if self.selection().is_empty() && self.caret < self.text.len() {
                        self.anchor = if press.cmd || press.alt { next_word(&self.text, self.caret) } else { next_char(&self.text, self.caret) };
                        if let Some(atom) = self.atoms.iter().find(|atom| atom.start <= self.caret && self.caret < atom.end) {
                            self.anchor = self.anchor.max(atom.end);
                        }
                    }
                    if !self.selection().is_empty() {
                        self.select_whole_atoms();
                        self.remember(false);
                        self.replace_selection("");
                        self.refresh_atoms(hooks);
                        out.changed = true;
                    }
                }
                Key::Left => {
                    self.typing = false;
                    let sel = self.selection();
                    let plain = !press.cmd && !press.alt;
                    let to = if !press.shift && !sel.is_empty() {
                        sel.start
                    } else if press.cmd && COMMAND_MEANS_LINE {
                        self.text[..self.caret].rfind('\n').map_or(0, |i| i + 1)
                    } else if plain {
                        // Over an atom in one step.
                        self.atoms.iter().find(|atom| atom.start < self.caret && self.caret <= atom.end).map_or_else(|| prev_char(&self.text, self.caret), |atom| atom.start)
                    } else {
                        prev_word(&self.text, self.caret)
                    };
                    let from = self.caret;
                    moved(self, to, press.shift);
                    self.snap_to_atoms(Some(from));
                }
                Key::Right => {
                    self.typing = false;
                    let sel = self.selection();
                    let plain = !press.cmd && !press.alt;
                    let to = if !press.shift && !sel.is_empty() {
                        sel.end
                    } else if press.cmd && COMMAND_MEANS_LINE {
                        self.text[self.caret..].find('\n').map_or(self.text.len(), |i| self.caret + i)
                    } else if plain {
                        self.atoms.iter().find(|atom| atom.start <= self.caret && self.caret < atom.end).map_or_else(|| next_char(&self.text, self.caret), |atom| atom.end)
                    } else {
                        next_word(&self.text, self.caret)
                    };
                    let from = self.caret;
                    moved(self, to, press.shift);
                    self.snap_to_atoms(Some(from));
                }
                Key::Up | Key::Down => {
                    self.typing = false;
                    let (x, y, h) = self.doc.caret_pos(self.to_doc(self.caret));
                    let ny = if press.key == Key::Up { y - h * 0.5 } else { y + h * 1.5 };
                    // With nowhere to go, the keys go to the ends of the line, as on a Mac.
                    let to = match (self.single_line || (press.cmd && COMMAND_MEANS_LINE), press.key == Key::Up) {
                        (true, true) => 0,
                        (true, false) => self.text.len(),
                        _ if ny < 0.0 => 0,
                        _ => self.from_doc(self.doc.hit(x, ny)),
                    };
                    let from = self.caret;
                    moved(self, to, press.shift);
                    self.snap_to_atoms(Some(from));
                }
                Key::Home => {
                    self.typing = false;
                    let to = self.text[..self.caret].rfind('\n').map_or(0, |i| i + 1);
                    let from = self.caret;
                    moved(self, to, press.shift);
                    self.snap_to_atoms(Some(from));
                }
                Key::End => {
                    self.typing = false;
                    let to = self.text[self.caret..].find('\n').map_or(self.text.len(), |i| self.caret + i);
                    let from = self.caret;
                    moved(self, to, press.shift);
                    self.snap_to_atoms(Some(from));
                }
                Key::Char(c) if press.cmd => match c.to_ascii_lowercase() {
                    'a' => {
                        self.anchor = 0;
                        self.caret = self.text.len();
                        self.typing = false;
                    }
                    'c' | 'x' => {
                        let sel = self.selection();
                        if !sel.is_empty() && !self.obscured {
                            let copied = hooks.copied(self, sel.clone()).unwrap_or_else(|| self.text[sel].to_owned());
                            ui.clipboard.set(&copied);
                            if c.eq_ignore_ascii_case(&'x') {
                                self.remember(false);
                                self.replace_selection("");
                                self.refresh_atoms(hooks);
                                out.changed = true;
                            }
                        }
                    }
                    'v' => {
                        let pasted = ui.clipboard.get();
                        let revision = self.revision;
                        if hooks.paste(self, pasted.clone()) {
                            if self.revision != revision {
                                self.typing = false;
                                self.refresh_atoms(hooks);
                                out.changed = true;
                            }
                        } else if let Some(pasted) = pasted {
                            self.remember(false);
                            let pasted = self.admissible(&pasted).into_owned();
                            self.replace_selection(&pasted);
                            self.refresh_atoms(hooks);
                            out.changed = true;
                        }
                    }
                    'z' => {
                        if self.step_history(!press.shift) {
                            self.refresh_atoms(hooks);
                            out.changed = true;
                        }
                    }
                    'y' => {
                        if self.step_history(false) {
                            self.refresh_atoms(hooks);
                            out.changed = true;
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

impl Default for TextEdit {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_motion_handles_cjk_and_edges() {
        let s = "hello  wide 世界";
        assert_eq!(prev_word(s, s.len()), 12);
        assert_eq!(prev_word(s, 11), 7);
        assert_eq!(prev_word(s, 5), 0);
        assert_eq!(next_word(s, 0), 5);
        assert_eq!(next_word(s, 5), 11);
        assert_eq!(next_word(s, s.len()), s.len());
        assert_eq!(prev_char(s, 15), 12);
        assert_eq!(next_char(s, 12), 15);
    }
}
