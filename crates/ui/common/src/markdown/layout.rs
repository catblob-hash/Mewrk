//! Blocks into pieces: positioned documents and the boxes around them, for one width. Sizes are the ones in
//! `styles/conversation.css`'s `.markdown-content` and `components/CodeBlock.css`.
//!
//! A piece's shaped text only depends on what it says, how it is styled and how wide it may be, so when a block is laid out
//! again (its source grew; the window changed) anything unchanged is taken back from a pool rather than shaped again. That is
//! what makes a growing list or a growing code block cost its last item, not all of it.

use super::math_layout::{typeset, Formula, Typeset};
use super::parse::{Block, Float, Inline, InlineStyle, Link, ListItem, TableCell, TableRow};
use super::{ImageResolver, MarkdownImage};
use fluent::el::TextAlign;
use fluent::text::{Band, BlockKind, Doc, FxHasher, TextCtx};
use fluent::theme::{Color, Theme, TypeStyle, Weight};
use fluent::ui::{rect, Rect};
use mewrk_panes::highlight::{self, Lang, State, Token};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

pub(super) type UiKey = (u32, u32);

/// What the reader has done to a code block, table or disclosure. Outlives its layouts, and a stream's next parse.
#[derive(Clone, Copy, Default)]
pub(super) struct BlockUi {
    /// The pointer is on it: a block `:root[data-code-collapse]` holds to `22em` lets go of that, as `pre:hover` does.
    pub hover: bool,
    pub open: Option<bool>,
    pub scroll_x: f32,
    pub copied_until: f32,
}

/// Wider than any line, in physical pixels: text laid out this wide never wraps.
const NO_WRAP: f32 = 1.0e7;
/// Code is shaped in runs of this many lines, so a block that keeps growing only shapes its last run again.
const CHUNK_LINES: usize = 256;
/// `MAX_HIGHLIGHTED_LINES`: past this code is shown uncoloured.
const MAX_HIGHLIGHTED_LINES: usize = 8000;
const NBSP: &str = "\u{a0}";
const TAB_STOP: usize = 8;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum SpanKind {
    Code,
    Kbd,
    Mark,
    Line,
    /// `underline dotted`, under a formula that would not parse.
    Dotted,
}

/// Something drawn with a run of text, relative to its piece.
#[derive(Clone, Debug)]
pub(super) struct SpanRect {
    pub r: [f32; 4],
    pub kind: SpanKind,
    pub color: Color,
}

pub(super) struct LinkSpan {
    pub link: Rc<Link>,
    /// One box per line the link is on, relative to its piece.
    pub rects: Vec<[f32; 4]>,
    /// How far under a line's top its underline goes.
    pub under: f32,
    pub color: Color,
}

/// What a copy writes instead of what the document holds: nothing for inline code's padding, the source for a formula, a
/// tab for the spaces that draw it.
pub(super) struct CopyEdit {
    pub range: Range<u32>,
    pub with: Arc<str>,
}

/// A run moved off the baseline after painting: `<sup>`, `<sub>`.
pub(super) struct Shift {
    pub r: [f32; 4],
    pub dy: f32,
}

/// A base and its reading over it, drawn in the room its document holds for them.
pub(super) struct InlineRuby {
    /// Which of the document's `boxes` it is.
    pub at: usize,
    pub base: String,
    pub base_ty: TypeStyle,
    pub reading: String,
    pub reading_ty: TypeStyle,
    pub color: Color,
    /// How wide the box is, which the narrower of the two is centred in.
    pub full: f32,
}

/// A typeset formula, drawn in the room its document holds for it.
pub(super) struct InlineFormula {
    /// Which of the document's `boxes` it is.
    pub at: usize,
    pub formula: Rc<Typeset>,
    pub color: Color,
    /// How wide the box holding it is: what a tag or a `multline` goes by. Its text size goes with that.
    pub full: f32,
    pub size: f32,
}

/// Why a formula did not parse.
pub(super) struct Note {
    pub message: Arc<str>,
}

/// A run of shaped text and everything that goes with it. Only depends on what makes up its key.
pub(super) struct TextLay {
    pub doc: Doc,
    pub spans: Vec<SpanRect>,
    pub links: Vec<LinkSpan>,
    pub copy: Vec<CopyEdit>,
    pub shifts: Vec<Shift>,
    pub formulas: Vec<InlineFormula>,
    pub rubies: Vec<InlineRuby>,
    pub notes: Vec<Note>,
    pub natural_w: f32,
    pub h: f32,
    /// What a code run left open, for the run after it.
    pub end_state: State,
}

/// Between a piece and the one before it, in a copy.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Sep {
    Newline,
    Tab,
}

pub(super) struct Piece {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub lay: TextLay,
    pub key: u64,
    pub align: TextAlign,
    pub sep: Sep,
    /// The scrolling box it lies in.
    pub region: Option<u16>,
    /// What a triple click selects: one piece, or all the runs of one code block.
    pub group: u32,
    /// Takes part in the streaming reveal. Math does not, as upstream's renderer leaves it out. Code does here, though upstream's
    /// leaves it out too: with one commit a second, code that arrived whole would land up to a second ahead of the text before it, and
    /// a long block would grow a second's lines at a time.
    pub fade: bool,
}

impl Piece {
    pub fn h(&self) -> f32 {
        self.lay.h
    }

    /// How far a line of this width moves for the piece's alignment.
    pub fn dx(&self, line_w: f32) -> f32 {
        match self.align {
            TextAlign::Start => 0.0,
            TextAlign::Center => ((self.w - line_w) * 0.5).max(0.0),
            TextAlign::End => (self.w - line_w).max(0.0),
        }
    }

    pub fn line_dx_at(&self, y: f32, scale: f32) -> f32 {
        if self.align == TextAlign::Start {
            return 0.0;
        }
        let phys = y * scale;
        let lines = &self.lay.doc.lines;
        let at = lines.partition_point(|l| l.y + l.h <= phys).min(lines.len().saturating_sub(1));
        lines.get(at).map_or(0.0, |l| self.dx(l.width / scale))
    }

    pub fn hit(&self, x: f32, y: f32, scale: f32) -> usize {
        let dx = self.line_dx_at(y, scale);
        self.lay.doc.hit((x - dx) * scale, y * scale)
    }
}

/// A box drawn behind the text.
#[derive(Clone, Copy)]
pub(super) struct Deco {
    pub r: Rect,
    pub radius: f32,
    pub fill: Color,
    pub stroke: Option<Color>,
    pub region: Option<u16>,
}

#[derive(Clone)]
pub(super) enum MarkerKind {
    Disc,
    Circle,
    Square,
    Number(Arc<str>),
    Check(bool),
    Disclosure(bool),
}

#[derive(Clone)]
pub(super) struct Marker {
    pub x: f32,
    pub y: f32,
    pub kind: MarkerKind,
    pub color: Color,
    pub ty: TypeStyle,
    pub region: Option<u16>,
}

pub(super) struct ImageBox {
    pub src: String,
    pub r: Rect,
    pub image: MarkdownImage,
    pub link: Option<Rc<Link>>,
    pub region: Option<u16>,
}

/// A fenced block's corner of controls.
pub(super) struct Chrome {
    pub label: Option<Arc<str>>,
    pub code: Arc<str>,
    /// Taller than `22em`, so that the appearance setting has something to hold back.
    pub can_collapse: bool,
}

/// A box that scrolls sideways inside the leaf: code that does not wrap, a table wider than the column, a long formula.
pub(super) struct Region {
    pub key: UiKey,
    pub r: Rect,
    /// What is shown of the inside.
    pub clip: Rect,
    pub content_w: f32,
    pub chrome: Option<Chrome>,
    /// The box it lies in, a table for a code block in one of its cells, which moves it along. Always earlier in the list.
    pub parent: Option<u16>,
}

impl Region {
    pub fn max_scroll(&self) -> f32 {
        (self.content_w - self.clip.w).max(0.0)
    }
}

/// A table's rules: `border-collapse`, so one to each edge a cell has. The level ones are by height, so a long table costs what is
/// in view, and what spans several columns or rows has no rule through it.
pub(super) struct Grid {
    pub region: u16,
    /// x, y, width.
    pub level: Vec<[f32; 3]>,
    /// x, y, height.
    pub upright: Vec<[f32; 3]>,
    pub color: Color,
}

pub(super) struct Toggle {
    pub key: UiKey,
    pub r: Rect,
    pub open: bool,
    pub region: Option<u16>,
}

/// A picture out of the text with `<img align>`, margins and all, in the coordinates of the block being laid out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FloatBox {
    pub left: bool,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Where one block or table cell ended up, and what of the layout is its own.
pub(super) struct Outlined {
    pub r: Rect,
    pub region: Option<u16>,
    pub pieces: Range<u32>,
    pub images: Range<u32>,
}

/// One block's layout for one width.
#[derive(Default)]
pub(super) struct Laid {
    pub width: f32,
    pub height: f32,
    pub mt: f32,
    pub mb: f32,
    pub pieces: Vec<Piece>,
    pub max_piece_h: f32,
    pub decos: Vec<Deco>,
    pub max_deco_h: f32,
    pub markers: Vec<Marker>,
    pub images: Vec<ImageBox>,
    /// What each image answered when asked, to notice it answering differently.
    pub image_refs: Vec<(String, Option<MarkdownImage>)>,
    pub regions: Vec<Region>,
    pub grids: Vec<Grid>,
    pub toggles: Vec<Toggle>,
    /// `id`s and names: what a fragment looks for first.
    pub anchors: Vec<(String, f32)>,
    /// Heading text, for what a fragment looks for after that.
    pub headings: Vec<(String, f32)>,
    pub natural_w: f32,
    /// It shows a formula as it stood earlier in a stream, which it may only do while that stream is still writing it.
    pub provisional: bool,
    /// A table cell holds several things one under the other, and the next cell starts back at the top: `pieces` do not go down.
    pub unsorted: bool,
    /// By the address of the block or cell, which lives as long as this does: what a screen reader is told is where.
    pub outline: HashMap<usize, Outlined>,
    /// The floats from blocks above that reach down into this one, as they were when it was laid out.
    pub carried: Vec<FloatBox>,
    /// Those and its own, which is what the block below it has to run around.
    pub floats: Vec<FloatBox>,
}

impl Laid {
    /// Where to start looking for the pieces at this height. `y` is relative to the block.
    pub fn first_piece_near(&self, y: f32) -> usize {
        if self.unsorted { 0 } else { self.pieces.partition_point(|p| p.y + self.max_piece_h <= y) }
    }

    /// The pieces in order have gone past this height, and none after this one can be back.
    pub fn past(&self, piece: &Piece, y: f32) -> bool {
        !self.unsorted && piece.y > y
    }
}

#[derive(Default)]
pub(super) struct Pool {
    entries: HashMap<u64, Vec<TextLay>>,
}

impl Pool {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn put(&mut self, laid: Laid) {
        for piece in laid.pieces {
            self.entries.entry(piece.key).or_default().push(piece.lay);
        }
    }

    fn take(&mut self, key: u64) -> Option<TextLay> {
        self.entries.get_mut(&key).and_then(Vec::pop)
    }
}

thread_local! {
    static SHAPED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many runs of text have been shaped on this thread. What tests count to see what a parse reused.
pub(super) fn shaped_count() -> u64 {
    SHAPED.with(std::cell::Cell::get)
}

pub(super) struct Cx<'a> {
    pub text: &'a mut TextCtx,
    pub scale: f32,
    pub theme: Theme,
    pub base: TypeStyle,
    pub color: Color,
    pub top: u32,
    pub ui: &'a HashMap<UiKey, BlockUi>,
    pub pool: &'a mut Pool,
    pub resolver: Option<ImageResolver>,
    pub wrap_default: bool,
    pub collapse_default: bool,
    /// In a table cell: `width: max-content`, so text stays on its lines and a picture has its margins all round.
    pub in_cell: bool,
    pub out: Laid,
    pub group: u32,
    pub sep: Sep,
    pub region: Option<u16>,
    /// This is the last block of a message still streaming: a formula here may not have all of its text yet.
    pub streaming_tail: bool,
    /// The last drawing of each display formula that parsed, for the moments in a stream when it does not.
    pub held: &'a mut HashMap<UiKey, Held>,
    /// What text in this block has to run around.
    pub floats: Vec<FloatBox>,
}

/// A display formula's last good drawing, and the source it came from.
pub(super) struct Held {
    pub source: String,
    pub typeset: Rc<Typeset>,
}

/// What stands for a formula right now.
enum Shown {
    Drawn(Rc<Typeset>),
    /// `math-formula--source`: quiet while it may yet parse, marked when it will not.
    Source { pending: bool, message: Option<Arc<str>> },
}

#[derive(Clone, Copy)]
struct Env {
    x: f32,
    w: f32,
    color: Color,
    align: TextAlign,
    /// A direct child of a list item, whose `<p>` has its own margin.
    in_li: bool,
    list_depth: u32,
}

/// Text in code style: `.markdown-content code`'s `.9em/1.55`.
fn code_type(base: TypeStyle) -> TypeStyle {
    let size = base.size * 0.9;
    TypeStyle { size, line: size * 1.55, weight: Weight::Regular, mono: true, italic: false, serif: false }
}

fn run_type(base: TypeStyle, style: InlineStyle) -> TypeStyle {
    let mut ty = base;
    if style.code {
        ty.size = base.size * 0.9;
        ty.mono = true;
    }
    if style.kbd {
        ty.size = base.size * 0.86;
        ty.mono = true;
    }
    if style.mono {
        ty.size = base.size * 0.9;
        ty.mono = true;
    }
    if style.sup || style.sub || style.small {
        ty.size *= 0.83;
    }
    if style.bold {
        ty.weight = Weight::Semibold;
    }
    if style.italic {
        ty.italic = true;
    }
    ty
}

fn hash_type(ty: &TypeStyle, h: &mut FxHasher) {
    h.write_u32(ty.size.to_bits());
    h.write_u32(ty.line.to_bits());
    ty.weight.hash(h);
    ty.mono.hash(h);
    ty.italic.hash(h);
}

fn hash_color(c: Color, h: &mut FxHasher) {
    h.write_u32(u32::from_le_bytes([c.r, c.g, c.b, c.a]));
}

fn rects_of(doc: &Doc, range: Range<usize>, scale: f32) -> Vec<[f32; 4]> {
    let mut out = Vec::new();
    doc.selection_rects(range, 0.0..f32::MAX, &mut out);
    for r in &mut out {
        // What a line grew by for a tall formula is not where its text is, and boxes and underlines go with the text.
        let at = doc.lines.partition_point(|l| l.y + l.h <= r[1] + 0.5).min(doc.lines.len().saturating_sub(1));
        if let Some(line) = doc.lines.get(at) {
            r[1] += line.above;
            r[3] -= line.above + line.below;
        }
        *r = r.map(|v| v / scale);
    }
    out
}

/// A half-streamed `\fra` is the start of some command, not an unknown one, and must not flash red on the way to `\frac`.
fn without_partial_command(source: &str) -> &str {
    let letters = source.bytes().rev().take_while(u8::is_ascii_alphabetic).count();
    let before = source.len() - letters;
    if source[..before].ends_with('\\') && source[..before].bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1 {
        return &source[..before - 1];
    }
    source
}

impl Cx<'_> {
    fn shape_inlines(&mut self, runs: &[Inline], width: f32, bands: Vec<Band>, ty: TypeStyle, color: Color) -> TextLay {
        SHAPED.with(|c| c.set(c.get() + 1));
        let scale = self.scale;
        let mut doc = Doc::new(width);
        doc.set_bands(bands);
        doc.begin_block(self.text, BlockKind::Text, 0.0, 0.0, ty);
        let mut copy = Vec::new();
        // Ranges first: where a run ended up is only known once the whole text has been placed.
        let mut marks: Vec<(Range<usize>, SpanKind, Color, f32)> = Vec::new();
        let mut links: Vec<(Range<usize>, Rc<Link>, Color)> = Vec::new();
        let mut shifts: Vec<(Range<usize>, f32)> = Vec::new();
        let mut formulas = Vec::new();
        let mut rubies = Vec::new();
        let mut notes: Vec<(Range<usize>, Arc<str>)> = Vec::new();
        for run in runs {
            match run {
                Inline::Break => doc.push(self.text, "\n", ty, color),
                Inline::Anchor(_) => {}
                Inline::Text { text, style, link } => {
                    self.push_run(&mut doc, text, *style, link.as_ref(), ty, color, &mut copy, &mut marks, &mut links, &mut shifts);
                }
                Inline::Math { source, link, .. } => {
                    let start = doc.text().len();
                    let run_color = if link.is_some() { self.theme.accent_text } else { color };
                    let typeset = typeset(self.text, source, false, ty.size);
                    match &*typeset {
                        Typeset::Ready(formula) => {
                            doc.push_box(self.text, (formula.w * scale).ceil(), (formula.h * scale).ceil(), (formula.d * scale).ceil(), ty);
                            copy.push(CopyEdit { range: start as u32..doc.text().len() as u32, with: Arc::from(format!("${source}$")) });
                            formulas.push(InlineFormula { at: doc.boxes().len() - 1, formula: typeset.clone(), color: run_color, full: formula.w, size: ty.size });
                        }
                        Typeset::Error(message) => {
                            // `.math-formula--source > code`, with nothing a copy would have to leave out.
                            let source_ty = TypeStyle { size: ty.size * 0.92, line: ty.line, weight: Weight::Regular, mono: true, italic: false, serif: false };
                            doc.push(self.text, &collapse_spaces(&source.replace('\n', " ")), source_ty, self.theme.critical);
                            copy.push(CopyEdit { range: start as u32..doc.text().len() as u32, with: Arc::from(source.as_str()) });
                            marks.push((start..doc.text().len(), SpanKind::Dotted, self.theme.critical, ty.line * 0.5 + ty.size * 0.35 + 3.0));
                            notes.push((start..doc.text().len(), message.clone()));
                        }
                    }
                    if let Some(link) = link {
                        links.push((start..doc.text().len(), link.clone(), run_color));
                    }
                }
                Inline::Ruby { base, reading, style, link } => {
                    let start = doc.text().len();
                    let run_color = if link.is_some() { self.theme.accent_text } else { color };
                    let base_ty = run_type(ty, *style);
                    // `rt { font-size: 50% }`, standing on its base. The line makes room for it as it does for a tall formula.
                    let size = base_ty.size * 0.5;
                    let reading_ty = TypeStyle { size, line: (size * 1.3).round(), weight: Weight::Regular, italic: false, ..base_ty };
                    let base_run = self.text.line(base, &base_ty, run_color);
                    let reading_w = self.text.line(reading, &reading_ty, run_color).width;
                    let advance = base_run.advance.max(reading_w).ceil();
                    let above = base_run.baseline() + (reading_ty.line * scale).round();
                    let below = (base_ty.line * scale).round() - base_run.baseline();
                    doc.push_box(self.text, advance, above, below, ty);
                    copy.push(CopyEdit { range: start as u32..doc.text().len() as u32, with: Arc::from(format!("{base}({reading})")) });
                    rubies.push(InlineRuby { at: doc.boxes().len() - 1, base: base.clone(), base_ty, reading: reading.clone(), reading_ty, color: run_color, full: advance / scale });
                    if let Some(link) = link {
                        links.push((start..doc.text().len(), link.clone(), run_color));
                    }
                }
                Inline::Image { alt, link, .. } => {
                    // `.markdown-content__image-missing`: the description stands where the picture would have.
                    let start = doc.text().len();
                    let missing = TypeStyle { italic: true, ..ty };
                    doc.push(self.text, alt, missing, self.theme.text_tertiary);
                    if let Some(link) = link {
                        links.push((start..doc.text().len(), link.clone(), self.theme.text_tertiary));
                    }
                }
            }
        }
        let mut spans = Vec::new();
        for (range, kind, color, y) in marks {
            let rects = rects_of(&doc, range, scale);
            let lines = rects.len();
            // An inline box is as tall as its font and its own padding, inside a taller line: `.08em` for code, `.05em` and
            // its borders for a key, nothing for a mark.
            let box_h = match kind {
                SpanKind::Code => (ty.size * 0.9 * 1.3 + ty.size * 0.16).min(ty.line),
                SpanKind::Kbd => (ty.size * 0.86 * 1.3 + ty.size * 0.1 + 3.0).min(ty.line),
                SpanKind::Mark => (ty.size * 1.3).min(ty.line),
                SpanKind::Line | SpanKind::Dotted => 1.0,
            };
            for r in rects {
                // A padded run that wrapped leaves its padding alone at the end of the line, which is not a box to draw.
                if lines > 1 && matches!(kind, SpanKind::Code | SpanKind::Kbd) && r[2] < ty.size * 0.5 {
                    continue;
                }
                let top = if matches!(kind, SpanKind::Line | SpanKind::Dotted) { r[1] + y } else { r[1] + (r[3] - box_h) * 0.5 };
                spans.push(SpanRect { r: [r[0], top, r[2], box_h], kind, color });
            }
        }
        let under = ty.line * 0.5 + ty.size * 0.35 + 2.0;
        let links = links.into_iter().map(|(range, link, color)| LinkSpan { link, rects: rects_of(&doc, range, scale), under, color }).collect();
        let shifts = shifts.into_iter().flat_map(|(range, dy)| rects_of(&doc, range, scale).into_iter().map(move |r| Shift { r, dy })).collect();
        let notes = notes.into_iter().map(|(_, message)| Note { message }).collect();
        let natural_w = doc.lines.iter().map(|l| l.width).fold(0.0, f32::max) / scale;
        let h = doc.height() / scale;
        TextLay { doc, spans, links, copy, shifts, formulas, rubies, notes, natural_w, h, end_state: State::default() }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_run(&mut self, doc: &mut Doc, text: &str, style: InlineStyle, link: Option<&Rc<Link>>, base: TypeStyle, color: Color, copy: &mut Vec<CopyEdit>, marks: &mut Vec<(Range<usize>, SpanKind, Color, f32)>, links: &mut Vec<(Range<usize>, Rc<Link>, Color)>, shifts: &mut Vec<(Range<usize>, f32)>) {
        if text.is_empty() {
            return;
        }
        let collapsed = collapse_spaces(text);
        let text: &str = &collapsed;
        let t = self.theme;
        let ty = run_type(base, style);
        let boxed = style.code || style.kbd;
        let run_color = match (link, style.code, style.kbd) {
            (Some(_), ..) => t.accent_text,
            (None, true, _) | (None, _, true) => t.text_primary,
            _ => color,
        };
        let start = doc.text().len();
        // `padding: .08em .32em`. A no-break space in the body font is that wide, and a copy leaves it out.
        if boxed {
            doc.push(self.text, NBSP, base, run_color);
        }
        let inner = doc.text().len();
        doc.push(self.text, text, ty, run_color);
        let inner_end = doc.text().len();
        if boxed {
            doc.push(self.text, NBSP, base, run_color);
        }
        let end = doc.text().len();
        if boxed {
            let empty: Arc<str> = Arc::from("");
            copy.push(CopyEdit { range: start as u32..inner as u32, with: empty.clone() });
            copy.push(CopyEdit { range: inner_end as u32..end as u32, with: empty });
        }
        if style.code {
            marks.push((start..end, SpanKind::Code, t.code_bg, 0.0));
        }
        if style.kbd {
            marks.push((start..end, SpanKind::Kbd, t.control_fill, 0.0));
        }
        if style.mark {
            marks.push((start..end, SpanKind::Mark, t.caution_bg, 0.0));
        }
        if style.strike {
            marks.push((start..end, SpanKind::Line, run_color, base.line * 0.5 + base.size * 0.05));
        }
        if style.underline {
            marks.push((start..end, SpanKind::Line, run_color, base.line * 0.5 + base.size * 0.35 + 2.0));
        }
        if style.sup {
            shifts.push((start..end, -base.size * 0.35));
        }
        if style.sub {
            shifts.push((start..end, base.size * 0.2));
        }
        if let Some(link) = link {
            links.push((start..end, link.clone(), run_color));
        }
    }

    /// What the floats take from text at (x, y) in a box `w` wide, top down, in the document's own units.
    fn bands_for(&self, x: f32, y: f32, w: f32) -> Vec<Band> {
        if self.floats.is_empty() {
            return Vec::new();
        }
        let mut edges: Vec<f32> = self.floats.iter().flat_map(|f| [f.y, f.y + f.h]).filter(|edge| *edge > y).collect();
        edges.sort_by(f32::total_cmp);
        edges.dedup();
        let mut bands = Vec::new();
        let mut from = y;
        for until in edges {
            let (mut left, mut right) = (0.0f32, 0.0f32);
            for f in self.floats.iter().filter(|f| f.y < until && f.y + f.h > from) {
                if f.left {
                    left = left.max(f.x + f.w - x);
                } else {
                    right = right.max(x + w - f.x);
                }
            }
            bands.push(Band { until: (until - y) * self.scale, left: left * self.scale, right: right * self.scale });
            from = until;
        }
        while bands.last().is_some_and(|band| band.left <= 0.0 && band.right <= 0.0) {
            bands.pop();
        }
        bands
    }

    /// Text as a piece, from the pool when the same text was shaped the same way before. Beside a float it runs around it, and
    /// starts under it when a word could not fit beside it. Answers how far down it ended.
    #[allow(clippy::too_many_arguments)]
    fn text_piece(&mut self, runs: &[Inline], x: f32, y: f32, w: f32, wrap: bool, ty: TypeStyle, color: Color, align: TextAlign, fade: bool) -> f32 {
        let width = if wrap { (w * self.scale).max(1.0) } else { NO_WRAP };
        let top = y;
        let mut y = y;
        let mut bands = if wrap { self.bands_for(x, y, w) } else { Vec::new() };
        while let Some(band) = bands.first().filter(|band| width - band.left - band.right < ty.size * 4.0 * self.scale) {
            y += band.until / self.scale;
            bands = self.bands_for(x, y, w);
        }
        let mut h = FxHasher::default();
        hash_type(&ty, &mut h);
        hash_color(color, &mut h);
        h.write_u32(width.to_bits());
        for band in &bands {
            h.write_u32(band.until.to_bits());
            h.write_u32(band.left.to_bits());
            h.write_u32(band.right.to_bits());
        }
        for run in runs {
            match run {
                Inline::Break => h.write_u8(0),
                Inline::Anchor(_) => {}
                Inline::Text { text, style, link } => {
                    text.hash(&mut h);
                    style.hash(&mut h);
                    hash_link(link.as_ref(), &mut h);
                }
                Inline::Math { source, style, link } => {
                    h.write_u8(1);
                    source.hash(&mut h);
                    style.hash(&mut h);
                    hash_link(link.as_ref(), &mut h);
                }
                Inline::Image { alt, link, .. } => {
                    h.write_u8(2);
                    alt.hash(&mut h);
                    hash_link(link.as_ref(), &mut h);
                }
                Inline::Ruby { base, reading, style, link } => {
                    h.write_u8(3);
                    base.hash(&mut h);
                    reading.hash(&mut h);
                    style.hash(&mut h);
                    hash_link(link.as_ref(), &mut h);
                }
            }
        }
        let key = h.finish();
        let lay = match self.pool.take(key) {
            Some(lay) => lay,
            None => self.shape_inlines(runs, width, bands, ty, color),
        };
        let height = lay.h;
        self.group += 1;
        let (sep, region, group) = (std::mem::replace(&mut self.sep, Sep::Newline), self.region, self.group);
        self.out.pieces.push(Piece { x, y, w, lay, key, align, sep, region, group, fade });
        y - top + height
    }

    /// Where a floated picture goes: as high as it can from `y`, never above the one before it, against its side or what already
    /// stands there. It moves down past what is in the way when there is no room beside it.
    #[allow(clippy::too_many_arguments)]
    fn place_float(&self, left: bool, w: f32, h: f32, x: f32, y: f32, room: f32) -> (f32, f32) {
        let mut top = self.floats.last().map_or(y, |f| f.y.max(y));
        loop {
            let inside: Vec<&FloatBox> = self.floats.iter().filter(|f| f.y < top + h && f.y + f.h > top).collect();
            let used_left = inside.iter().filter(|f| f.left).map(|f| f.x + f.w - x).fold(0.0f32, f32::max);
            let used_right = inside.iter().filter(|f| !f.left).map(|f| x + room - f.x).fold(0.0f32, f32::max);
            if inside.is_empty() || w <= room - used_left - used_right {
                return (if left { x + used_left } else { x + room - used_right - w }, top);
            }
            top = inside.iter().map(|f| f.y + f.h).fold(f32::MAX, f32::min);
        }
    }

    /// `<img align>`: pictures out of the text, for what follows to run around. Answers the text left. In a cell, whose width comes
    /// from what it holds, a picture stays where it is.
    fn float_pictures(&mut self, inlines: &[Inline], x: f32, y: f32, w: f32) -> Option<Vec<Inline>> {
        if self.in_cell || !inlines.iter().any(|inline| matches!(inline, Inline::Image { float: Some(_), .. })) {
            return None;
        }
        let em = self.base.size;
        let mut rest = Vec::with_capacity(inlines.len());
        for inline in inlines {
            let Inline::Image { src, width, float: Some(side), link, .. } = inline else {
                rest.push(inline.clone());
                continue;
            };
            let Some(image) = self.resolve(src) else {
                rest.push(inline.clone());
                continue;
            };
            let (nw, nh) = (image.size.0 as f32, image.size.1 as f32);
            let shown_w = width.unwrap_or(nw).min(w).max(1.0);
            let shown_h = shown_w * nh / nw.max(1.0);
            // `margin: .7em 0`, without the one `> :first-child` takes away.
            let margin = em * 0.7;
            let above = if self.top == 0 && y <= 0.0 && self.floats.is_empty() { 0.0 } else { margin };
            let left = *side == Float::Left;
            let height = above + shown_h + margin;
            let (at, top) = self.place_float(left, shown_w, height, x, y, w);
            self.floats.push(FloatBox { left, x: at, y: top, w: shown_w, h: height });
            self.out.images.push(ImageBox { src: src.clone(), r: rect(at, top + above, shown_w, shown_h), image, link: link.clone(), region: self.region });
            self.out.natural_w = self.out.natural_w.max(at + shown_w);
        }
        Some(trim_run_spaces(trim_breaks(&rest)))
    }

    /// What a box that keeps its own width gets beside the floats at `y`: `overflow: auto` and the like don't let text run around
    /// anything, so they stand beside a float, or under it when there's no room. Answers its top and its column.
    fn beside_floats(&self, y: f32, env: Env) -> (f32, Env) {
        let mut top = y;
        loop {
            let Some(band) = self.bands_for(env.x, top, env.w).first().copied() else { return (top, env) };
            let (left, right) = (band.left / self.scale, band.right / self.scale);
            if env.w - left - right >= (self.base.size * 8.0).min(env.w) {
                return (top, Env { x: env.x + left, w: env.w - left - right, ..env });
            }
            top += band.until / self.scale;
        }
    }

    fn deco(&mut self, r: Rect, radius: f32, fill: Color, stroke: Option<Color>) {
        let region = self.region;
        self.out.decos.push(Deco { r, radius, fill, stroke, region });
    }

    fn resolve(&mut self, src: &str) -> Option<MarkdownImage> {
        let answer = answer_for(self.resolver.as_ref(), src);
        self.out.image_refs.push((src.to_owned(), answer));
        answer
    }

    /// A paragraph's worth of inlines. A picture that can be drawn is a block of its own (`img { display: block }`) and splits
    /// the text around it; one that cannot stays in the line as its description.
    #[allow(clippy::too_many_arguments)]
    fn lay_inlines(&mut self, inlines: &[Inline], x: f32, y: f32, w: f32, ty: TypeStyle, color: Color, align: TextAlign) -> f32 {
        let em = self.base.size;
        for inline in inlines {
            if let Inline::Anchor(id) = inline {
                self.out.anchors.push((id.clone(), y));
            }
        }
        let unfloated = self.float_pictures(inlines, x, y, w);
        let inlines = unfloated.as_deref().unwrap_or(inlines);
        let has_images = inlines.iter().any(|i| matches!(i, Inline::Image { .. }));
        let wrap = !self.in_cell;
        if inlines.is_empty() {
            return 0.0;
        }
        if !has_images {
            return self.text_piece(inlines, x, y, w, wrap, ty, color, align, true);
        }
        let mut cur = y;
        let mut run_start = 0;
        let mut drew_image = false;
        for (i, inline) in inlines.iter().enumerate() {
            let Inline::Image { src, width, link, .. } = inline else { continue };
            let Some(image) = self.resolve(src) else { continue };
            let run = trim_run_spaces(trim_breaks(&inlines[run_start..i]));
            if !run.is_empty() {
                cur += self.text_piece(&run, x, cur, w, wrap, ty, color, align, true);
            }
            let margin = em * 0.7;
            // The margin only sits between things: `> :first-child` and `> :last-child` take the outer ones away. A cell has no
            // such rule.
            if cur > y || drew_image || self.in_cell {
                cur += margin;
            }
            let (nw, nh) = (image.size.0 as f32, image.size.1 as f32);
            let shown_w = width.unwrap_or(nw).min(w).max(1.0);
            let shown_h = shown_w * nh / nw.max(1.0);
            let ix = x + match align {
                TextAlign::Start => 0.0,
                TextAlign::Center => (w - shown_w) * 0.5,
                TextAlign::End => w - shown_w,
            };
            self.out.images.push(ImageBox { src: src.clone(), r: rect(ix, cur, shown_w, shown_h), image, link: link.clone(), region: self.region });
            self.out.natural_w = self.out.natural_w.max(x + shown_w);
            cur += shown_h;
            drew_image = true;
            run_start = i + 1;
        }
        let run = if drew_image { trim_run_spaces(trim_breaks(&inlines[run_start..])) } else { inlines[run_start..].to_vec() };
        if !run.is_empty() {
            if drew_image {
                cur += em * 0.7;
            }
            cur += self.text_piece(&run, x, cur, w, wrap, ty, color, align, true);
        } else if drew_image && self.in_cell {
            cur += em * 0.7;
        }
        cur - y
    }

    fn margins(&self, block: &Block, in_li: bool) -> (f32, f32) {
        let em = self.base.size;
        match block {
            Block::Paragraph { tight: true, .. } | Block::Anchor(_) => (0.0, 0.0),
            Block::Paragraph { .. } => {
                let m = if in_li { 0.18 } else { 0.48 } * em;
                (m, m)
            }
            Block::Heading { level, .. } => {
                let size = em * heading_scale(*level);
                (1.05 * size, 0.42 * size)
            }
            Block::Code { .. } | Block::Quote(_) => (0.65 * em, 0.65 * em),
            Block::Math { .. } => (0.72 * em, 0.72 * em),
            Block::Table { .. } => (0.7 * em, 0.7 * em),
            Block::Rule => (0.9 * em, 0.9 * em),
            Block::Details { .. } => (0.5 * em, 0.5 * em),
            // Through a list and its first and last items, margins collapse into one: the largest of them.
            Block::List { items, .. } => {
                let (mut top, mut bottom) = (0.5 * em, 0.5 * em);
                if let Some(first) = items.first().and_then(|i| i.blocks.first()) {
                    top = top.max(self.margins(first, true).0);
                }
                if let Some(last) = items.last().and_then(|i| i.blocks.last()) {
                    bottom = bottom.max(self.margins(last, true).1);
                }
                (top, bottom)
            }
            // `<li>` in `<ol>` in `<section>`: the first item's margin is the list's, and the rest are the items'.
            Block::Footnote { number, blocks, .. } => {
                let item = 0.18 * em;
                let top = blocks.first().map_or(0.0, |b| self.margins(b, true).0).max(if *number <= 1 { 0.5 * em } else { item });
                let bottom = blocks.last().map_or(0.0, |b| self.margins(b, true).1).max(item);
                (top, bottom)
            }
            Block::Aligned { blocks, .. } | Block::Indented { blocks, .. } => {
                let top = blocks.first().map_or(0.0, |b| self.margins(b, in_li).0);
                let bottom = blocks.last().map_or(0.0, |b| self.margins(b, in_li).1);
                (top, bottom)
            }
        }
    }

    /// Blocks one under the other with their margins collapsed; the first's top margin and the last's bottom are the
    /// caller's. Answers the height between those.
    fn lay_seq(&mut self, blocks: &[Block], y: f32, env: Env) -> f32 {
        let mut cur = y;
        let mut prev: Option<f32> = None;
        for block in blocks {
            if let Block::Anchor(id) = block {
                self.out.anchors.push((id.clone(), cur + prev.unwrap_or(0.0)));
                continue;
            }
            let (top, bottom) = self.margins(block, env.in_li);
            if let Some(prev) = prev {
                cur += prev.max(top);
            }
            let mut column = env;
            if !self.floats.is_empty() && matches!(block, Block::Code { .. } | Block::Table { .. } | Block::Math { .. } | Block::Rule) {
                (cur, column) = self.beside_floats(cur, env);
            }
            let (pieces, images) = (self.out.pieces.len() as u32, self.out.images.len() as u32);
            let height = self.lay_block(block, cur, column);
            self.outline(block as *const Block as usize, rect(column.x, cur, column.w, height), pieces, images);
            cur += height;
            prev = Some(bottom);
        }
        cur - y
    }

    fn outline(&mut self, thing: usize, r: Rect, pieces: u32, images: u32) {
        let outlined = Outlined { r, region: self.region, pieces: pieces..self.out.pieces.len() as u32, images: images..self.out.images.len() as u32 };
        self.out.outline.insert(thing, outlined);
    }

    fn lay_block(&mut self, block: &Block, y: f32, env: Env) -> f32 {
        let t = self.theme;
        let em = self.base.size;
        match block {
            Block::Paragraph { inlines, .. } => self.lay_inlines(inlines, env.x, y, env.w, self.base, env.color, env.align),
            Block::Heading { level, inlines, id } => {
                let size = em * heading_scale(*level);
                let ty = TypeStyle { size, line: (size * 1.3).round(), weight: Weight::Semibold, mono: false, italic: false, serif: self.base.serif };
                if let Some(id) = id {
                    self.out.anchors.push((id.clone(), y));
                }
                self.out.headings.push((plain_text(inlines), y));
                self.lay_inlines(inlines, env.x, y, env.w, ty, env.color, env.align)
            }
            Block::Code { uid, label, language, code, chrome } => self.lay_code(*uid, label.as_deref(), *language, code, *chrome, y, env),
            Block::Math { uid, source } => self.lay_math(*uid, source, y, env),
            Block::Quote(blocks) => {
                let pad = 0.05 * em;
                let inner = 2.0 + 0.85 * em;
                let env2 = Env { x: env.x + inner, w: (env.w - inner).max(1.0), color: t.text_secondary, in_li: false, ..env };
                let h = self.lay_seq(blocks, y + pad, env2) + pad * 2.0;
                self.deco(rect(env.x, y, 2.0, h), 0.0, t.control_strong_stroke, None);
                h
            }
            Block::List { start, items } => self.lay_list(*start, items, y, env),
            Block::Table { uid, rows } => self.lay_table(*uid, rows, y, env),
            Block::Rule => {
                self.deco(rect(env.x, y, env.w, 1.0), 0.0, t.divider, None);
                1.0
            }
            Block::Aligned { align, blocks } => self.lay_seq(blocks, y, Env { align: *align, ..env }),
            Block::Indented { by, blocks } => self.lay_seq(blocks, y, Env { x: env.x + by, w: (env.w - by).max(1.0), ..env }),
            Block::Details { uid, open, summary, blocks } => self.lay_details(*uid, *open, summary, blocks, y, env),
            Block::Footnote { number, label, blocks } => {
                let pad = 1.65 * em;
                self.out.anchors.push((format!("user-content-fn-{label}"), y));
                self.out.markers.push(Marker { x: env.x + pad, y, kind: MarkerKind::Number(Arc::from(format!("{number}."))), color: env.color, ty: self.base, region: self.region });
                let env2 = Env { x: env.x + pad, w: (env.w - pad).max(1.0), in_li: true, ..env };
                self.lay_seq(blocks, y, env2).max(self.base.line)
            }
            Block::Anchor(_) => 0.0,
        }
    }

    fn lay_list(&mut self, start: Option<u64>, items: &[ListItem], y: f32, env: Env) -> f32 {
        let em = self.base.size;
        // `ul.contains-task-list`. An `ol` keeps its padding, but a `.task-list-item` is a flex box whichever it is in: no marker, and
        // it does not count towards the numbers after it.
        let tasks = items.iter().any(|i| i.task.is_some());
        let plain_list = tasks && start.is_none();
        let pad = if plain_list { 0.25 * em } else { 1.65 * em };
        let item_margin = 0.18 * em;
        let mut cur = y;
        let mut prev: Option<f32> = None;
        let mut number = start.unwrap_or(1);
        for item in items {
            let top = item.blocks.first().map_or(0.0, |b| self.margins(b, true).0).max(item_margin);
            if let Some(prev) = prev {
                cur += prev.max(top);
            }
            let mut x = env.x + pad;
            match (item.task, start) {
                (Some(done), _) => {
                    // `.task-list-item > input { margin-top: .38em }`, then its `gap: .45em`.
                    self.out.markers.push(Marker { x, y: cur + 0.38 * em, kind: MarkerKind::Check(done), color: env.color, ty: self.base, region: self.region });
                    x += 13.0 + 0.45 * em;
                }
                (None, Some(_)) => {
                    let label: Arc<str> = Arc::from(format!("{number}."));
                    self.out.markers.push(Marker { x, y: cur, kind: MarkerKind::Number(label), color: env.color, ty: self.base, region: self.region });
                    number += 1;
                }
                (None, None) if plain_list => {}
                (None, None) => {
                    let kind = match env.list_depth {
                        0 => MarkerKind::Disc,
                        1 => MarkerKind::Circle,
                        _ => MarkerKind::Square,
                    };
                    self.out.markers.push(Marker { x, y: cur, kind, color: env.color, ty: self.base, region: self.region });
                }
            }
            let env2 = Env { x, w: (env.x + env.w - x).max(1.0), in_li: true, list_depth: env.list_depth + 1, ..env };
            cur += self.lay_seq(&item.blocks, cur, env2).max(self.base.line);
            prev = Some(item.blocks.last().map_or(0.0, |b| self.margins(b, true).1).max(item_margin));
        }
        cur - y
    }

    fn lay_details(&mut self, uid: u32, open: bool, summary: &[Inline], blocks: &[Block], y: f32, env: Env) -> f32 {
        let key = (self.top, uid);
        let open = self.ui.get(&key).and_then(|u| u.open).unwrap_or(open);
        // The marker is inside the summary's line, as a page's disclosure triangle is.
        let indent = self.base.size * 1.1;
        self.out.markers.push(Marker { x: env.x, y, kind: MarkerKind::Disclosure(open), color: env.color, ty: self.base, region: self.region });
        let fallback;
        let runs: &[Inline] = if summary.is_empty() {
            fallback = [Inline::Text { text: mewrk_client::t("详情", "Details").to_string(), style: InlineStyle::default(), link: None }];
            &fallback
        } else {
            summary
        };
        let h = self.lay_inlines(runs, env.x + indent, y, (env.w - indent).max(1.0), self.base, env.color, env.align).max(self.base.line);
        self.out.toggles.push(Toggle { key, r: rect(env.x, y, env.w, h), open, region: self.region });
        if !open || blocks.is_empty() {
            return h;
        }
        let env2 = Env { in_li: false, ..env };
        let gap = blocks.first().map_or(0.0, |b| self.margins(b, false).0);
        h + gap + self.lay_seq(blocks, y + h + gap, env2)
    }

    #[allow(clippy::too_many_arguments)]
    fn lay_code(&mut self, uid: u32, label: Option<&str>, language: Option<Lang>, code: &Arc<str>, chrome: bool, y: f32, env: Env) -> f32 {
        let t = self.theme;
        let key = (self.top, uid);
        let state = self.ui.get(&key).copied().unwrap_or_default();
        // Both are the appearance settings', and what the pointer does: nothing about one block is kept but that.
        let wrap = self.wrap_default;
        let collapsed = self.collapse_default && !state.hover;
        let ty = code_type(self.base);
        let plain = highlight::color(Token::Plain, &t);
        // `pre`'s `padding: 10px 12px` inside its one-pixel border.
        let (pad_x, pad_y) = (13.0, 11.0);
        let inner_w = (env.w - pad_x * 2.0).max(1.0);
        let width = if wrap { (inner_w * self.scale).max(1.0) } else { NO_WRAP };
        let region_index = self.out.regions.len() as u16;
        // The box goes in first and learns its size last, so it stays behind whatever the block draws.
        let deco_at = self.out.decos.len();
        self.out.decos.push(Deco { r: rect(0.0, 0.0, 0.0, 0.0), radius: 8.0, fill: t.code_bg, stroke: Some(t.card_stroke), region: self.region });
        let saved_region = self.region.replace(region_index);

        let lines = split_code_lines(code);
        let colored = lines.len() <= MAX_HIGHLIGHTED_LINES;
        let mut grammar_state = State::default();
        let mut cur = y + pad_y;
        let mut natural: f32 = 0.0;
        self.group += 1;
        let group = self.group;
        let mut label_hash = FxHasher::default();
        label.hash(&mut label_hash);
        for chunk in lines.chunks(CHUNK_LINES.max(1)) {
            let mut h = FxHasher::default();
            h.write_u8(7);
            hash_type(&ty, &mut h);
            h.write_u32(width.to_bits());
            h.write_u64(label_hash.finish());
            h.write_u32(grammar_state.0);
            colored.hash(&mut h);
            hash_color(plain, &mut h);
            for line in chunk {
                line.hash(&mut h);
            }
            let piece_key = h.finish();
            let lay = match self.pool.take(piece_key) {
                Some(lay) => lay,
                None => self.shape_code(chunk, if colored { language } else { None }, grammar_state, width, ty, plain),
            };
            grammar_state = lay.end_state;
            natural = natural.max(lay.natural_w);
            let height = lay.h;
            let sep = std::mem::replace(&mut self.sep, Sep::Newline);
            self.out.pieces.push(Piece { x: env.x + pad_x, y: cur, w: inner_w, lay, key: piece_key, align: TextAlign::Start, sep, region: Some(region_index), group, fade: true });
            cur += height;
        }
        self.region = saved_region;
        let full_h = cur - y + pad_y;
        // `max-height: 22em`, border box.
        let limit = 22.0 * self.base.size;
        let can_collapse = full_h > limit;
        let h = if collapsed && can_collapse { limit } else { full_h };
        let r = rect(env.x, y, env.w, h);
        self.out.decos[deco_at].r = r;
        let content_w = if wrap { env.w } else { natural + pad_x * 2.0 };
        self.out.natural_w = self.out.natural_w.max(env.x + content_w);
        let chrome = chrome.then(|| Chrome { label: label.map(Arc::from), code: code.clone(), can_collapse });
        self.out.regions.push(Region { key, r, clip: r.inset(1.0), content_w: content_w - 2.0, chrome, parent: saved_region });
        h
    }

    fn shape_code(&mut self, lines: &[&str], language: Option<Lang>, start: State, width: f32, ty: TypeStyle, plain: Color) -> TextLay {
        SHAPED.with(|c| c.set(c.get() + 1));
        let scale = self.scale;
        let t = self.theme;
        let mut doc = Doc::new(width);
        doc.begin_block(self.text, BlockKind::Code, 0.0, 0.0, ty);
        let mut state = start;
        let mut tokens = Vec::new();
        let mut copy = Vec::new();
        let tab: Arc<str> = Arc::from("\t");
        let mut buffer = String::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                doc.push(self.text, "\n", ty, plain);
            }
            let base = doc.text().len();
            let shown = clean_code_line(line, &mut buffer, |from, to| copy.push(CopyEdit { range: (base + from) as u32..(base + to) as u32, with: tab.clone() }));
            match language {
                Some(lang) => {
                    highlight::line(lang, &mut state, shown, &mut tokens);
                    for (i, (at, token)) in tokens.iter().enumerate() {
                        let end = tokens.get(i + 1).map_or(shown.len(), |next| next.0);
                        let token_ty = TypeStyle { italic: *token == Token::Comment && t.code.comment_italic, ..ty };
                        doc.push(self.text, &shown[*at..end], token_ty, highlight::color(*token, &t));
                    }
                }
                None => doc.push(self.text, shown, ty, plain),
            }
        }
        let natural_w = doc.lines.iter().map(|l| l.width).fold(0.0, f32::max) / scale;
        let h = doc.height() / scale;
        TextLay { doc, spans: Vec::new(), links: Vec::new(), copy, shifts: Vec::new(), formulas: Vec::new(), rubies: Vec::new(), notes: Vec::new(), natural_w, h, end_state: state }
    }

    /// What to show for a display formula. A formula in the tail of a stream that does not parse right now is most likely
    /// not finished, and keeps its last good drawing until it parses again, so that it does not jump between a drawing and its
    /// source with every commit. Upstream does let it jump. One that never parsed shows its source, quietly.
    fn display_shown(&mut self, key: UiKey, source: &str) -> Shown {
        let size = self.base.size;
        if !self.streaming_tail {
            let typeset = typeset(self.text, source, true, size);
            if let Typeset::Error(message) = &*typeset {
                return Shown::Source { pending: false, message: Some(message.clone()) };
            }
            return Shown::Drawn(typeset);
        }
        let complete = without_partial_command(source);
        let typeset = typeset(self.text, complete, true, size);
        if !matches!(&*typeset, Typeset::Error(_)) {
            self.held.insert(key, Held { source: complete.to_owned(), typeset: typeset.clone() });
            self.out.provisional |= complete.len() != source.len();
            return Shown::Drawn(typeset);
        }
        self.out.provisional = true;
        match self.held.get(&key) {
            Some(held) if source.starts_with(held.source.as_str()) => Shown::Drawn(held.typeset.clone()),
            _ => Shown::Source { pending: true, message: None },
        }
    }

    fn lay_math(&mut self, uid: u32, source: &str, y: f32, env: Env) -> f32 {
        let key = (self.top, uid);
        // `.math-formula--display`'s `padding: .15em 0`. Its margin is `margins`' to give.
        let pad = 0.15 * self.base.size;
        let region_index = self.out.regions.len() as u16;
        let saved = self.region.replace(region_index);
        let shown = self.display_shown(key, source);
        let width = match &shown {
            Shown::Drawn(_) => NO_WRAP,
            // `overflow-wrap: anywhere`: a source that cannot be drawn wraps in its column.
            Shown::Source { .. } => (env.w * self.scale).max(1.0),
        };
        let mut h = FxHasher::default();
        h.write_u8(8);
        hash_type(&self.base, &mut h);
        hash_color(env.color, &mut h);
        h.write_u32(width.to_bits());
        source.hash(&mut h);
        match &shown {
            Shown::Drawn(typeset) => {
                h.write_u64(Rc::as_ptr(typeset) as usize as u64);
                // Only a formula with something at the side of the column cares how wide that is.
                if matches!(&**typeset, Typeset::Ready(formula) if !formula.tags.is_empty() || !formula.spread.is_empty()) {
                    h.write_u32(env.w.to_bits());
                }
            }
            Shown::Source { pending, .. } => pending.hash(&mut h),
        }
        let piece_key = h.finish();
        let lay = match self.pool.take(piece_key) {
            Some(lay) => lay,
            None => self.shape_display(&shown, source, width, env.w, env.color),
        };
        let (height, natural) = (lay.h, lay.natural_w);
        self.group += 1;
        let (sep, group) = (std::mem::replace(&mut self.sep, Sep::Newline), self.group);
        self.out.pieces.push(Piece { x: env.x, y: y + pad, w: env.w, lay, key: piece_key, align: env.align, sep, region: Some(region_index), group, fade: false });
        self.region = saved;
        let h = height + pad * 2.0;
        let r = rect(env.x, y, env.w, h);
        self.out.natural_w = self.out.natural_w.max(env.x + natural);
        // A glyph may stick out of its box a little, where upstream's `svg { overflow: visible }` lets it; the box still scrolls.
        let bleed = 2.0;
        self.out.regions.push(Region { key, r, clip: rect(r.x - bleed, r.y, r.w + bleed * 2.0, r.h), content_w: natural + bleed * 2.0, chrome: None, parent: saved });
        h
    }

    fn shape_display(&mut self, shown: &Shown, source: &str, width: f32, column: f32, color: Color) -> TextLay {
        SHAPED.with(|c| c.set(c.get() + 1));
        let scale = self.scale;
        let ty = self.base;
        let mut doc = Doc::new(width);
        doc.begin_block(self.text, BlockKind::Text, 0.0, 0.0, ty);
        let (mut formulas, mut spans, mut notes) = (Vec::new(), Vec::new(), Vec::new());
        let with: Arc<str> = match shown {
            Shown::Drawn(typeset) => {
                let mut full = 0.0;
                if let Typeset::Ready(formula) = &**typeset {
                    let Formula { h, d, .. } = *formula;
                    full = formula.full_width(column, ty.size);
                    doc.push_box(self.text, (full * scale).ceil(), (h * scale).ceil(), (d * scale).ceil(), ty);
                }
                formulas.push(InlineFormula { at: 0, formula: typeset.clone(), color, full, size: ty.size });
                Arc::from(format!("$${source}$$"))
            }
            Shown::Source { pending, message } => {
                let tint = if *pending { self.theme.text_secondary } else { self.theme.critical };
                let source_ty = TypeStyle { size: ty.size * 0.92, line: ty.line, weight: Weight::Regular, mono: true, italic: false, serif: false };
                doc.push(self.text, &collapse_spaces(&source.replace('\n', " ")), source_ty, tint);
                let range = 0..doc.text().len();
                if let Some(message) = message {
                    let y = ty.line * 0.5 + ty.size * 0.35 + 3.0;
                    let rects = rects_of(&doc, range, scale);
                    spans.extend(rects.iter().map(|r| SpanRect { r: [r[0], r[1] + y, r[2], 1.0], kind: SpanKind::Dotted, color: tint }));
                    notes.push(Note { message: message.clone() });
                }
                // Upstream's source stands on its own here: a copy has no delimiters to carry.
                Arc::from(source)
            }
        };
        let copy = vec![CopyEdit { range: 0..doc.text().len() as u32, with }];
        let natural_w = doc.lines.iter().map(|l| l.width).fold(0.0, f32::max) / scale;
        let h = doc.height() / scale;
        TextLay { doc, spans, links: Vec::new(), copy, shifts: Vec::new(), formulas, rubies: Vec::new(), notes, natural_w, h, end_state: State::default() }
    }

    /// What a cell holds, at (x, y) in a box `w` wide: answers its height. Its text stays on its lines either way, and laying it out
    /// at another width only moves it, so the second time costs nothing unless it holds blocks.
    #[allow(clippy::too_many_arguments)]
    fn lay_cell(&mut self, cell: &TableCell, x: f32, y: f32, w: f32, ty: TypeStyle, color: Color, align: TextAlign) -> f32 {
        let saved = std::mem::replace(&mut self.in_cell, true);
        let (pieces, images) = (self.out.pieces.len() as u32, self.out.images.len() as u32);
        let h = match &cell.blocks {
            None => self.lay_inlines(&cell.inlines, x, y, w, ty, color, align),
            Some(blocks) => {
                let env = Env { x, w, color, align, in_li: false, list_depth: 0 };
                // A cell's margins stay inside it.
                let top = blocks.iter().find(|b| !matches!(b, Block::Anchor(_))).map_or(0.0, |b| self.margins(b, false).0);
                let bottom = blocks.iter().rev().find(|b| !matches!(b, Block::Anchor(_))).map_or(0.0, |b| self.margins(b, false).1);
                let base = std::mem::replace(&mut self.base, ty);
                let height = self.lay_seq(blocks, y + top, env);
                self.base = base;
                top + height + bottom
            }
        };
        self.outline(cell as *const TableCell as usize, rect(x, y, w, h), pieces, images);
        self.in_cell = saved;
        h
    }

    /// The HTML table model on `border-collapse` and `width: max-content`: cells take the next free slot of their row, and whatever a
    /// spanning cell needs past what its columns and rows have goes to them in proportion.
    fn lay_table(&mut self, uid: u32, rows: &[TableRow], y: f32, env: Env) -> f32 {
        let t = self.theme;
        let key = (self.top, uid);
        let em = self.base.size * 0.94;
        let (pad_x, pad_y) = (0.62 * em, 0.38 * em);
        let ratio = self.base.line / self.base.size;
        let body = TypeStyle { size: em, line: (em * ratio).round(), ..self.base };
        let head = TypeStyle { weight: Weight::Semibold, ..body };
        let border = 1.0;

        // Slots.
        struct Slot {
            row: usize,
            cell: usize,
            col: usize,
            cols: usize,
            rows: usize,
        }
        let mut slots: Vec<Slot> = Vec::new();
        let mut taken: Vec<usize> = Vec::new();
        let mut columns = 0;
        for (r, row) in rows.iter().enumerate() {
            let left_in_section = rows[r..].iter().take_while(|other| other.section == row.section).count();
            let mut col = 0;
            for (c, cell) in row.cells.iter().enumerate() {
                while taken.get(col).is_some_and(|until| *until > r) {
                    col += 1;
                }
                let cols = (cell.colspan as usize).max(1);
                let spanned = if cell.rowspan == 0 { left_in_section } else { (cell.rowspan as usize).min(left_in_section) }.max(1);
                if taken.len() < col + cols {
                    taken.resize(col + cols, 0);
                }
                for until in &mut taken[col..col + cols] {
                    *until = (*until).max(r + spanned);
                }
                slots.push(Slot { row: r, cell: c, col, cols, rows: spanned });
                col += cols;
            }
            columns = columns.max(col);
        }
        if columns == 0 {
            return 0.0;
        }
        let region_index = self.out.regions.len() as u16;
        let saved = self.region.replace(region_index);
        // Its place comes now and what it is comes last: what its cells hold goes after it, and goes along with it.
        self.out.regions.push(Region { key, r: rect(0.0, 0.0, 0.0, 0.0), clip: rect(0.0, 0.0, 0.0, 0.0), content_w: 0.0, chrome: None, parent: saved });
        let type_of = |cell: &TableCell| if cell.header { head } else { body };

        // How big each would like to be, with what it lays out set aside. Every cell is shaped at its natural width, and wide tables
        // scroll rather than wrap.
        let outer = std::mem::take(&mut self.out);
        let mut naturals: Vec<(f32, f32)> = Vec::with_capacity(slots.len());
        for slot in &slots {
            let cell = &rows[slot.row].cells[slot.cell];
            let h = self.lay_cell(cell, 0.0, 0.0, NATURAL_CELL, type_of(cell), env.color, TextAlign::Start);
            let trial = std::mem::take(&mut self.out);
            let mut w = trial.pieces.iter().map(|p| p.x + p.lay.natural_w).fold(0.0, f32::max);
            w = trial.images.iter().map(|i| i.r.right()).fold(w, f32::max);
            w = w.max(trial.natural_w);
            naturals.push((w, h));
            self.pool.put(trial);
        }
        self.out = outer;
        let first_piece = self.out.pieces.len();

        let mut widths = vec![0.0f32; columns];
        let mut heights = vec![0.0f32; rows.len()];
        for (slot, (w, h)) in slots.iter().zip(&naturals) {
            if slot.cols == 1 {
                widths[slot.col] = widths[slot.col].max(*w);
            }
            if slot.rows == 1 {
                heights[slot.row] = heights[slot.row].max(*h);
            }
        }
        // The narrowest spans first: what they settle is what the wider ones find.
        let mut spanning: Vec<usize> = (0..slots.len()).filter(|i| slots[*i].cols > 1).collect();
        spanning.sort_by_key(|i| slots[*i].cols);
        for i in spanning {
            let slot = &slots[i];
            // A column the span covers brings its padding and its rule.
            let inner: f32 = (slot.cols - 1) as f32 * (pad_x * 2.0 + border);
            share(&mut widths[slot.col..slot.col + slot.cols], naturals[i].0 - inner);
        }
        let mut tall: Vec<usize> = (0..slots.len()).filter(|i| slots[*i].rows > 1).collect();
        tall.sort_by_key(|i| slots[*i].rows);
        for i in tall {
            let slot = &slots[i];
            let inner: f32 = (slot.rows - 1) as f32 * (pad_y * 2.0 + border);
            share(&mut heights[slot.row..slot.row + slot.rows], naturals[i].1 - inner);
        }

        // Add the cell's padding and its share of the rules, then stretch to `min-width: min(100%, 360px)`.
        let mut cols: Vec<f32> = widths.iter().map(|w| w + pad_x * 2.0 + border).collect();
        let total: f32 = cols.iter().sum::<f32>() + border;
        let min_width = env.w.min(360.0);
        if total < min_width {
            let extra = min_width - total;
            let sum: f32 = cols.iter().sum::<f32>().max(1.0);
            for col in &mut cols {
                *col += extra * *col / sum;
            }
        }
        let mut xs = Vec::with_capacity(columns + 1);
        let mut x = env.x;
        for col in &cols {
            xs.push(x);
            x += col;
        }
        xs.push(x);
        let content_w = x - env.x + border;
        let mut ys = Vec::with_capacity(rows.len() + 1);
        let mut cur = y;
        for height in &heights {
            ys.push(cur);
            cur += height + pad_y * 2.0 + border;
        }
        ys.push(cur);

        // Where everything goes, in the order it reads.
        let mut last_row = usize::MAX;
        for slot in &slots {
            let cell = &rows[slot.row].cells[slot.cell];
            let (cell_x, cell_y) = (xs[slot.col], ys[slot.row]);
            let (right, bottom) = (xs[slot.col + slot.cols], ys[slot.row + slot.rows]);
            if cell.header {
                self.deco(rect(cell_x, cell_y, right - cell_x + border, bottom - cell_y + border), 0.0, t.code_bg, None);
            }
            if last_row == slot.row {
                self.sep = Sep::Tab;
            }
            last_row = slot.row;
            let inner_w = right - cell_x - pad_x * 2.0 - border;
            self.lay_cell(cell, cell_x + border + pad_x, cell_y + border + pad_y, inner_w, type_of(cell), env.color, cell.align);
        }
        // A cell of several things and then its neighbour start over from the top.
        self.out.unsorted |= self.out.pieces[first_piece..].windows(2).any(|pair| pair[1].y < pair[0].y);

        // One rule to each edge a cell has, in runs.
        let mut level = vec![vec![false; columns]; rows.len() + 1];
        let mut upright = vec![vec![false; rows.len()]; columns + 1];
        for slot in &slots {
            for c in slot.col..slot.col + slot.cols {
                level[slot.row][c] = true;
                level[slot.row + slot.rows][c] = true;
            }
            for r in slot.row..slot.row + slot.rows {
                upright[slot.col][r] = true;
                upright[slot.col + slot.cols][r] = true;
            }
        }
        let mut level_rules = Vec::new();
        for (r, line) in level.iter().enumerate() {
            let mut c = 0;
            while c < columns {
                if !line[c] {
                    c += 1;
                    continue;
                }
                let start = c;
                while c < columns && line[c] {
                    c += 1;
                }
                level_rules.push([xs[start], ys[r], xs[c] - xs[start] + border]);
            }
        }
        let mut upright_rules = Vec::new();
        for (c, line) in upright.iter().enumerate() {
            let mut r = 0;
            while r < rows.len() {
                if !line[r] {
                    r += 1;
                    continue;
                }
                let start = r;
                while r < rows.len() && line[r] {
                    r += 1;
                }
                upright_rules.push([xs[c], ys[start], ys[r] - ys[start] + border]);
            }
        }
        let h = cur - y + border;
        self.region = saved;
        self.out.natural_w = self.out.natural_w.max(env.x + content_w);
        let r = rect(env.x, y, env.w, h);
        self.out.regions[region_index as usize] = Region { key, r, clip: r, content_w, chrome: None, parent: saved };
        self.out.grids.push(Grid { region: region_index, level: level_rules, upright: upright_rules, color: t.divider });
        h
    }
}

/// `MarkdownImage`'s `src`: what a surface's resolver makes of it, or with none a picture the page could draw by itself, which is
/// to say one already in its address: the `img-src` of `tauri.conf.json` is `'self' data:`.
pub(super) fn answer_for(resolver: Option<&ImageResolver>, src: &str) -> Option<MarkdownImage> {
    if src.is_empty() {
        return None;
    }
    match resolver {
        Some(resolve) => resolve(src),
        None => super::data_image::picture(src),
    }
}

/// How wide a cell may be while it is being measured.
const NATURAL_CELL: f32 = 16384.0;

/// A spanning cell's want, shared among what it spans: in proportion to what each already has, or evenly when none has anything.
fn share(slots: &mut [f32], want: f32) {
    let have: f32 = slots.iter().sum();
    if want <= have {
        return;
    }
    let extra = want - have;
    if have <= 0.0 {
        let each = extra / slots.len() as f32;
        slots.iter_mut().for_each(|slot| *slot += each);
    } else {
        slots.iter_mut().for_each(|slot| *slot += extra * *slot / have);
    }
}

/// `white-space: normal`, for the text of a line: tabs and runs of spaces are one space on the page.
fn collapse_spaces(text: &str) -> std::borrow::Cow<'_, str> {
    let bytes = text.as_bytes();
    let collapsible = bytes.contains(&b'\t') || bytes.windows(2).any(|w| w == b"  ");
    if !collapsible {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c == ' ' || c == '\t' {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    std::borrow::Cow::Owned(out)
}

fn trim_breaks(runs: &[Inline]) -> &[Inline] {
    let mut runs = runs;
    while matches!(runs.first(), Some(Inline::Break)) {
        runs = &runs[1..];
    }
    while matches!(runs.last(), Some(Inline::Break)) {
        runs = &runs[..runs.len() - 1];
    }
    runs
}

/// Text either side of a picture, which is a block of its own: a line starts and ends with no space, whatever the source had there.
fn trim_run_spaces(runs: &[Inline]) -> Vec<Inline> {
    let mut runs = runs.to_vec();
    if let Some(Inline::Text { text, .. }) = runs.first_mut() {
        *text = text.trim_start_matches(' ').to_owned();
    }
    if let Some(Inline::Text { text, .. }) = runs.last_mut() {
        *text = text.trim_end_matches(' ').to_owned();
    }
    runs.retain(|run| !matches!(run, Inline::Text { text, .. } if text.is_empty()));
    runs
}

pub(super) fn heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.42,
        2 => 1.25,
        3 => 1.12,
        _ => 1.0,
    }
}

fn hash_link(link: Option<&Rc<Link>>, h: &mut FxHasher) {
    match link {
        None => h.write_u8(0),
        Some(link) => {
            h.write_u8(1);
            // A link's target decides what a click does, and it is part of what the shaped run hands out.
            format!("{:?}", link.target).hash(h);
            link.title.hash(h);
        }
    }
}

pub(super) fn plain_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text { text, .. } => out.push_str(text),
            Inline::Math { source, .. } => out.push_str(source),
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::Ruby { base, .. } => out.push_str(base),
            Inline::Break => out.push(' '),
            Inline::Anchor(_) => {}
        }
    }
    out
}

/// `splitCodeLines`: a file ending in a newline has no extra last line.
pub fn split_code_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.len() > 1 && lines.last() == Some(&"") {
        lines.pop();
    }
    for line in &mut lines {
        *line = line.strip_suffix('\r').unwrap_or(line);
    }
    lines
}

/// A code line as drawn: tabs to their stops and control characters blanked, which a font would draw as boxes. Tells the
/// caller where each tab went, so a copy can put it back.
fn clean_code_line<'a>(line: &'a str, buffer: &'a mut String, mut tab: impl FnMut(usize, usize)) -> &'a str {
    if !line.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return line;
    }
    buffer.clear();
    let mut col = 0;
    for c in line.chars() {
        match c {
            '\t' => {
                let n = TAB_STOP - col % TAB_STOP;
                let from = buffer.len();
                buffer.extend(std::iter::repeat_n(' ', n));
                tab(from, buffer.len());
                col += n;
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                buffer.push(' ');
                col += 1;
            }
            c => {
                buffer.push(c);
                col += 1;
            }
        }
    }
    buffer
}

/// A top-level block's own top margin, which is what decides how far below the last block it starts.
pub(super) fn top_margin(blocks: &[Block], cx: &Cx<'_>) -> f32 {
    blocks.first().map_or(0.0, |b| cx.margins(b, false).0)
}

/// A top-level block at one width.
#[allow(clippy::too_many_arguments)]
pub(super) fn lay_top(blocks: &[Block], width: f32, cx: &mut Cx<'_>) -> Laid {
    let env = Env { x: 0.0, w: width.max(1.0), color: cx.color, align: TextAlign::Start, in_li: false, list_depth: 0 };
    let carried = cx.floats.clone();
    let height = cx.lay_seq(blocks, 0.0, env);
    let (mt, mb) = (top_margin(blocks, cx), blocks.iter().rev().find(|b| !matches!(b, Block::Anchor(_))).map_or(0.0, |b| cx.margins(b, false).1));
    let mut laid = std::mem::take(&mut cx.out);
    laid.carried = carried;
    laid.floats = std::mem::take(&mut cx.floats);
    laid.width = width;
    laid.height = height;
    laid.mt = mt;
    laid.mb = mb;
    laid.max_piece_h = laid.pieces.iter().map(Piece::h).fold(0.0, f32::max);
    laid.decos.sort_by(|a, b| a.r.y.total_cmp(&b.r.y));
    laid.max_deco_h = laid.decos.iter().map(|d| d.r.h).fold(0.0, f32::max);
    laid.markers.sort_by(|a, b| a.y.total_cmp(&b.y));
    laid.grids.iter_mut().for_each(|grid| grid.level.sort_by(|a, b| a[1].total_cmp(&b[1])));
    let text_w = laid.pieces.iter().filter(|p| p.region.is_none()).map(|p| p.x + p.lay.natural_w).fold(0.0, f32::max);
    laid.natural_w = laid.natural_w.max(text_w);
    laid
}
