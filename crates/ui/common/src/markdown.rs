//! `MarkdownContent.tsx`, with `lib/markdownHtml.ts`, `lib/documentLinks.ts`, `lib/externalLinks.ts`, `MarkdownCodeBlock` from
//! `CodeBlock.tsx`, `MathFormula.tsx` and `lib/remarkPathLinks.ts` as they are used by it.
//!
//! A message is one `custom` leaf that lays out and paints all its blocks itself, rather than an element tree: timelines
//! virtualise and the view runs every frame, and one leaf is what lets a selection run across blocks. State lives in a
//! bounded table outside the view, because a virtualised card scrolled away has to come back with its layout, its open
//! disclosures and its selection.
//!
//! What it costs. Building the view hashes the content and does nothing else unless that changed. A change reparses only
//! from the start of the last top-level block when the text before it is the same, and every block whose source is unchanged
//! keeps its shaped layout, so a stream reshapes its tail. A frame finds the visible blocks by bisection and copies their
//! visible lines. What a screen reader is told about it is in `reading.rs`.

#![allow(clippy::type_complexity)]

mod data_image;
mod html;
mod layout;
mod math;
mod math_layout;
mod parse;
mod reading;
mod tex;

pub use html::may_contain_html;
pub use layout::split_code_lines;
pub use math::normalize_math_delimiters;
pub use parse::{parse_markdown, Block, Float, Inline, InlineStyle, Link, LinkTarget, ListItem, ParseOptions, TableCell, TableRow};

use crate::path_text::{external_http_url, slug_matches};
use fluent::gpu::{GlyphInst, ImageId};
use fluent::text::{hash_str, is_cjk, TextCtx};
use layout::{BlockUi, Cx, FloatBox, Held, Laid, Pool, Piece, Region, Sep, SpanKind, UiKey};
use mewrk_client::prelude::*;
use mewrk_client::runs::stream_reveal::{fresh_alpha, next_stream_reveal_split, reveal_timings, StreamRevealSplit, STREAM_REVEAL_MS};
use parse::Defs;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::rc::Weak;

/// A picture a surface already has: `resolve_image_src`'s answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarkdownImage {
    pub id: ImageId,
    pub size: (u32, u32),
}

pub type ImageResolver = Rc<dyn Fn(&str) -> Option<MarkdownImage>>;

/// A click on, or a rest over, a detected path or a link to a file.
#[derive(Clone, Debug, PartialEq)]
pub struct PathLink {
    /// Exactly what the link carries: absolute, or relative to `base_dir`.
    pub path: String,
    /// The line the reference named, or `None` when it named only a file.
    pub line: Option<u32>,
    /// The directory relative paths resolve against, or `None` when the surface knew none.
    pub base_dir: Option<String>,
    /// The link's box, for a menu that opens where the link is.
    pub anchor: Rect,
}

#[derive(Clone)]
pub struct MarkdownOptions {
    /// A live stream: what each commit adds sweeps in from the left.
    pub streaming: bool,
    /// Turn file paths into links. Callers opt in per surface, so text a person typed is never parsed this way.
    pub linkify_paths: bool,
    /// What relative paths resolve against, read at click time.
    pub path_base_dir: Option<String>,
    /// Render the HTML the text carries, through GitHub's allow-list. Text a person typed renders exactly as typed.
    pub render_html: bool,
    /// Leave relative links to `on_document_link`: a link in a repository document is written against the document, which
    /// only the file pane knows.
    pub document_links: bool,
    /// Appearance's `singleDollarMath`.
    pub single_dollar_math: bool,
    /// The base text style.
    pub ty: TypeStyle,
    /// The text's colour; the theme's primary by default.
    pub color: Option<Color>,
    /// `Some(image)` is drawn; `None` leaves the alternative text standing. With no resolver every picture is its description,
    /// as remote ones are upstream, where the page's `img-src` refuses them.
    pub resolve_image_src: Option<ImageResolver>,
    /// A detected path or file link was clicked.
    pub on_open_path: Option<Rc<dyn Fn(&mut App, PathLink)>>,
    /// `document_links`: a relative `href` goes to the caller.
    pub on_document_link: Option<Rc<dyn Fn(&mut App, &str)>>,
    /// The pointer rested on a path for 60 ms: a lookup can start while the reader is still deciding.
    pub on_hover_path: Option<Rc<dyn Fn(&mut App, &PathLink)>>,
    /// An http(s) link was clicked. By default it goes to the host's `open_external_url`.
    pub on_open_external: Option<Rc<dyn Fn(&mut App, &str)>>,
    /// Brings this window rectangle into view: what a `#fragment` link wants, and a drag past the edge of the view. A leaf has
    /// no way to reach the scroller it is in, so without this a fragment link does nothing.
    pub on_reveal: Option<Rc<dyn Fn(&mut App, Rect)>>,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            streaming: false,
            linkify_paths: false,
            path_base_dir: None,
            render_html: false,
            document_links: false,
            single_dollar_math: true,
            ty: TypeStyle { size: 13.0, line: 20.5, weight: Weight::Regular, mono: false, italic: false, serif: false },
            color: None,
            resolve_image_src: None,
            on_open_path: None,
            on_document_link: None,
            on_hover_path: None,
            on_open_external: None,
            on_reveal: None,
        }
    }
}

impl MarkdownOptions {
    fn parse_options(&self) -> ParseOptions {
        ParseOptions { linkify_paths: self.linkify_paths, render_html: self.render_html, document_links: self.document_links, single_dollar_math: self.single_dollar_math }
    }
}

/// `estimateMarkdownHeight`: a bounded sample, so a multi-megabyte message that has not entered the viewport allocates nothing.
pub fn estimate_markdown_height(content: &str, width: f32) -> f32 {
    if content.is_empty() {
        return 20.0;
    }
    const SAMPLE: usize = 16_384;
    // Upstream's 64 characters a line, at its column's width.
    let per_line = if width > 0.0 { (width / 7.0).round().max(8.0) } else { 64.0 };
    let (mut lines, mut length, mut bytes) = (0.0f64, 0.0f32, 0usize);
    let mut flush = |length: f32| lines += (length / per_line).ceil().max(1.0) as f64;
    for c in content.chars() {
        if bytes >= SAMPLE {
            break;
        }
        bytes += c.len_utf8();
        if c == '\n' {
            flush(length);
            length = 0.0;
        } else {
            length += if is_cjk(c) { 2.0 } else { 1.0 };
        }
    }
    flush(length);
    let visual = (lines * (content.len() as f64 / bytes.max(1) as f64)).ceil();
    ((visual * 22.0) as f32).clamp(20.0, 2_000_000.0)
}

// ---------------------------------------------------------------- retained state

struct TopBlock {
    hash: u64,
    start: usize,
    /// A footnote's number, or 0.
    footnote: u32,
    index: u32,
    blocks: Vec<Block>,
    /// At most two widths: a parent that asks about one width and settles on another must not lay the message out twice a frame.
    laid: Vec<Laid>,
}

impl TopBlock {
    fn laid_at(&self, width: f32) -> Option<&Laid> {
        self.laid.iter().find(|l| same_width(l.width, width))
    }

    fn laid_at_mut(&mut self, width: f32) -> Option<&mut Laid> {
        self.laid.iter_mut().find(|l| same_width(l.width, width))
    }

    fn any_laid(&self) -> Option<&Laid> {
        self.laid.first()
    }
}

fn same_width(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

/// Top of each block and the total height, for one width and one generation of the blocks.
struct Positions {
    width: f32,
    generation: u64,
    ys: Vec<f32>,
    height: f32,
}

/// A position in reading order: the block, the piece in it, and the byte in its text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Pos {
    block: u32,
    piece: u32,
    byte: u32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Unit {
    Char,
    Word,
    Block,
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    /// What the press took: one caret, a word, a block. A drag extends from it by the same unit.
    base: (Pos, Pos),
    unit: Unit,
    range: (Pos, Pos),
}

impl Selection {
    fn is_empty(&self) -> bool {
        self.range.0 == self.range.1
    }
}

/// What the current press went down on, decided on release.
enum Pressed {
    None,
    Chrome { key: UiKey, block: u32, region: u16 },
    Bar { key: UiKey, block: u32, region: u16, grab_x: f32, start: f32 },
    Toggle { key: UiKey },
    Link { block: u32, piece: u32, link: u32 },
    ImageLink { block: u32, image: u32 },
}

/// The text of one piece a commit added: everything at or after `start`, and its share of the one sweep the commit gets. Spread in
/// reading order, each piece starts when the text before it has finished, so text that crosses a paragraph arrives as one stroke.
#[derive(Clone, Copy, Debug)]
struct Reveal {
    start: Pos,
    since: f32,
    delay: f32,
    duration: f32,
    /// What the mask's edge is measured in.
    em: f32,
}

struct PendingReveal {
    /// What was on screen from `first_changed` on, or nothing if it had not been drawn yet.
    previous: Option<String>,
    first_changed: u32,
}

/// How long one commit's text takes to arrive.
const REVEAL_SECONDS: f32 = STREAM_REVEAL_MS as f32 / 1000.0;
/// How wide "as wide as it wants" goes.
const NATURAL_CAP: f32 = 4096.0;
const HOVER_DWELL: f32 = 0.06;
const MIN_LAYOUT_WIDTH: f32 = 4.0;

struct MdState {
    /// The width last asked about by layout.
    measured_width: f32,
    content_hash: u64,
    /// Nothing has been parsed yet.
    fresh: bool,
    parse_options: ParseOptions,
    html_on: bool,
    /// What the blocks were parsed from, after the math pass.
    source: String,
    defs: Defs,
    blocks: Vec<TopBlock>,
    /// Bumped whenever a block or a toggle changes.
    generation: u64,
    options: MarkdownOptions,

    theme: Option<Theme>,
    scale: f32,
    ty: TypeStyle,
    color: Option<Color>,
    wrap_default: bool,
    collapse_default: bool,
    pool: Pool,
    /// What a display formula still arriving last drew. Only for a message that is streaming.
    held_math: HashMap<UiKey, Held>,
    positions: Vec<Positions>,
    natural: Option<(u64, f32)>,
    last_height: f32,
    has_images: bool,
    /// Some block has a picture out of the text, which can reach down past that block's end.
    has_floats: bool,

    ui: HashMap<UiKey, BlockUi>,
    /// The tall code block the pointer is on, as far as the layout knows, and what the pointer has moved to since.
    hovered_code: Option<UiKey>,
    next_hovered_code: Option<Option<UiKey>>,
    selection: Option<Selection>,
    dragging: bool,
    pressed: Pressed,
    last_press: Option<(f32, f32, u8)>,
    hover_path: Option<(u32, u32, u32, f32, bool)>,
    reveals: Vec<Reveal>,
    pending_reveal: Option<PendingReveal>,
    source_bytes: usize,
    /// Where the leaf was painted last: what a fragment's window rectangle is worked out from, and what tells a press on this
    /// message from a press elsewhere while this one is not the one being painted.
    leaf_rect: Cell<Rect>,
    /// The message as a screen reader walks it, and what that was read from.
    reading: Option<(reading::ReadingShape, Rc<[AriaChild<App>]>)>,
    /// A screen reader pressed a code block's copy button, and only a frame holds the clipboard.
    pending_copy: Option<(UiKey, Arc<str>)>,
}

impl MdState {
    fn new(options: MarkdownOptions) -> Self {
        Self {
            measured_width: 0.0,
            content_hash: 0,
            fresh: true,
            parse_options: options.parse_options(),
            html_on: false,
            source: String::new(),
            defs: HashMap::new(),
            blocks: Vec::new(),
            generation: 1,
            ty: options.ty,
            color: options.color,
            options,
            theme: None,
            scale: 0.0,
            wrap_default: false,
            collapse_default: false,
            pool: Pool::default(),
            held_math: HashMap::new(),
            positions: Vec::new(),
            natural: None,
            last_height: 0.0,
            has_images: false,
            has_floats: false,
            ui: HashMap::new(),
            hovered_code: None,
            next_hovered_code: None,
            selection: None,
            dragging: false,
            pressed: Pressed::None,
            last_press: None,
            hover_path: None,
            reveals: Vec::new(),
            pending_reveal: None,
            source_bytes: 0,
            reading: None,
            pending_copy: None,
            leaf_rect: Cell::new(rect(0.0, 0.0, 0.0, 0.0)),
        }
    }

    /// The text changed: blocks whose source did not change stay as they are, layouts and all.
    fn update_content(&mut self, content: &str) {
        let options = self.options.parse_options();
        let normalized = math::normalized(content);
        let prepared = math::prepare_math(&normalized, options.single_dollar_math);
        let prepared: &str = &prepared;
        let html_on = options.render_html && html::may_contain_html(content);
        let same_mode = !self.fresh && options == self.parse_options && html_on == self.html_on;
        let footnotes = prepared.contains("[^");
        let extra = parse::footnote_labels_hash(prepared);

        // The text before the last block is the same, and a block once closed stays closed: only the rest can have changed.
        let tail_from = if same_mode && !footnotes {
            // From the start of its line: a list's marker may be indented, and how far decides what its next lines are.
            let line_start = |at: usize| self.source.as_bytes()[..at.min(self.source.len())].iter().rposition(|b| *b == b'\n').map_or(0, |n| n + 1);
            self.blocks.last().map(|b| line_start(b.start)).filter(|&at| prepared.len() >= at && prepared.as_bytes()[..at] == self.source.as_bytes()[..at] && !self.blocks.iter().any(|b| b.footnote > 0))
        } else {
            None
        };

        let mut parsed = None;
        let mut kept = 0;
        if let Some(at) = tail_from {
            let last = self.blocks.len() - 1;
            let last_hash = self.blocks[last].hash;
            let attempt = parse::parse_top(&prepared[at..], at, &self.defs, extra, options, html_on, |hash| hash == last_hash);
            // A definition can reach back to any block before it. That is the one thing a tail parse cannot answer.
            let reaches_back = attempt.defs.iter().any(|(label, value)| self.defs.get(label) != Some(value));
            if !reaches_back {
                parsed = Some(attempt);
                kept = last;
            }
        }
        let parsed = parsed.unwrap_or_else(|| {
            let have: HashSet<u64> = if same_mode { self.blocks.iter().map(|b| b.hash).collect() } else { HashSet::new() };
            let whole = parse::parse_top(prepared, 0, &HashMap::new(), extra, options, html_on, |hash| have.contains(&hash));
            self.defs = whole.defs.clone();
            whole
        });

        // How much of the rendered text can be taken as still on screen, for the reveal.
        let new_hashes: Vec<u64> = self.blocks[..kept].iter().map(|b| b.hash).chain(parsed.items.iter().map(|i| i.hash)).collect();
        let first_changed = self.blocks.iter().map(|b| b.hash).zip(&new_hashes).take_while(|(a, b)| a == *b).count();
        let previous = self.options.streaming.then(|| {
            let mut text = String::new();
            for block in &self.blocks[first_changed.min(self.blocks.len())..] {
                let laid = block.any_laid()?;
                for piece in &laid.pieces {
                    text.push_str(piece.lay.doc.text());
                }
            }
            Some(text)
        });
        let had_text = !self.blocks.is_empty();

        self.pool.clear();
        // Under other options the same source can mean something else.
        if !same_mode {
            self.blocks.clear();
        }
        let mut old: HashMap<u64, VecDeque<TopBlock>> = HashMap::new();
        for block in self.blocks.drain(kept..) {
            old.entry(block.hash).or_default().push_back(block);
        }
        for item in parsed.items {
            let reused = old.get_mut(&item.hash).and_then(VecDeque::pop_front);
            let block = match (reused, item.blocks) {
                (Some(mut block), _) => {
                    block.start = item.start;
                    block.footnote = item.footnote;
                    block
                }
                (None, Some(blocks)) => TopBlock { hash: item.hash, start: item.start, footnote: item.footnote, index: 0, blocks, laid: Vec::new() },
                // It said it had this block a moment ago. Nothing to draw beats a panic.
                (None, None) => continue,
            };
            self.blocks.push(block);
        }
        for block in old.into_values().flatten() {
            for laid in block.laid {
                self.pool.put(laid);
            }
        }
        // `remark-gfm` gathers footnotes at the end of the message, in the order they are first referred to.
        if footnotes && self.blocks.iter().any(|b| b.footnote > 0) {
            let (body, mut notes): (Vec<_>, Vec<_>) = self.blocks.drain(..).partition(|b| b.footnote == 0);
            notes.sort_by_key(|b| b.footnote);
            self.blocks = body.into_iter().chain(notes).collect();
        }
        let pool = &mut self.pool;
        for (i, block) in self.blocks.iter_mut().enumerate() {
            if block.index != i as u32 {
                // Layouts name their toggles by this index. Their shaped text is still good.
                block.index = i as u32;
                for laid in block.laid.drain(..) {
                    pool.put(laid);
                }
            }
        }

        if self.options.streaming {
            self.pending_reveal = Some(PendingReveal { previous: previous.flatten().filter(|_| had_text), first_changed: first_changed as u32 });
        } else {
            self.pending_reveal = None;
            self.reveals.clear();
        }
        self.source.clear();
        self.source.push_str(prepared);
        self.source_bytes = content.len();
        self.parse_options = options;
        self.html_on = html_on;
        self.fresh = false;
        self.generation += 1;
        self.natural = None;
    }

    fn drop_layouts(&mut self) {
        for block in &mut self.blocks {
            block.laid.clear();
        }
        self.pool.clear();
        self.positions.clear();
        self.natural = None;
        self.generation += 1;
    }

    /// `pre:hover { max-height: none }`: a tall block let go of as the pointer reaches it. Done here, before anything has asked this frame
    /// how tall the message is, so the room it takes and what it paints stay the same frame's.
    fn apply_code_hover(&mut self) {
        let Some(next) = self.next_hovered_code.take() else { return };
        if next == self.hovered_code {
            return;
        }
        let was = std::mem::replace(&mut self.hovered_code, next);
        for (key, hover) in [(was, false), (next, true)] {
            let Some(key) = key else { continue };
            self.ui.entry(key).or_default().hover = hover;
            if let Some(block) = self.blocks.get_mut(key.0 as usize) {
                for laid in block.laid.drain(..) {
                    self.pool.put(laid);
                }
            }
        }
        self.generation += 1;
        self.natural = None;
    }

    /// Whatever the layout depends on besides the blocks, checked once for each frame's first question.
    fn check_environment(&mut self, scale: f32, theme: Theme, wrap_default: bool, collapse_default: bool) {
        let changed = self.scale != scale || self.theme != Some(theme) || self.ty != self.options.ty || self.color != self.options.color || self.wrap_default != wrap_default || self.collapse_default != collapse_default;
        if changed {
            self.drop_layouts();
            self.scale = scale;
            self.theme = Some(theme);
            self.ty = self.options.ty;
            self.color = self.options.color;
            self.wrap_default = wrap_default;
            self.collapse_default = collapse_default;
        }
        self.apply_code_hover();
        // A resolver that had nothing for a picture may have it now.
        if self.has_images {
            let resolver = self.options.resolve_image_src.clone();
            let mut dirty = false;
            for block in &mut self.blocks {
                let stale = block.laid.iter().any(|laid| laid.image_refs.iter().any(|(src, seen)| {
                    layout::answer_for(resolver.as_ref(), src) != *seen
                }));
                if stale {
                    for laid in block.laid.drain(..) {
                        self.pool.put(laid);
                    }
                    dirty = true;
                }
            }
            if dirty {
                self.generation += 1;
                self.natural = None;
            }
        }
    }

    /// Lays every block out for this width, answering which entry of `positions` holds it.
    fn ensure_layout(&mut self, text: &mut TextCtx, time: f32, width: f32) -> usize {
        if let Some(at) = self.positions.iter().position(|p| same_width(p.width, width) && p.generation == self.generation) {
            return at;
        }
        let Some(theme) = self.theme else { return self.store_positions(width, Vec::new(), 0.0) };
        let color = self.options.color.unwrap_or(theme.text_primary);
        let mut evicted = false;
        let resolver = self.options.resolve_image_src.clone();
        let last = self.blocks.len().saturating_sub(1);
        let streaming = self.options.streaming;
        let mut ys = Vec::with_capacity(self.blocks.len());
        let mut y = 0.0f32;
        let mut prev: Option<f32> = None;
        // The floats that reach below `y`, measured from it. What a block is laid out around depends on these, so a change above it
        // that moves one means laying it out again. Almost always there are none.
        let mut carried: Vec<FloatBox> = Vec::new();
        let mut float_bottom = 0.0f32;
        let moved = |floats: &[FloatBox], dy: f32| floats.iter().map(|f| FloatBox { y: f.y + dy, ..*f }).filter(|f| f.y + f.h > 0.0).collect::<Vec<_>>();
        for (i, block) in self.blocks.iter_mut().enumerate() {
            // A formula drawn from what it was earlier in the stream has its answer now: the stream moved on past it, or ended.
            if !(streaming && i == last) && block.laid.iter().any(|laid| laid.provisional) {
                block.laid.clear();
            }
            if let Some(at) = block.laid.iter().position(|laid| same_width(laid.width, width) && laid.carried != moved(&carried, -prev.map_or(0.0, |p| p.max(laid.mt)))) {
                let stale = block.laid.remove(at);
                self.pool.put(stale);
            }
            if block.laid_at(width).is_none() {
                if block.laid.len() >= 2 {
                    let oldest = block.laid.remove(0);
                    self.pool.put(oldest);
                    evicted = true;
                }
                let mut cx = Cx { text, scale: self.scale, theme, base: self.options.ty, color, top: block.index, ui: &self.ui, pool: &mut self.pool, resolver: resolver.clone(), wrap_default: self.wrap_default, collapse_default: self.collapse_default, in_cell: false, out: Laid::default(), group: 0, sep: Sep::Newline, region: None, streaming_tail: streaming && i == last, held: &mut self.held_math, floats: Vec::new() };
                let mt = layout::top_margin(&block.blocks, &cx);
                cx.floats = moved(&carried, -prev.map_or(0.0, |p| p.max(mt)));
                let laid = layout::lay_top(&block.blocks, width, &mut cx);
                self.has_images |= !laid.image_refs.is_empty();
                block.laid.push(laid);
            }
            let laid = block.laid_at(width).expect("laid above");
            let gap = prev.map_or(0.0, |p| p.max(laid.mt));
            self.has_floats |= !laid.floats.is_empty();
            float_bottom = laid.floats.iter().map(|f| y + gap + f.y + f.h).fold(float_bottom, f32::max);
            // Nothing but an anchor or a floated picture: it takes no room, and its margins are nobody's.
            if laid.height <= 0.0 && laid.pieces.is_empty() {
                ys.push(y + gap);
                carried = moved(&laid.floats, gap);
                continue;
            }
            y += gap;
            ys.push(y);
            y += laid.height;
            prev = Some(laid.mb);
            carried = moved(&laid.floats, -laid.height);
        }
        // What was laid out for a width nobody uses any more has served its purpose now.
        if evicted {
            self.pool.clear();
        }
        // Nothing here clips a picture hanging off the end, so it counts.
        let y = y.max(float_bottom);
        self.settle_reveal(width, time);
        self.store_positions(width, ys, y)
    }

    fn store_positions(&mut self, width: f32, ys: Vec<f32>, height: f32) -> usize {
        self.last_height = height;
        self.positions.retain(|p| p.generation == self.generation);
        if self.positions.len() >= 2 {
            self.positions.remove(0);
        }
        self.positions.push(Positions { width, generation: self.generation, ys, height });
        self.positions.len() - 1
    }

    /// The streaming reveal's split, taken on the text as rendered: the longest prefix this render shares with the last one.
    fn settle_reveal(&mut self, width: f32, time: f32) {
        let Some(pending) = self.pending_reveal.take() else { return };
        if !self.options.streaming {
            return;
        }
        let first = pending.first_changed as usize;
        let mut text = String::new();
        // Every piece from the first changed block on, where it starts in that text, and whether it takes part.
        let mut slots: Vec<(Pos, usize, usize)> = Vec::new();
        for block in &self.blocks[first.min(self.blocks.len())..] {
            if let Some(laid) = block.laid_at(width) {
                for (p, piece) in laid.pieces.iter().enumerate() {
                    let body = piece.lay.doc.text();
                    if piece.fade && !body.trim().is_empty() {
                        slots.push((Pos { block: block.index, piece: p as u32, byte: 0 }, text.len(), body.len()));
                    }
                    text.push_str(body);
                }
            }
        }
        // The same text again, a render for some other reason, keeps the split it had, so a sweep in flight is not cut short.
        if pending.previous.as_deref() == Some(text.as_str()) {
            return;
        }
        // Only what this commit added is masked: what the last one was still sweeping in is settled now.
        self.reveals.clear();
        // One long block still arriving would otherwise be compared a character at a time, whole, on every commit. What is equal
        // byte for byte is settled however it is split, so the split only has to look past that.
        let same = pending.previous.as_deref().map_or(0, |previous| common_prefix(previous, &text));
        let previous = pending.previous.map(|previous| StreamRevealSplit { text: previous[same..].to_owned(), settled: 0, tick: 0 });
        let settled = same + next_stream_reveal_split(previous.as_ref(), &text[same..]).settled;
        if settled >= text.len() {
            return;
        }
        let spans: Vec<(usize, usize)> = slots.iter().map(|(_, offset, length)| (*offset, *length)).collect();
        let em = self.options.ty.size;
        let timings = reveal_timings(&StreamRevealSplit { text, settled, tick: 0 }, &spans);
        for ((at, _, _), timing) in slots.iter().zip(timings) {
            let Some(timing) = timing else { continue };
            self.reveals.push(Reveal { start: Pos { byte: timing.cut as u32, ..*at }, since: time, delay: timing.delay_ms as f32 / 1000.0, duration: timing.duration_ms as f32 / 1000.0, em });
        }
    }

    fn natural_width(&mut self, text: &mut TextCtx, time: f32) -> f32 {
        if let Some((generation, natural)) = self.natural
            && generation == self.generation
        {
            return natural;
        }
        self.ensure_layout(text, time, NATURAL_CAP);
        let natural = self.blocks.iter().filter_map(|b| b.laid_at(NATURAL_CAP)).map(|l| l.natural_w).fold(0.0, f32::max).ceil().min(NATURAL_CAP);
        self.natural = Some((self.generation, natural));
        natural
    }

    fn measure(&mut self, text: &mut TextCtx, time: f32, known: Option<f32>, avail: f32) -> (f32, f32) {
        match known {
            // A pane collapsed to nothing: a line a character wide would be a lot of lines to lay out for nobody.
            Some(w) if w < MIN_LAYOUT_WIDTH => (w, self.last_height),
            Some(w) => {
                let at = self.ensure_layout(text, time, w);
                self.measured_width = w;
                (w, self.positions[at].height)
            }
            // Min-content: a message can always give way, and how tall it would be then is nobody's real question.
            None if avail < MIN_LAYOUT_WIDTH => (avail.max(0.0), self.last_height),
            None if avail.is_finite() => {
                let at = self.ensure_layout(text, time, avail);
                self.measured_width = avail;
                (avail, self.positions[at].height)
            }
            None => {
                let natural = self.natural_width(text, time);
                let at = self.ensure_layout(text, time, NATURAL_CAP);
                (natural, self.positions[at].height)
            }
        }
    }

    // ------------------------------------------------------------ selection

    /// The piece at the first position, or the last, of a layout with anything to select.
    fn first_pos(&self, width: f32, from_block: usize) -> Option<Pos> {
        for block in &self.blocks[from_block.min(self.blocks.len())..] {
            if let Some(laid) = block.laid_at(width)
                && !laid.pieces.is_empty()
            {
                return Some(Pos { block: block.index, piece: 0, byte: 0 });
            }
        }
        None
    }

    fn last_pos(&self, width: f32, up_to_block: usize) -> Option<Pos> {
        for block in self.blocks[..up_to_block.min(self.blocks.len())].iter().rev() {
            if let Some(laid) = block.laid_at(width)
                && let Some(piece) = laid.pieces.last()
            {
                return Some(Pos { block: block.index, piece: laid.pieces.len() as u32 - 1, byte: piece.lay.doc.text().len() as u32 });
            }
        }
        None
    }

    /// The reading-order position nearest a point in the leaf. Above or between things is the start of what comes next; past
    /// everything is the end.
    fn locate(&self, width: f32, positions: usize, x: f32, y: f32) -> Option<Pos> {
        let ys = &self.positions[positions].ys;
        if ys.is_empty() {
            return None;
        }
        let at = ys.partition_point(|top| *top <= y).saturating_sub(1);
        let block = &self.blocks[at];
        let laid = block.laid_at(width)?;
        let rel_y = y - ys[at];
        if rel_y >= laid.height && at + 1 == ys.len() {
            return self.last_pos(width, at + 1);
        }
        if laid.pieces.is_empty() || rel_y >= laid.height {
            return self.first_pos(width, at + 1).or_else(|| self.last_pos(width, at + 1));
        }
        let scan_from = laid.first_piece_near(rel_y);
        let mut best: Option<usize> = None;
        for (i, piece) in laid.pieces.iter().enumerate().skip(scan_from) {
            if piece.y > rel_y {
                if laid.unsorted {
                    continue;
                }
                // Above this piece, and past everything before it.
                if best.is_some() {
                    break;
                }
                return Some(Pos { block: block.index, piece: i as u32, byte: 0 });
            }
            if rel_y < piece.y + piece.h() {
                // On this row: among cells beside each other, the one under the pointer.
                let scroll = self.region_scroll(laid, piece.region);
                let px = x + scroll;
                match best {
                    None => best = Some(i),
                    Some(_) if px >= piece.x => best = Some(i),
                    _ => {}
                }
            }
        }
        let i = match best {
            Some(i) => i,
            None => return self.first_pos(width, at + 1).or_else(|| self.last_pos(width, at + 1)),
        };
        let piece = &laid.pieces[i];
        let scroll = self.region_scroll(laid, piece.region);
        let byte = piece.hit(x + scroll - piece.x, rel_y - piece.y, self.scale);
        Some(Pos { block: block.index, piece: i as u32, byte: byte as u32 })
    }

    /// How far a box has moved what lies in it, with the boxes it lies in.
    fn region_scroll(&self, laid: &Laid, region: Option<u16>) -> f32 {
        let mut total = 0.0;
        let mut at = region;
        while let Some(region) = at.and_then(|r| laid.regions.get(r as usize)) {
            total += self.ui.get(&region.key).map_or(0.0, |u| u.scroll_x).clamp(0.0, region.max_scroll());
            at = region.parent;
        }
        total
    }

    fn piece_text(&self, width: f32, pos: Pos) -> Option<&str> {
        let block = self.blocks.get(pos.block as usize)?;
        let laid = block.laid_at(width).or_else(|| block.any_laid())?;
        Some(laid.pieces.get(pos.piece as usize)?.lay.doc.text())
    }

    /// The word or the run of spaces around a position, by the unit a double click selects.
    fn word_at(&self, width: f32, pos: Pos) -> (Pos, Pos) {
        let Some(text) = self.piece_text(width, pos) else { return (pos, pos) };
        let (a, b) = word_range(text, pos.byte as usize);
        (Pos { byte: a as u32, ..pos }, Pos { byte: b as u32, ..pos })
    }

    /// The block around a position: a paragraph, a cell, all of a code block.
    fn block_at(&self, width: f32, pos: Pos) -> (Pos, Pos) {
        let Some(block) = self.blocks.get(pos.block as usize) else { return (pos, pos) };
        let Some(laid) = block.laid_at(width) else { return (pos, pos) };
        let Some(piece) = laid.pieces.get(pos.piece as usize) else { return (pos, pos) };
        let group = piece.group;
        let first = (0..=pos.piece as usize).rev().take_while(|&i| laid.pieces[i].group == group).last().unwrap_or(pos.piece as usize);
        let last = (pos.piece as usize..laid.pieces.len()).take_while(|&i| laid.pieces[i].group == group).last().unwrap_or(pos.piece as usize);
        (Pos { block: pos.block, piece: first as u32, byte: 0 }, Pos { block: pos.block, piece: last as u32, byte: laid.pieces[last].lay.doc.text().len() as u32 })
    }

    fn unit_at(&self, width: f32, pos: Pos, unit: Unit) -> (Pos, Pos) {
        match unit {
            Unit::Char => (pos, pos),
            Unit::Word => self.word_at(width, pos),
            Unit::Block => self.block_at(width, pos),
        }
    }

    /// The selection as text: newlines between blocks, tabs between a table's cells.
    fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        if selection.is_empty() {
            return None;
        }
        let (lo, hi) = selection.range;
        let mut out = String::new();
        let mut first = true;
        for b in lo.block..=hi.block {
            let Some(block) = self.blocks.get(b as usize) else { break };
            let Some(laid) = block.any_laid() else { continue };
            let from = if b == lo.block { lo.piece as usize } else { 0 };
            let to = if b == hi.block { (hi.piece as usize + 1).min(laid.pieces.len()) } else { laid.pieces.len() };
            for p in from..to {
                let piece = &laid.pieces[p];
                let text = piece.lay.doc.text();
                let a = if b == lo.block && p == lo.piece as usize { floor_char(text, lo.byte as usize) } else { 0 };
                let z = if b == hi.block && p == hi.piece as usize { floor_char(text, hi.byte as usize) } else { text.len() };
                if a >= z {
                    continue;
                }
                if !first {
                    out.push(match piece.sep {
                        Sep::Newline => '\n',
                        Sep::Tab => '\t',
                    });
                }
                first = false;
                push_copy(piece, a, z, &mut out);
            }
        }
        (!out.is_empty()).then_some(out)
    }

    fn clear_selection(&mut self) {
        self.selection = None;
        self.dragging = false;
    }

    fn toggle_ui(&mut self, key: UiKey, f: impl FnOnce(&mut BlockUi)) {
        f(self.ui.entry(key).or_default());
        // The block's layout goes, and its shaped text with it only if that really changed.
        if let Some(block) = self.blocks.get_mut(key.0 as usize) {
            for laid in block.laid.drain(..) {
                self.pool.put(laid);
            }
        }
        self.generation += 1;
        self.natural = None;
    }
}

/// How many leading bytes two texts share, on a character boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    const STEP: usize = 4096;
    let (x, y) = (a.as_bytes(), b.as_bytes());
    let limit = x.len().min(y.len());
    let mut at = 0;
    while at + STEP <= limit && x[at..at + STEP] == y[at..at + STEP] {
        at += STEP;
    }
    while at < limit && x[at] == y[at] {
        at += 1;
    }
    while !a.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// `text[a..z]` as a copy has it, with what stands in for drawn text put back.
fn push_copy(piece: &Piece, a: usize, z: usize, out: &mut String) {
    let text = piece.lay.doc.text();
    let edits = &piece.lay.copy;
    if edits.is_empty() {
        out.push_str(&text[a..z]);
        return;
    }
    let mut at = a;
    for edit in edits {
        let (from, to) = (edit.range.start as usize, edit.range.end as usize);
        if to <= at || from >= z {
            continue;
        }
        if from > at {
            out.push_str(&text[at..from]);
        }
        out.push_str(&edit.with);
        at = to;
    }
    if at < z {
        out.push_str(&text[at..z]);
    }
}

fn floor_char(text: &str, byte: usize) -> usize {
    let mut at = byte.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn word_class(c: char) -> u8 {
    if c == '\n' {
        3
    } else if c.is_whitespace() {
        0
    } else if is_cjk(c) && c.is_alphabetic() {
        2
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        4
    }
}

/// What a double click selects: a run of word characters, of CJK, of spaces, or one mark of punctuation.
fn word_range(text: &str, byte: usize) -> (usize, usize) {
    let at = floor_char(text, byte);
    let Some(here) = text[at..].chars().next().or_else(|| text[..at].chars().next_back()) else { return (at, at) };
    let class = word_class(here);
    let at = if text[at..].is_empty() { at - here.len_utf8() } else { at };
    if class >= 3 {
        return (at, at + here.len_utf8());
    }
    let start = text[..at].char_indices().rev().take_while(|(_, c)| word_class(*c) == class).last().map_or(at, |(i, _)| i);
    let end = text[at..].char_indices().find(|(_, c)| word_class(*c) != class).map_or(text.len(), |(i, _)| at + i);
    (start, end)
}

// ---------------------------------------------------------------- the table

const MAX_ENTRIES: usize = 512;
/// A few long messages must not keep their glyphs for ever just because they fit the entry count.
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct Table {
    entries: HashMap<u64, (Rc<RefCell<MdState>>, u64)>,
    clock: u64,
    bytes: usize,
}

impl Table {
    fn get(&mut self, id: u64, options: &MarkdownOptions) -> Rc<RefCell<MdState>> {
        self.clock += 1;
        let clock = self.clock;
        if let Some((state, used)) = self.entries.get_mut(&id) {
            *used = clock;
            return state.clone();
        }
        if self.entries.len() >= MAX_ENTRIES {
            self.evict_oldest();
        }
        let state = Rc::new(RefCell::new(MdState::new(options.clone())));
        self.entries.insert(id, (state.clone(), clock));
        state
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest) = self.entries.iter().min_by_key(|(_, (_, used))| *used).map(|(id, _)| *id)
            && let Some((state, _)) = self.entries.remove(&oldest)
        {
            self.bytes = self.bytes.saturating_sub(state.borrow().source_bytes);
        }
    }

    fn account(&mut self, before: usize, after: usize) {
        self.bytes = self.bytes.saturating_sub(before) + after;
        while self.bytes > MAX_SOURCE_BYTES && self.entries.len() > 1 {
            self.evict_oldest();
        }
    }
}

thread_local! {
    static TABLE: RefCell<Table> = RefCell::new(Table::default());
    /// The message holding the selection, which one press anywhere has to be able to clear.
    static OWNER: RefCell<Option<Weak<RefCell<MdState>>>> = const { RefCell::new(None) };
}

fn owner() -> Option<Rc<RefCell<MdState>>> {
    OWNER.with(|o| o.borrow().as_ref().and_then(Weak::upgrade))
}

/// Whether any message or fluent selectable text has a non-empty selection: Cmd+C belongs to those words.
pub fn has_text_selection() -> bool {
    let here = owner().is_some_and(|state| state.try_borrow().is_ok_and(|s| s.selection.is_some_and(|sel| !sel.is_empty())));
    here || text_selection_active()
}

/// Drops every selection, in messages and in fluent's selectable text.
pub fn clear_text_selection() {
    if let Some(state) = owner()
        && let Ok(mut s) = state.try_borrow_mut()
    {
        s.clear_selection();
    }
    OWNER.with(|o| *o.borrow_mut() = None);
    fluent::el::clear_text_selection();
}

/// The text selected in a message, as a copy would write it. For a surface's own "Copy" row.
pub fn selected_text() -> Option<String> {
    owner().and_then(|state| state.try_borrow().ok().and_then(|s| s.selected_text()))
}

// ---------------------------------------------------------------- the element

fn appearance_defaults(app: &App) -> (bool, bool) {
    let appearance = &app.doc.global_settings.appearance;
    (appearance.code_block_wrappable, appearance.code_block_collapsible)
}

/// Where a `#fragment` of a message is on screen, once it has been laid out: for a link followed from elsewhere, which lands on
/// the document after it has been drawn.
pub fn markdown_fragment_rect(key: impl Hash, fragment: &str) -> Option<Rect> {
    let id = markdown_key(key);
    let state = TABLE.with(|t| t.borrow().entries.get(&id).map(|(s, _)| s.clone()))?;
    state.try_borrow().ok()?.fragment_rect(fragment)
}

/// What `rect_of` finds a message's leaf by.
pub fn markdown_key(key: impl Hash) -> u64 {
    hash_key(("markdown", hash_key(key)))
}

/// A message as one leaf. `content` is only read on a change; the leaf holds nothing but a reference to retained state, and it
/// answers `measure` with its height for the width it is asked about.
pub fn markdown_content(key: impl Hash, content: &str, options: MarkdownOptions) -> E {
    let id = markdown_key(key);
    let state = TABLE.with(|t| t.borrow_mut().get(id, &options));
    let hash = hash_str(content);
    {
        let mut s = state.borrow_mut();
        let changed_options = s.parse_options != options.parse_options();
        // The end of a stream settles what a formula still arriving was only standing in for.
        if s.options.streaming && !options.streaming {
            s.held_math.clear();
            if s.blocks.iter().any(|b| b.laid.iter().any(|laid| laid.provisional)) {
                s.generation += 1;
                s.natural = None;
            }
        }
        s.options = options;
        if s.fresh || hash != s.content_hash || changed_options {
            let before = s.source_bytes;
            s.update_content(content);
            s.content_hash = hash;
            let after = s.source_bytes;
            drop(s);
            TABLE.with(|t| t.borrow_mut().account(before, after));
        }
    }
    let measured = state.clone();
    let reading = a11y_active().then(|| MdState::reading_tree(&state));
    custom(move |app: &mut App, ui: &mut Ui<'_>, r: Rect| paint_leaf(app, ui, r, &state))
        .measure(move |app: &mut App, ui: &mut Ui<'_>, known: Option<f32>, avail: f32| {
            let (wrap, collapse) = appearance_defaults(app);
            data_image::remember_task(app);
            let mut s = measured.borrow_mut();
            s.check_environment(ui.scale, ui.theme, wrap, collapse);
            s.measure(ui.text, ui.time, known, avail)
        })
        .key(id)
        .min_w(0.0)
        .map(|leaf| match reading {
            Some(tree) if !tree.is_empty() => leaf.aria_children(tree),
            _ => leaf,
        })
}

/// What a frame's input asks for once the state is no longer borrowed: handlers may well reach back into it.
enum Action {
    Path(Rc<dyn Fn(&mut App, PathLink)>, PathLink),
    Hover(Rc<dyn Fn(&mut App, &PathLink)>, PathLink),
    Document(Rc<dyn Fn(&mut App, &str)>, String),
    External(String),
    Reveal(Rc<dyn Fn(&mut App, Rect)>, Rect),
}

fn paint_leaf(app: &mut App, ui: &mut Ui<'_>, r: Rect, state: &Rc<RefCell<MdState>>) {
    if r.w < MIN_LAYOUT_WIDTH {
        return;
    }
    let (wrap, collapse) = appearance_defaults(app);
    let mut actions = Vec::new();
    let copy = ui.input.keys.iter().any(|k| k.cmd && matches!(k.key, Key::Char('c') | Key::Char('C')));
    {
        let mut s = state.borrow_mut();
        s.check_environment(ui.scale, ui.theme, wrap, collapse);
        // Layout rounds the box to whole pixels after asking for its height at the unrounded width. Laid out at the rounded one, a
        // line that only just fitted wraps after all, and the message paints a line taller than the room it took.
        let width = if (s.measured_width - r.w).abs() <= 1.0 { s.measured_width } else { r.w };
        let r = rect(r.x, r.y, width, r.h);
        let positions = s.ensure_layout(ui.text, ui.time, width);
        s.interact(ui, r, positions, state, &mut actions);
        // Input can have changed a toggle.
        let positions = s.ensure_layout(ui.text, ui.time, width);
        s.paint(ui, r, positions);
        if copy && let Some(text) = s.selected_text() {
            ui.clipboard.set(&text);
        }
        if let Some((key, code)) = s.pending_copy.take() {
            ui.clipboard.set(&code);
            s.ui.entry(key).or_default().copied_until = ui.time + 1.4;
            ui.wake_after(1.4);
            announce(t("已复制", "Copied"));
        }
        // What it was told before this frame's layout no longer holds, and nothing else would ask for the next frame.
        if a11y_active() && s.reading.as_ref().is_some_and(|(shape, _)| *shape != s.reading_shape()) {
            repaint();
        }
    }
    // A selection in a message scrolled out of the view still answers Cmd+C, through whichever message is on screen.
    if copy
        && let Some(owner) = owner()
        && !Rc::ptr_eq(&owner, state)
        && let Some(text) = owner.try_borrow().ok().and_then(|s| s.selected_text())
    {
        ui.clipboard.set(&text);
    }
    for action in actions {
        run_action(app, action, state);
    }
}

fn run_action(app: &mut App, action: Action, state: &Rc<RefCell<MdState>>) {
    match action {
        Action::Path(f, link) => f(app, link),
        Action::Hover(f, link) => f(app, &link),
        Action::Document(f, href) => f(app, &href),
        Action::External(url) => {
            let custom = state.borrow().options.on_open_external.clone();
            match custom {
                Some(f) => f(app, &url),
                None => app.invoke_logged("open_external_url", json!({ "url": url })),
            }
        }
        Action::Reveal(f, r) => f(app, r),
    }
}

/// What is under a point in the leaf.
enum Hit {
    None,
    Chrome { key: UiKey, block: u32, region: u16 },
    Bar { key: UiKey, block: u32, region: u16 },
    Toggle { key: UiKey },
    ImageLink { block: u32, image: u32 },
    Link { block: u32, piece: u32, link: u32, rect: Rect },
    Text,
}

const CHROME_SIZE: f32 = 22.0;
const CHROME_GAP: f32 = 4.0;
const CHROME_INSET: f32 = 5.0;
const BAR_H: f32 = 10.0;

/// `.markdown-code__bar`: its copy button, top right. Rectangle relative to the leaf.
fn copy_button(region: Rect, ox: f32, oy: f32) -> Rect {
    rect(ox + region.right() - CHROME_INSET - CHROME_SIZE, oy + region.y + CHROME_INSET, CHROME_SIZE, CHROME_SIZE)
}

impl MdState {
    fn hit_at(&self, width: f32, positions: usize, x: f32, y: f32, ox: f32, oy: f32) -> Hit {
        let ys = &self.positions[positions].ys;
        if ys.is_empty() {
            return Hit::None;
        }
        let at = ys.partition_point(|top| *top <= y).saturating_sub(1);
        let block = &self.blocks[at];
        let Some(laid) = block.laid_at(width) else { return Hit::None };
        let (rx, ry) = (x, y - ys[at]);
        // A floated picture hanging over this block from one above, with a link on it.
        if self.has_floats {
            for (b, above) in self.blocks.iter().enumerate().take(at + 1) {
                let Some(above_laid) = above.laid_at(width) else { continue };
                let hung = y - ys[b];
                if let Some(image) = above_laid.images.iter().position(|image| image.link.is_some() && image.r.contains(x, hung)) {
                    return Hit::ImageLink { block: above.index, image: image as u32 };
                }
            }
        }
        if ry >= laid.height {
            return Hit::None;
        }
        // The innermost box first, for what lies over what: its controls before the scrollbar of the table it is in.
        for (i, region) in laid.regions.iter().enumerate().rev() {
            let moved = self.region_scroll(laid, region.parent);
            if !region.r.contains(rx + moved, ry) {
                continue;
            }
            if region.chrome.is_some() {
                let r = copy_button(region.r, 0.0, 0.0);
                if r.contains(rx + moved, ry) {
                    return Hit::Chrome { key: region.key, block: block.index, region: i as u16 };
                }
            }
            if region.max_scroll() > 0.0 && ry >= region.clip.bottom() - BAR_H {
                return Hit::Bar { key: region.key, block: block.index, region: i as u16 };
            }
        }
        for toggle in &laid.toggles {
            if toggle.r.contains(rx + self.region_scroll(laid, toggle.region), ry) {
                return Hit::Toggle { key: toggle.key };
            }
        }
        for (i, image) in laid.images.iter().enumerate() {
            if image.link.is_some() && image.r.contains(rx + self.region_scroll(laid, image.region), ry) {
                return Hit::ImageLink { block: block.index, image: i as u32 };
            }
        }
        // Over text, a link there or nothing.
        let from = laid.first_piece_near(ry);
        for (p, piece) in laid.pieces.iter().enumerate().skip(from) {
            if laid.past(piece, ry) {
                break;
            }
            if ry < piece.y || ry >= piece.y + piece.h() {
                continue;
            }
            let scroll = self.region_scroll(laid, piece.region);
            let (px, py) = (rx + scroll - piece.x, ry - piece.y);
            let dx = piece.line_dx_at(py, self.scale);
            for (l, link) in piece.lay.links.iter().enumerate() {
                for r in &link.rects {
                    if px - dx >= r[0] && px - dx < r[0] + r[2] && py >= r[1] && py < r[1] + r[3] {
                        let shown = rect(ox + piece.x - scroll + dx + r[0], oy + ys[at] + piece.y + r[1], r[2], r[3]);
                        return Hit::Link { block: block.index, piece: p as u32, link: l as u32, rect: shown };
                    }
                }
            }
            let (first, last) = piece.lay.doc.visible_lines(py * self.scale..py * self.scale + 0.5);
            let inside_line = piece.lay.doc.lines[first..last].iter().any(|line| {
                let line_w = line.width / self.scale;
                let line_dx = piece.dx(line_w);
                px >= line_dx && px <= line_dx + line_w + 2.0
            });
            if inside_line {
                return Hit::Text;
            }
        }
        Hit::None
    }

    fn link_action(&self, link: &Link, anchor: Rect) -> Option<Action> {
        match &link.target {
            LinkTarget::External(href) => external_http_url(href).map(Action::External),
            LinkTarget::Path { target, line } => {
                let f = self.options.on_open_path.clone()?;
                Some(Action::Path(f, PathLink { path: target.clone(), line: *line, base_dir: self.options.path_base_dir.clone(), anchor }))
            }
            LinkTarget::Document(href) => {
                let f = self.options.on_document_link.clone()?;
                Some(Action::Document(f, href.clone()))
            }
            // With no `on_reveal` there is no scroller to reach, and the link does nothing rather than move the wrong one.
            LinkTarget::Fragment(fragment) => {
                // Not in this leaf: a document made of several, as a notebook is, may have it in another.
                let Some(r) = self.fragment_rect(fragment) else {
                    let f = self.options.on_document_link.clone().filter(|_| self.options.document_links)?;
                    return Some(Action::Document(f, format!("#{fragment}")));
                };
                let f = self.options.on_reveal.clone()?;
                Some(Action::Reveal(f, r))
            }
            // `mailto:` has no handler here, and an inert link must not navigate anything.
            LinkTarget::Protocol(_) | LinkTarget::Inert => None,
        }
    }

    /// `scrollToFragment`: an element with that id, possibly behind the prefix the sanitizer gives every id, then a heading
    /// whose slug matches. Only this message is searched: two replies on screen can each have a footnote 1, and the link
    /// means the one beside it. Answers a rectangle relative to the leaf.
    fn fragment_target(&self, fragment: &str) -> Option<(f32, f32)> {
        let width = self.positions.last()?.width;
        let at = self.positions.iter().position(|p| same_width(p.width, width) && p.generation == self.generation)?;
        let ys = &self.positions[at].ys;
        let decoded = crate::path_text::decode(fragment);
        for id in [format!("user-content-{decoded}"), decoded.clone()] {
            for (b, block) in self.blocks.iter().enumerate() {
                let Some(laid) = block.laid_at(width) else { continue };
                if let Some((_, y)) = laid.anchors.iter().find(|(name, _)| *name == id) {
                    return Some((ys[b] + y, width));
                }
            }
        }
        for (b, block) in self.blocks.iter().enumerate() {
            let Some(laid) = block.laid_at(width) else { continue };
            if let Some((_, y)) = laid.headings.iter().find(|(text, _)| slug_matches(text, &decoded)) {
                return Some((ys[b] + y, width));
            }
        }
        None
    }

    fn fragment_rect(&self, fragment: &str) -> Option<Rect> {
        let (y, width) = self.fragment_target(fragment)?;
        let leaf = self.leaf_rect.get();
        Some(rect(leaf.x, leaf.y + y, width, self.options.ty.line))
    }

    fn interact(&mut self, ui: &mut Ui<'_>, r: Rect, positions: usize, this: &Rc<RefCell<MdState>>, actions: &mut Vec<Action>) {
        let width = r.w;
        self.leaf_rect.set(r);
        // `pointer_in` answers false under `block_pointer`: something lies over this leaf, or the press was captured.
        let inside = ui.pointer_in(&r);
        let (px, py) = ui.input.pointer.unwrap_or((r.x, r.y));
        let hit = if inside { self.hit_at(width, positions, px - r.x, py - r.y, r.x, r.y) } else { Hit::None };

        // Hover.
        let mut hovered_path = None;
        match &hit {
            Hit::Chrome { .. } | Hit::Toggle { .. } | Hit::ImageLink { .. } => ui.cursor = CursorKind::Hand,
            Hit::Link { block, piece, link, rect: anchor } => {
                ui.cursor = CursorKind::Hand;
                let span = &self.blocks[*block as usize].laid_at(width).expect("hit it").pieces[*piece as usize].lay.links[*link as usize];
                if let LinkTarget::Path { target, line } = &span.link.target {
                    hovered_path = Some((*block, *piece, *link, target.clone(), *line, *anchor));
                }
            }
            Hit::Text => ui.cursor = CursorKind::Text,
            Hit::Bar { .. } | Hit::None => {}
        }
        // `pre:hover`. The bar of controls is `pre`'s neighbour, not inside it, so a pointer on it is not on the code.
        if self.collapse_default {
            let at = self.positions[positions].ys.partition_point(|top| *top <= py - r.y).saturating_sub(1);
            let y0 = self.positions[positions].ys.get(at).copied().unwrap_or(0.0);
            let over_code = (inside && !matches!(hit, Hit::Chrome { .. }))
                .then(|| self.blocks.get(at).and_then(|b| b.laid_at(width)))
                .flatten()
                .and_then(|laid| laid.regions.iter().find(|g| g.chrome.as_ref().is_some_and(|c| c.can_collapse) && g.r.contains(px - r.x + self.region_scroll(laid, g.parent), py - r.y - y0)))
                .map(|g| g.key);
            if over_code != self.hovered_code {
                self.next_hovered_code = Some(over_code);
                ui.animating = true;
            }
        }
        // Prefetch once, after the dwell: the pointer crossing a link on its way somewhere asks nothing.
        match hovered_path {
            Some((block, piece, link, target, line, anchor)) => {
                let same = self.hover_path.is_some_and(|(b, p, l, ..)| (b, p, l) == (block, piece, link));
                if !same {
                    self.hover_path = Some((block, piece, link, ui.time, false));
                }
                if let Some((.., since, fired)) = &mut self.hover_path
                    && !*fired
                    && self.options.on_hover_path.is_some()
                {
                    let waited = ui.time - *since;
                    if waited >= HOVER_DWELL {
                        *fired = true;
                        let f = self.options.on_hover_path.clone().expect("checked above");
                        actions.push(Action::Hover(f, PathLink { path: target, line, base_dir: self.options.path_base_dir.clone(), anchor }));
                    } else {
                        ui.wake_after(HOVER_DWELL - waited);
                    }
                }
            }
            None => self.hover_path = None,
        }

        // Sideways wheel over a box that scrolls.
        if inside && ui.input.scroll_x != 0.0 && !ui.mem.block_pointer {
            let at = self.positions[positions].ys.partition_point(|top| *top <= py - r.y).saturating_sub(1);
            let y0 = self.positions[positions].ys.get(at).copied().unwrap_or(0.0);
            if let Some(laid) = self.blocks.get(at).and_then(|b| b.laid_at(width))
                // The innermost that has room that way, and what it hasn't room for goes to the one around it, as a wheel does anywhere.
                && let Some(region) = laid.regions.iter().rev().find(|g| {
                    let own = self.ui.get(&g.key).map_or(0.0, |u| u.scroll_x).clamp(0.0, g.max_scroll());
                    g.r.contains(px - r.x + self.region_scroll(laid, g.parent), py - r.y - y0) && (own - ui.input.scroll_x).clamp(0.0, g.max_scroll()) != own
                })
            {
                let (key, max) = (region.key, region.max_scroll());
                let entry = self.ui.entry(key).or_default();
                entry.scroll_x = (entry.scroll_x - ui.input.scroll_x).clamp(0.0, max);
            }
        }

        if ui.input.pressed {
            if inside {
                if !ui.mem.press_keeps_focus {
                    ui.mem.focus = 0;
                }
                self.press(ui, r, positions, width, px, py, &hit, this);
            } else {
                // A menu opened over the words, "Copy" for one, must be able to act on them.
                let over_overlay = overlay_rects().iter().any(|o| o.contains(px, py));
                if !over_overlay {
                    self.clear_selection();
                    // The message holding the selection may be scrolled out, and not here to hear this press itself.
                    if let Some(other) = owner()
                        && !Rc::ptr_eq(&other, this)
                        && let Ok(mut other) = other.try_borrow_mut()
                        && !other.leaf_rect.get().contains(px, py)
                    {
                        other.clear_selection();
                    }
                }
            }
        }

        if self.dragging {
            if ui.input.down {
                ui.animating = true;
                if let Some(pos) = self.locate(width, positions, px - r.x, py - r.y)
                    && let Some(mut selection) = self.selection
                {
                    let (a, b) = self.unit_at(width, pos, selection.unit);
                    selection.range = (selection.base.0.min(a), selection.base.1.max(b));
                    self.selection = Some(selection);
                }
                self.autoscroll(ui, r, positions, width, px, py, actions);
            } else {
                self.dragging = false;
            }
        }

        if let Pressed::Bar { key, block, region, grab_x, start } = self.pressed {
            ui.animating = true;
            if let Some(laid) = self.blocks.get(block as usize).and_then(|b| b.laid_at(width))
                && let Some(g) = laid.regions.get(region as usize)
            {
                let max = g.max_scroll();
                let track = g.clip.w.max(1.0);
                let per_px = g.content_w / track;
                let entry = self.ui.entry(key).or_default();
                entry.scroll_x = (start + (px - grab_x) * per_px).clamp(0.0, max);
            }
        }

        if ui.input.released {
            self.dragging = false;
            let pressed = std::mem::replace(&mut self.pressed, Pressed::None);
            self.release(ui, width, inside, &hit, pressed, actions);
        } else if !ui.input.down && !ui.input.pressed {
            self.pressed = Pressed::None;
        }
    }

    /// A drag past the edge of the view asks whoever scrolls this to follow it.
    #[allow(clippy::too_many_arguments)]
    fn autoscroll(&mut self, ui: &mut Ui<'_>, r: Rect, positions: usize, width: f32, px: f32, py: f32, actions: &mut Vec<Action>) {
        // Inside a box that scrolls sideways, the box follows first.
        if let Some(selection) = self.selection {
            let block = selection.range.1.block as usize;
            let y0 = self.positions[positions].ys.get(block).copied().unwrap_or(0.0);
            if let Some(laid) = self.blocks.get(block).and_then(|b| b.laid_at(width))
                && let Some(g) = laid.regions.iter().find(|g| g.r.y + y0 + r.y <= py && py < g.r.bottom() + y0 + r.y && g.max_scroll() > 0.0)
            {
                let (left, right) = (r.x + g.clip.x, r.x + g.clip.right());
                let step = if px < left { px - left } else if px > right { px - right } else { 0.0 };
                if step != 0.0 {
                    let (key, max) = (g.key, g.max_scroll());
                    let entry = self.ui.entry(key).or_default();
                    entry.scroll_x = (entry.scroll_x + step * 0.25).clamp(0.0, max);
                }
            }
        }
        if let Some(f) = self.options.on_reveal.clone()
            && (py < ui.clip.y || py >= ui.clip.bottom())
        {
            actions.push(Action::Reveal(f, rect(px, py - 2.0, 1.0, 4.0)));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn press(&mut self, ui: &mut Ui<'_>, r: Rect, positions: usize, width: f32, px: f32, py: f32, hit: &Hit, this: &Rc<RefCell<MdState>>) {
        // The controls have nothing to do with the text under them.
        match hit {
            Hit::Chrome { key, block, region, .. } => {
                self.pressed = Pressed::Chrome { key: *key, block: *block, region: *region };
                self.clear_selection();
                return;
            }
            Hit::Bar { key, block, region } => {
                let start = self.ui.get(key).map_or(0.0, |u| u.scroll_x);
                self.pressed = Pressed::Bar { key: *key, block: *block, region: *region, grab_x: px, start };
                return;
            }
            Hit::Toggle { key } => self.pressed = Pressed::Toggle { key: *key },
            Hit::ImageLink { block, image } => self.pressed = Pressed::ImageLink { block: *block, image: *image },
            Hit::Link { block, piece, link, .. } => self.pressed = Pressed::Link { block: *block, piece: *piece, link: *link },
            Hit::Text | Hit::None => self.pressed = Pressed::None,
        }
        // Whatever selection there was anywhere is over, and this message holds the next one.
        if let Some(other) = owner()
            && !Rc::ptr_eq(&other, this)
            && let Ok(mut other) = other.try_borrow_mut()
        {
            other.clear_selection();
        }
        fluent::el::clear_text_selection();
        OWNER.with(|o| *o.borrow_mut() = Some(Rc::downgrade(this)));

        // `double_click` says only that this press came soon after the last one, so how many it makes, and whether they were in
        // the same place, is counted here.
        let near = self.last_press.is_some_and(|(x, y, _)| (x - px).abs() < 4.0 && (y - py).abs() < 4.0);
        let previous = self.last_press.map_or(0, |(.., count)| count);
        let count = if ui.input.double_click && near { previous % 3 + 1 } else { 1 };
        self.last_press = Some((px, py, count));
        let unit = match count {
            2 => Unit::Word,
            3 => Unit::Block,
            _ => Unit::Char,
        };
        match self.locate(width, positions, px - r.x, py - r.y) {
            Some(pos) => {
                let base = self.unit_at(width, pos, unit);
                self.selection = Some(Selection { base, unit, range: base });
                self.dragging = true;
            }
            None => self.clear_selection(),
        }
    }

    fn release(&mut self, ui: &mut Ui<'_>, width: f32, inside: bool, hit: &Hit, pressed: Pressed, actions: &mut Vec<Action>) {
        let has_selection = self.selection.is_some_and(|s| !s.is_empty());
        // A plain click only: a modifier keeps its platform's meaning, as `activationClick` says.
        let modifier = ui.input.zoom_modifier;
        match (pressed, hit) {
            (Pressed::Chrome { key, block, region }, Hit::Chrome { key: k2, .. }) if inside && key == *k2 => {
                let code = self.blocks.get(block as usize).and_then(|b| b.laid_at(width)).and_then(|l| l.regions.get(region as usize)).and_then(|g| g.chrome.as_ref()).map(|c| c.code.clone());
                if let Some(code) = code {
                    ui.clipboard.set(&code);
                    self.ui.entry(key).or_default().copied_until = ui.time + 1.4;
                    ui.wake_after(1.4);
                }
            }
            (Pressed::Toggle { key }, Hit::Toggle { key: k2 }) if inside && key == *k2 && !has_selection && !modifier => {
                let open = self.blocks.get(key.0 as usize).and_then(|b| b.laid_at(width)).and_then(|l| l.toggles.iter().find(|t| t.key == key)).is_some_and(|t| t.open);
                // What is selected may be about to stop being laid out.
                self.clear_selection();
                self.toggle_ui(key, |u| u.open = Some(!open));
            }
            (Pressed::Link { block, piece, link }, Hit::Link { block: b2, piece: p2, link: l2, rect: anchor }) if inside && (block, piece, link) == (*b2, *p2, *l2) && !has_selection && !modifier => {
                let target = self.blocks[block as usize].laid_at(width).and_then(|l| l.pieces.get(piece as usize)).and_then(|p| p.lay.links.get(link as usize)).map(|s| s.link.clone());
                if let Some(target) = target
                    && let Some(action) = self.link_action(&target, *anchor)
                {
                    actions.push(action);
                }
            }
            (Pressed::ImageLink { block, image }, Hit::ImageLink { block: b2, image: i2 }) if inside && (block, image) == (*b2, *i2) && !modifier => {
                let box_ = self.blocks[block as usize].laid_at(width).and_then(|l| l.images.get(image as usize)).map(|b| (b.link.clone(), b.r));
                if let Some((Some(target), r)) = box_
                    && let Some(action) = self.link_action(&target, r)
                {
                    actions.push(action);
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ painting

    fn paint(&mut self, ui: &mut Ui<'_>, r: Rect, positions: usize) {
        let width = r.w;
        let theme = ui.theme;
        // The reveal is decoration; text must still land fully opaque without it.
        if !self.options.streaming || fluent::reduced_motion() {
            self.reveals.clear();
        }
        self.reveals.retain(|reveal| ui.time - reveal.since < REVEAL_SECONDS);
        if !self.reveals.is_empty() {
            ui.animating = true;
        }
        let selection = self.selection.filter(|s| !s.is_empty()).map(|s| s.range);
        let top = (ui.clip.y - r.y).max(0.0);
        let bottom = (ui.clip.bottom() - r.y).min(self.positions[positions].height + 1.0);
        let ys = std::mem::take(&mut self.positions[positions].ys);
        let first = ys.partition_point(|y| *y <= top).saturating_sub(1);
        let (px, py) = ui.input.pointer.unwrap_or((f32::MIN, f32::MIN));
        let pointer_inside = ui.pointer_in(&r);
        let hover_link = self.hover_link(width, &ys, pointer_inside, px - r.x, py - r.y);
        // A floated picture can hang into view from a block above.
        let first = if self.has_floats { 0 } else { first };
        for i in first..ys.len() {
            if ys[i] >= bottom {
                break;
            }
            let y0 = r.y + ys[i];
            let (ui_map, reveals, scale) = (&self.ui, &self.reveals, self.scale);
            let block = &mut self.blocks[i];
            let index = block.index;
            let Some(laid) = block.laid_at_mut(width) else { continue };
            if ys[i] + laid.floats.iter().map(|f| f.y + f.h).fold(laid.height, f32::max) <= top {
                continue;
            }
            paint_block(ui, laid, index, r.x, y0, top - ys[i], bottom - ys[i], selection, reveals, ui_map, scale, &theme, hover_link, pointer_inside, px, py);
        }
        self.positions[positions].ys = ys;
    }

    fn hover_link(&self, width: f32, ys: &[f32], inside: bool, x: f32, y: f32) -> Option<(u32, u32, u32)> {
        if !inside || ys.is_empty() {
            return None;
        }
        let at = ys.partition_point(|top| *top <= y).saturating_sub(1);
        let laid = self.blocks.get(at)?.laid_at(width)?;
        let ry = y - ys[at];
        let from = laid.first_piece_near(ry);
        for (p, piece) in laid.pieces.iter().enumerate().skip(from) {
            if laid.past(piece, ry) {
                break;
            }
            if piece.y > ry {
                continue;
            }
            let scroll = self.region_scroll(laid, piece.region);
            let (px, py) = (x + scroll - piece.x, ry - piece.y);
            let dx = piece.line_dx_at(py, self.scale);
            for (l, link) in piece.lay.links.iter().enumerate() {
                if link.rects.iter().any(|r| px - dx >= r[0] && px - dx < r[0] + r[2] && py >= r[1] && py < r[1] + r[3]) {
                    return Some((at as u32, p as u32, l as u32));
                }
            }
        }
        None
    }
}

fn glyph_clip(ui: &Ui<'_>) -> [i16; 4] {
    let s = ui.scale;
    let i = |v: f32| (v * s).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    [i(ui.clip.x), i(ui.clip.y), i(ui.clip.right()), i(ui.clip.bottom())]
}

/// The piece's text at (x, y). Aligned text goes a line at a time, each with its own offset.
fn paint_piece_text(ui: &mut Ui<'_>, piece: &mut Piece, x: f32, y: f32) {
    let s = ui.scale;
    let (px, py) = ((x * s).round(), (y * s).round());
    let clip = glyph_clip(ui);
    let top = (ui.clip.y * s).round() - py;
    let bottom = (ui.clip.bottom() * s).round() - py;
    let layer = ui.layer;
    if piece.align == TextAlign::Start {
        piece.lay.doc.paint(ui.text, &mut ui.scene.layers[layer].glyphs, px, py, clip, top..bottom);
        return;
    }
    let (a, b) = piece.lay.doc.visible_lines(top..bottom);
    for i in a..b {
        let line = piece.lay.doc.lines[i];
        let dx = (piece.dx(line.width / s) * s).round();
        piece.lay.doc.paint(ui.text, &mut ui.scene.layers[layer].glyphs, px + dx, py, clip, line.y..line.y + line.h);
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_block(ui: &mut Ui<'_>, laid: &mut Laid, index: u32, ox: f32, oy: f32, top: f32, bottom: f32, selection: Option<(Pos, Pos)>, reveals: &[Reveal], ui_map: &HashMap<UiKey, BlockUi>, scale: f32, theme: &Theme, hover_link: Option<(u32, u32, u32)>, pointer_inside: bool, px: f32, py: f32) {
    let saved_clip = ui.clip;
    // How far each box has moved what lies in it, and what of it shows: a box inside another moves and is cut off with it.
    let own: Vec<f32> = laid.regions.iter().map(|g| ui_map.get(&g.key).map_or(0.0, |u| u.scroll_x).clamp(0.0, g.max_scroll())).collect();
    let mut scrolls = own.clone();
    let mut clips: Vec<Rect> = Vec::with_capacity(laid.regions.len());
    for (i, g) in laid.regions.iter().enumerate() {
        let shown = rect(ox + g.clip.x, oy + g.clip.y, g.clip.w, g.clip.h);
        clips.push(match g.parent {
            Some(parent) => {
                let (moved, around) = (scrolls[parent as usize], clips[parent as usize]);
                scrolls[i] += moved;
                rect(shown.x - moved, shown.y, shown.w, shown.h).intersect(&around)
            }
            None => shown,
        });
    }
    let clip_of = |region: Option<u16>| region.map(|g| clips[g as usize]);

    // Boxes.
    let from = laid.decos.partition_point(|d| d.r.y + laid.max_deco_h <= top);
    for deco in &laid.decos[from..] {
        if deco.r.y >= bottom {
            break;
        }
        if deco.r.bottom() <= top {
            continue;
        }
        let dx = deco.region.map_or(0.0, |g| scrolls[g as usize]);
        let shown = rect(ox + deco.r.x - dx, oy + deco.r.y, deco.r.w, deco.r.h);
        ui.clip = clip_of(deco.region).map_or(saved_clip, |c| saved_clip.intersect(&c));
        match deco.stroke {
            Some(stroke) => ui.card(shown, deco.radius, deco.fill, stroke),
            None => ui.fill(shown, deco.radius, deco.fill),
        }
    }
    ui.clip = saved_clip;

    // A table's rules: the level ones by height, so a long table costs what is in view.
    for grid in &laid.grids {
        let region = &laid.regions[grid.region as usize];
        if region.r.bottom() < top || region.r.y > bottom {
            continue;
        }
        let dx = scrolls[grid.region as usize];
        ui.clip = saved_clip.intersect(&clips[grid.region as usize]);
        let from = grid.level.partition_point(|rule| rule[1] < top - 1.0);
        for rule in &grid.level[from..] {
            if rule[1] > bottom {
                break;
            }
            ui.fill(rect(ox + rule[0] - dx, oy + rule[1], rule[2], 1.0), 0.0, grid.color);
        }
        for rule in &grid.upright {
            let (y0, y1) = (rule[1].max(top), (rule[1] + rule[2]).min(bottom));
            if y1 > y0 {
                ui.fill(rect(ox + rule[0] - dx, oy + y0, 1.0, y1 - y0), 0.0, grid.color);
            }
        }
    }
    ui.clip = saved_clip;

    // Markers.
    let from = laid.markers.partition_point(|m| m.y + 40.0 <= top);
    for marker in &laid.markers[from..] {
        if marker.y >= bottom {
            break;
        }
        ui.clip = clip_of(marker.region).map_or(saved_clip, |c| saved_clip.intersect(&c));
        paint_marker(ui, marker, ox - marker.region.map_or(0.0, |g| scrolls[g as usize]), oy, theme);
    }

    for image in &laid.images {
        if image.r.bottom() <= top || image.r.y >= bottom {
            continue;
        }
        ui.clip = clip_of(image.region).map_or(saved_clip, |c| saved_clip.intersect(&c));
        let shown = rect(ox + image.r.x - image.region.map_or(0.0, |g| scrolls[g as usize]), oy + image.r.y, image.r.w, image.r.h);
        ui.image(image.image.id, shown, 8.0, 1.0);
    }
    ui.clip = saved_clip;

    // Text.
    let from = laid.first_piece_near(top);
    let unsorted = laid.unsorted;
    let mut rects: Vec<[f32; 4]> = Vec::new();
    for (p, piece) in laid.pieces.iter_mut().enumerate().skip(from) {
        if piece.y >= bottom {
            if unsorted {
                continue;
            }
            break;
        }
        if piece.y + piece.h() <= top {
            continue;
        }
        let scroll = piece.region.map_or(0.0, |g| scrolls[g as usize]);
        let (x, y) = (ox + piece.x - scroll, oy + piece.y);
        ui.clip = clip_of(piece.region).map_or(saved_clip, |c| saved_clip.intersect(&c));
        if ui.clip.w <= 0.0 || ui.clip.h <= 0.0 || x > ui.clip.right() || x + piece.lay.natural_w.max(piece.w) < ui.clip.x {
            continue;
        }
        let aligned = piece.align != TextAlign::Start;

        // Inline boxes and rules.
        for span in &piece.lay.spans {
            let [sx, sy, sw, sh] = span.r;
            if piece.y + sy + sh <= top || piece.y + sy >= bottom {
                continue;
            }
            let dx = if aligned { piece.line_dx_at(sy + sh * 0.5, scale) } else { 0.0 };
            let shown = rect(x + dx + sx, y + sy, sw, sh);
            match span.kind {
                SpanKind::Code => ui.fill(shown, 4.0, span.color),
                SpanKind::Kbd => {
                    ui.card(shown, 4.0, span.color, theme.control_strong_stroke);
                    // `border-bottom-width: 2px`.
                    ui.fill(rect(shown.x + 1.0, shown.bottom() - 2.0, (shown.w - 2.0).max(0.0), 1.0), 0.0, theme.control_strong_stroke);
                }
                SpanKind::Mark => ui.fill(shown, 3.0, span.color),
                SpanKind::Line => ui.fill(shown, 0.0, span.color),
                SpanKind::Dotted => {
                    let (from, to) = (shown.x.max(ui.clip.x), shown.right().min(ui.clip.right()));
                    let mut at = shown.x + ((from - shown.x) / 3.0).floor().max(0.0) * 3.0;
                    while at < to {
                        ui.fill(rect(at, shown.y, 1.0, 1.0), 0.0, span.color);
                        at += 3.0;
                    }
                }
            }
        }
        for (l, link) in piece.lay.links.iter().enumerate() {
            let hovered = hover_link == Some((index, p as u32, l as u32));
            let color = if hovered { link.color } else { link.color.fade(0.35) };
            for r in &link.rects {
                if piece.y + r[1] + r[3] <= top || piece.y + r[1] >= bottom {
                    continue;
                }
                let dx = if aligned { piece.line_dx_at(r[1] + r[3] * 0.5, scale) } else { 0.0 };
                ui.fill(rect(x + dx + r[0], y + r[1] + link.under, r[2], 1.0), 0.0, color);
            }
        }

        // Selection, over the boxes and behind the text.
        let here = |byte: u32| Pos { block: index, piece: p as u32, byte };
        let len = piece.lay.doc.text().len() as u32;
        if let Some((lo, hi)) = selection {
            let a = lo.max(here(0));
            let z = hi.min(here(len));
            if a < z {
                rects.clear();
                let range = a.byte as usize..z.byte as usize;
                let view = ((top - piece.y) * scale)..((bottom - piece.y) * scale);
                piece.lay.doc.selection_rects(range, view, &mut rects);
                for [sx, sy, sw, sh] in rects.drain(..) {
                    let dx = if aligned { line_offset(piece, sy, scale) } else { 0.0 };
                    ui.fill(rect(x + dx + sx / scale, y + sy / scale, sw / scale, sh / scale), 2.0, theme.selection);
                }
            }
        }

        let layer = ui.layer;
        let mark = ui.scene.layers[layer].glyphs.len();
        paint_piece_text(ui, piece, x, y);
        if !piece.lay.shifts.is_empty() {
            shift_glyphs(&mut ui.scene.layers[layer].glyphs[mark..], piece, x, y, scale);
        }
        if piece.fade
            && let Some(reveal) = reveals.iter().find(|reveal| (reveal.start.block, reveal.start.piece) == (index, p as u32))
        {
            let time = ui.time;
            sweep_glyphs(&mut ui.scene.layers[layer].glyphs[mark..], piece, reveal, time, x, y, scale, aligned);
        }
        // After the sweep: a formula is not swept in, as upstream's renderer leaves it out.
        paint_formulas(ui, piece, x, y, top - piece.y, bottom - piece.y, scale);
    }
    ui.clip = saved_clip;

    // The corner of controls and a sideways scrollbar lie over the text, and a layer draws its text over its boxes.
    let saved_layer = ui.layer;
    ui.layer += 1;
    for (i, region) in laid.regions.iter().enumerate() {
        if region.r.bottom() <= top || region.r.y >= bottom {
            continue;
        }
        let moved = scrolls[i] - own[i];
        let shown = rect(ox + region.r.x - moved, oy + region.r.y, region.r.w, region.r.h);
        let around = region.parent.map_or(saved_clip, |parent| saved_clip.intersect(&clips[parent as usize]));
        let hovered = pointer_inside && shown.contains(px, py) && around.contains(px, py);
        ui.clip = around.intersect(&shown);
        if hovered && let Some(chrome) = &region.chrome {
            let state = ui_map.get(&region.key).copied().unwrap_or_default();
            paint_chrome(ui, chrome, shown, state, theme, px, py);
        }
        if region.max_scroll() > 0.0 && hovered {
            let track = rect(shown.x + 1.0, shown.bottom() - 7.0, (shown.w - 2.0).max(1.0), 4.0);
            let thumb_w = (track.w * region.clip.w / region.content_w).max(24.0).min(track.w);
            let at = own[i] / region.max_scroll().max(1.0);
            ui.fill(rect(track.x + (track.w - thumb_w) * at, track.y, thumb_w, track.h), 2.0, theme.text_tertiary.fade(0.6));
        }
    }
    ui.layer = saved_layer;
    ui.clip = saved_clip;
}

/// The formulas of a piece that are in view. `top` and `bottom` are relative to the piece.
fn paint_formulas(ui: &mut Ui<'_>, piece: &Piece, x: f32, y: f32, top: f32, bottom: f32, scale: f32) {
    for inline in &piece.lay.formulas {
        let Some(placed) = piece.lay.doc.boxes().get(inline.at) else { continue };
        let math_layout::Typeset::Ready(formula) = &*inline.formula else { continue };
        let baseline = placed.baseline / scale;
        if baseline + formula.d <= top || baseline - formula.h >= bottom {
            continue;
        }
        let dx = piece.line_dx_at(baseline, scale);
        formula.paint(ui, x + dx + placed.x / scale, y + baseline, inline.full, inline.size, inline.color);
    }
    for ruby in &piece.lay.rubies {
        let Some(placed) = piece.lay.doc.boxes().get(ruby.at) else { continue };
        let baseline = placed.baseline / scale;
        let line_top = baseline - ui.text.line(&ruby.base, &ruby.base_ty, ruby.color).baseline() / scale;
        if line_top + ruby.base_ty.line <= top || line_top - ruby.reading_ty.line >= bottom {
            continue;
        }
        let left = x + piece.line_dx_at(baseline, scale) + placed.x / scale;
        let centred = |ui: &mut Ui<'_>, text: &str, ty: &TypeStyle| left + (ruby.full - ui.measure(text, ty)) * 0.5;
        let base_x = centred(ui, &ruby.base, &ruby.base_ty);
        ui.label(base_x, y + line_top, &ruby.base, &ruby.base_ty, ruby.color);
        let reading_x = centred(ui, &ruby.reading, &ruby.reading_ty);
        ui.label(reading_x, y + line_top - ruby.reading_ty.line, &ruby.reading, &ruby.reading_ty, ruby.color);
    }
}

fn line_offset(piece: &Piece, line_y_phys: f32, scale: f32) -> f32 {
    piece.line_dx_at((line_y_phys + 0.5) / scale, scale)
}

fn paint_marker(ui: &mut Ui<'_>, marker: &layout::Marker, ox: f32, oy: f32, theme: &Theme) {
    let (x, y) = (ox + marker.x, oy + marker.y);
    let line = marker.ty.line;
    // A list's marker sits outside its text, about a space off it.
    let dot = rect(x - marker.ty.size * 0.9, y + line * 0.5 - 2.5, 5.0, 5.0);
    match &marker.kind {
        layout::MarkerKind::Disc => ui.fill(dot, 2.5, marker.color),
        layout::MarkerKind::Circle => ui.card(dot, 2.5, Color::TRANSPARENT, marker.color),
        layout::MarkerKind::Square => ui.fill(dot, 0.0, marker.color),
        layout::MarkerKind::Number(label) => ui.label_right(x - marker.ty.size * 0.35, y, label, &marker.ty, marker.color),
        layout::MarkerKind::Check(done) => {
            let r = rect(x, y, 13.0, 13.0);
            if *done {
                ui.fill(r, 3.0, theme.accent);
                ui.named_icon("Check", r.x + 6.5, r.y + 6.5, 10.0, theme.text_on_accent, 0.0);
            } else {
                ui.card(r, 3.0, Color::TRANSPARENT, theme.control_strong_stroke);
            }
        }
        layout::MarkerKind::Disclosure(open) => {
            ui.named_icon("ChevronRight", x + 6.0, y + line * 0.5, 12.0, marker.color, if *open { 90.0 } else { 0.0 });
        }
    }
}

fn paint_chrome(ui: &mut Ui<'_>, chrome: &layout::Chrome, region: Rect, state: BlockUi, theme: &Theme, px: f32, py: f32) {
    let r = copy_button(rect(0.0, 0.0, region.w, region.h), region.x, region.y);
    let hovered = r.contains(px, py);
    let fill = if hovered { theme.subtle_hover.mix(theme.code_bg, 0.2) } else { theme.code_bg };
    ui.fill(r, 5.0, fill);
    let color = if hovered { theme.text_primary } else { theme.text_secondary };
    let copied = state.copied_until > ui.time;
    ui.named_icon(if copied { "Check" } else { "Copy" }, r.x + r.w * 0.5, r.y + r.h * 0.5, 12.0, color, 0.0);
    if copied {
        ui.wake_after(state.copied_until - ui.time);
    }
    if let Some(label) = &chrome.label {
        // `.markdown-code__language`: 10.5px/20px mono, padding 0 4px.
        let ty = TypeStyle { size: 10.5, line: 20.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
        let w = ui.measure(label, &ty);
        ui.label(r.x - CHROME_GAP - 4.0 - w, region.y + CHROME_INSET + 1.0, label, &ty, theme.text_tertiary);
    }
}

/// `<sup>` and `<sub>`: the glyphs of those runs move off the baseline. A document has one baseline to a line.
fn shift_glyphs(glyphs: &mut [GlyphInst], piece: &Piece, x: f32, y: f32, scale: f32) {
    for g in glyphs {
        let cx = (g.pos[0] + g.size[0] as f32 * 0.5) / scale - x;
        let cy = (g.pos[1] + g.size[1] as f32 * 0.5) / scale - y;
        let dx = piece.line_dx_at(cy, scale);
        for shift in &piece.lay.shifts {
            let [sx, sy, sw, sh] = shift.r;
            if cx - dx >= sx && cx - dx < sx + sw && cy >= sy - 2.0 && cy < sy + sh + 2.0 {
                g.pos[1] += shift.dy * scale;
                break;
            }
        }
    }
}

/// The streaming reveal, `.stream-fresh`'s mask: a gradient over a glyph at a time, since a mask needs something to mask. The edge runs
/// along the new text's line fragments laid end to end, so the sweep follows it onto the next line. Whether a glyph is new is decided by
/// where it is against those fragments, since a document does not keep which byte a glyph came from.
#[allow(clippy::too_many_arguments)]
fn sweep_glyphs(glyphs: &mut [GlyphInst], piece: &Piece, reveal: &Reveal, time: f32, x: f32, y: f32, scale: f32, aligned: bool) {
    // Hidden through its delay, shown for good once its turn is over.
    let progress = ((time - reveal.since - reveal.delay) / reveal.duration.max(0.001)).clamp(0.0, 1.0);
    if progress >= 1.0 {
        return;
    }
    let mut fragments = Vec::new();
    piece.lay.doc.selection_rects(reveal.start.byte as usize..piece.lay.doc.text().len(), 0.0..f32::MAX, &mut fragments);
    if aligned {
        for fragment in &mut fragments {
            fragment[0] += line_offset(piece, fragment[1], scale) * scale;
        }
    }
    let width: f32 = fragments.iter().map(|fragment| fragment[2]).sum();
    for g in glyphs {
        let gx = g.pos[0] + g.size[0] as f32 * 0.5 - (x * scale).round();
        let gy = g.pos[1] + g.size[1] as f32 * 0.5 - (y * scale).round();
        let mut along = 0.0;
        for [fx, fy, fw, fh] in &fragments {
            if gy >= *fy && gy < fy + fh {
                if gx >= fx - 1.0 {
                    let alpha = fresh_alpha(along + gx - fx, width, reveal.em * scale, progress);
                    if alpha < 1.0 {
                        g.color = g.color.fade(alpha);
                    }
                }
                break;
            }
            along += fw;
        }
    }
}

// ---------------------------------------------------------------- what tests look at

/// Looks into retained state, for tests. Not for views.
#[doc(hidden)]
pub mod probe {
    use super::*;

    fn with_state<R>(key: impl Hash, f: impl FnOnce(&MdState) -> R) -> Option<R> {
        let id = markdown_key(key);
        let state = TABLE.with(|t| t.borrow().entries.get(&id).map(|(s, _)| s.clone()))?;
        let s = state.borrow();
        Some(f(&s))
    }

    /// How many runs of text have been shaped on this thread so far.
    pub fn has_reading(key: impl Hash) -> bool {
        with_state(key, |s| s.reading.is_some()).unwrap_or(false)
    }

    pub fn shaped_count() -> u64 {
        layout::shaped_count()
    }

    pub fn block_count(key: impl Hash) -> usize {
        with_state(key, |s| s.blocks.len()).unwrap_or(0)
    }

    pub fn block_hashes(key: impl Hash) -> Vec<u64> {
        with_state(key, |s| s.blocks.iter().map(|b| b.hash).collect()).unwrap_or_default()
    }

    /// Text of every piece of a message, in reading order.
    pub fn piece_texts(key: impl Hash) -> Vec<String> {
        with_state(key, |s| s.blocks.iter().filter_map(|b| b.any_laid()).flat_map(|l| l.pieces.iter().map(|p| p.lay.doc.text().to_owned())).collect()).unwrap_or_default()
    }

    /// The text still sweeping in, joined across pieces.
    pub fn fresh_text(key: impl Hash) -> Vec<String> {
        with_state(key, |s| {
            let Some(start) = s.reveals.first().map(|r| r.start) else { return Vec::new() };
            let mut out = Vec::new();
            for block in &s.blocks[start.block as usize..] {
                let Some(laid) = block.any_laid() else { continue };
                for (p, piece) in laid.pieces.iter().enumerate() {
                    let here = Pos { block: block.index, piece: p as u32, byte: 0 };
                    if !piece.fade || (here.block, here.piece) < (start.block, start.piece) {
                        continue;
                    }
                    let from = if (here.block, here.piece) == (start.block, start.piece) { start.byte as usize } else { 0 };
                    out.push(piece.lay.doc.text()[from..].to_owned());
                }
            }
            out
        })
        .unwrap_or_default()
    }

    /// Height for the last width the message was laid out at.
    pub fn height(key: impl Hash) -> f32 {
        with_state(key, |s| s.last_height).unwrap_or(0.0)
    }

    pub fn entry_count() -> usize {
        TABLE.with(|t| t.borrow().entries.len())
    }

    /// Top of each heading, relative to the message, with its text.
    pub fn headings(key: impl Hash) -> Vec<(String, f32)> {
        with_state(key, |s| {
            let Some(pos) = s.positions.last() else { return Vec::new() };
            let mut out = Vec::new();
            for (b, block) in s.blocks.iter().enumerate() {
                if let Some(laid) = block.laid_at(pos.width) {
                    out.extend(laid.headings.iter().map(|(t, y)| (t.clone(), pos.ys.get(b).copied().unwrap_or(0.0) + y)));
                }
            }
            out
        })
        .unwrap_or_default()
    }

    /// Where the middle of a byte of a piece is, relative to the message: what a test points at.
    pub fn point_of(key: impl Hash, block: usize, piece: usize, byte: usize) -> Option<(f32, f32)> {
        with_state(key, |s| {
            let pos = s.positions.last()?;
            let laid = s.blocks.get(block)?.laid_at(pos.width)?;
            let p = laid.pieces.get(piece)?;
            let (x, y, h) = p.lay.doc.caret_pos(byte);
            let dx = p.dx(p.lay.doc.lines.first().map_or(0.0, |l| l.width / s.scale));
            Some((p.x + dx + x / s.scale + 0.5, pos.ys.get(block)? + p.y + (y + h * 0.5) / s.scale))
        })
        .flatten()
    }

    /// The rectangle of a code block or table, relative to the message.
    pub fn region_rect(key: impl Hash, block: usize, region: usize) -> Option<Rect> {
        with_state(key, |s| {
            let pos = s.positions.last()?;
            let g = s.blocks.get(block)?.laid_at(pos.width)?.regions.get(region)?;
            Some(rect(g.r.x, pos.ys.get(block)? + g.r.y, g.r.w, g.r.h))
        })
        .flatten()
    }

    /// Where the sideways scroll of a code block or table is, and as far as it goes.
    pub fn region_scroll(key: impl Hash, block: usize, region: usize) -> Option<(f32, f32)> {
        with_state(key, |s| {
            let pos = s.positions.last()?;
            let laid = s.blocks.get(block)?.laid_at(pos.width)?;
            let g = laid.regions.get(region)?;
            Some((s.region_scroll(laid, Some(region as u16)), g.max_scroll()))
        })
        .flatten()
    }

    /// Each piece's lines in reading order: where the line starts, across and down, and how wide it is, relative to the message.
    pub fn line_rects(key: impl Hash) -> Vec<Rect> {
        with_state(key, |s| {
            let Some(pos) = s.positions.last() else { return Vec::new() };
            let mut out = Vec::new();
            for (b, block) in s.blocks.iter().enumerate() {
                let (Some(y), Some(laid)) = (pos.ys.get(b).copied(), block.laid_at(pos.width)) else { continue };
                for piece in &laid.pieces {
                    for line in &piece.lay.doc.lines {
                        out.push(rect(piece.x + line.indent / s.scale, y + piece.y + line.y / s.scale, (line.width - line.indent) / s.scale, line.h / s.scale));
                    }
                }
            }
            out
        })
        .unwrap_or_default()
    }

    /// Each ruby in reading order: its base, its reading, and the left edge and baseline of the box they stand in.
    pub fn placed_rubies(key: impl Hash) -> Vec<(String, String, f32, f32)> {
        with_state(key, |s| {
            let Some(pos) = s.positions.last() else { return Vec::new() };
            let mut out = Vec::new();
            for (b, block) in s.blocks.iter().enumerate() {
                let (Some(y), Some(laid)) = (pos.ys.get(b).copied(), block.laid_at(pos.width)) else { continue };
                for piece in &laid.pieces {
                    for ruby in &piece.lay.rubies {
                        if let Some(placed) = piece.lay.doc.boxes().get(ruby.at) {
                            out.push((ruby.base.clone(), ruby.reading.clone(), piece.x + placed.x / s.scale, y + piece.y + placed.baseline / s.scale));
                        }
                    }
                }
            }
            out
        })
        .unwrap_or_default()
    }

    /// A box's sideways scroll by itself, without the boxes it lies in.
    pub fn region_scroll_own(key: impl Hash, block: usize, region: usize) -> Option<f32> {
        with_state(key, |s| {
            let pos = s.positions.last()?;
            let g = s.blocks.get(block)?.laid_at(pos.width)?.regions.get(region)?;
            Some(s.ui.get(&g.key).map_or(0.0, |u| u.scroll_x).clamp(0.0, g.max_scroll()))
        })
        .flatten()
    }

    /// Where a code block's copy button is, relative to the message.
    pub fn copy_button(key: impl Hash, block: usize, region: usize) -> Option<Rect> {
        with_state(key, |s| {
            let pos = s.positions.last()?;
            let g = s.blocks.get(block)?.laid_at(pos.width)?.regions.get(region)?;
            g.chrome.as_ref()?;
            Some(super::copy_button(g.r, 0.0, *pos.ys.get(block)?))
        })
        .flatten()
    }

    /// What stands beside a message's list items, in order: a number as it reads, or the name of the mark.
    pub fn list_marks(key: impl Hash) -> Vec<String> {
        with_state(key, |s| {
            s.blocks.iter().filter_map(|b| b.any_laid()).flat_map(|l| l.markers.iter()).map(|m| match &m.kind {
                layout::MarkerKind::Disc => "disc".to_owned(),
                layout::MarkerKind::Circle => "circle".to_owned(),
                layout::MarkerKind::Square => "square".to_owned(),
                layout::MarkerKind::Number(label) => label.to_string(),
                layout::MarkerKind::Check(done) => format!("check {done}"),
                layout::MarkerKind::Disclosure(open) => format!("disclosure {open}"),
            }).collect()
        })
        .unwrap_or_default()
    }

    /// Every picture a message draws, relative to its top block's top.
    pub fn images(key: impl Hash) -> Vec<Rect> {
        with_state(key, |s| {
            let Some(pos) = s.positions.last() else { return Vec::new() };
            let mut out = Vec::new();
            for (b, block) in s.blocks.iter().enumerate() {
                let Some(laid) = block.laid_at(pos.width) else { continue };
                let y0 = pos.ys.get(b).copied().unwrap_or(0.0);
                out.extend(laid.images.iter().map(|i| rect(i.r.x, y0 + i.r.y, i.r.w, i.r.h)));
            }
            out
        })
        .unwrap_or_default()
    }

    /// Every piece of a message with where it is, relative to the message.
    pub fn pieces(key: impl Hash) -> Vec<(String, f32, f32)> {
        with_state(key, |s| {
            let Some(pos) = s.positions.last() else { return Vec::new() };
            let mut out = Vec::new();
            for (b, block) in s.blocks.iter().enumerate() {
                let Some(laid) = block.laid_at(pos.width) else { continue };
                let y0 = pos.ys.get(b).copied().unwrap_or(0.0);
                out.extend(laid.pieces.iter().map(|p| (p.lay.doc.text().to_owned(), p.x, y0 + p.y)));
            }
            out
        })
        .unwrap_or_default()
    }

    /// A table's rules, relative to its block: the level ones as x, y, width, and the upright ones as x, y, height.
    pub fn table_rules(key: impl Hash, block: usize) -> Option<(Vec<[f32; 3]>, Vec<[f32; 3]>)> {
        with_state(key, |s| {
            let pos = s.positions.last()?;
            let grid = s.blocks.get(block)?.laid_at(pos.width)?.grids.first()?;
            Some((grid.level.clone(), grid.upright.clone()))
        })
        .flatten()
    }

    /// A formula's tree, as text a test can look through, or why it has none.
    pub fn tex_tree(source: &str) -> Result<String, String> {
        tex::parse(source).map(|nodes| format!("{nodes:?}"))
    }

    /// Every name MathJax's loaded packages define, a line each: package, kind, name.
    pub const MATHJAX_COMMANDS: &str = tex::COMMANDS;

    pub const MATH_CACHE_LIMIT: usize = math_layout::CACHE_LIMIT;

    /// How many formulas have been set on this thread, rather than found, and how many are being kept.
    pub fn math_cache() -> (u64, usize) {
        math_layout::cache_stats()
    }

    /// Where a display formula goes in a column this wide: the start of the formula, the start of its tags, and how wide its box is.
    pub fn formula_in_column(text: &mut TextCtx, source: &str, size: f32, column: f32) -> Option<(f32, f32, f32)> {
        let typeset = math_layout::typeset(text, source, true, size);
        let math_layout::Typeset::Ready(formula) = &*typeset else { return None };
        let (at, tags) = formula.tag_layout(column, size);
        Some((at, tags, formula.full_width(column, size)))
    }

    /// A `multline`'s rows, each as its width and the share of the free room that goes before it.
    pub fn formula_spread(text: &mut TextCtx, source: &str, size: f32) -> Vec<(f32, f32)> {
        let typeset = math_layout::typeset(text, source, true, size);
        let math_layout::Typeset::Ready(formula) = &*typeset else { return Vec::new() };
        formula.spread.iter().map(|row| (row.w, row.free)).collect()
    }

    /// A formula's width, height and depth at a text size, in logical pixels, or why it has none.
    pub fn formula_metrics(text: &mut TextCtx, source: &str, display: bool, size: f32) -> Result<(f32, f32, f32), String> {
        match &*math_layout::typeset(text, source, display, size) {
            math_layout::Typeset::Ready(formula) => Ok((formula.w, formula.h, formula.d)),
            math_layout::Typeset::Error(message) => Err(message.to_string()),
        }
    }

    /// A formula's runs of text: what each says, where it starts on its baseline, its size, and the colour it asked for.
    pub fn formula_runs(text: &mut TextCtx, source: &str, display: bool, size: f32) -> Vec<(String, f32, f32, f32, Option<Color>)> {
        let typeset = math_layout::typeset(text, source, display, size);
        let math_layout::Typeset::Ready(formula) = &*typeset else { return Vec::new() };
        formula.items.iter().filter_map(|item| match item {
            math_layout::Item::Run { text, size, x, y, color, .. } => Some((text.to_string(), *x, *y, *size, *color)),
            _ => None,
        }).collect()
    }

    /// A formula's rules, as x, y, width and height.
    pub fn formula_rules(text: &mut TextCtx, source: &str, display: bool, size: f32) -> Vec<[f32; 4]> {
        let typeset = math_layout::typeset(text, source, display, size);
        let math_layout::Typeset::Ready(formula) = &*typeset else { return Vec::new() };
        formula.items.iter().filter_map(|item| match item {
            math_layout::Item::Rule { x, y, w, h, .. } => Some([*x, *y, *w, *h]),
            _ => None,
        }).collect()
    }

    /// How many things a formula draws as paths, with their rectangles' sizes in pixels.
    pub fn formula_paths(text: &mut TextCtx, source: &str, display: bool, size: f32) -> Vec<(u32, u32)> {
        let typeset = math_layout::typeset(text, source, display, size);
        let math_layout::Typeset::Ready(formula) = &*typeset else { return Vec::new() };
        formula.items.iter().filter_map(|item| match item {
            math_layout::Item::Path { px, .. } => Some(*px),
            _ => None,
        }).collect()
    }

    /// Every piece's formulas, in reading order: the left edge and baseline of each, relative to the message, and its size.
    pub fn placed_formulas(key: impl Hash) -> Vec<(f32, f32, f32, f32, f32)> {
        with_state(key, |s| {
            let Some(pos) = s.positions.last() else { return Vec::new() };
            let mut out = Vec::new();
            for (b, block) in s.blocks.iter().enumerate() {
                let Some(laid) = block.laid_at(pos.width) else { continue };
                let y0 = pos.ys.get(b).copied().unwrap_or(0.0);
                for piece in &laid.pieces {
                    for inline in &piece.lay.formulas {
                        let Some(placed) = piece.lay.doc.boxes().get(inline.at) else { continue };
                        if let math_layout::Typeset::Ready(formula) = &*inline.formula {
                            out.push((piece.x + placed.x / s.scale, y0 + piece.y + placed.baseline / s.scale, formula.w, formula.h, formula.d));
                        }
                    }
                }
            }
            out
        })
        .unwrap_or_default()
    }

    /// The reasons shown on hovering formulas that did not parse, in reading order.
    pub fn math_notes(key: impl Hash) -> Vec<String> {
        with_state(key, |s| s.blocks.iter().filter_map(|b| b.any_laid()).flat_map(|l| l.pieces.iter()).flat_map(|p| p.lay.notes.iter().map(|n| n.message.to_string())).collect()).unwrap_or_default()
    }

    /// Where the disclosure of a `<details>` is, relative to the message.
    pub fn toggle_rect(key: impl Hash, block: usize, index: usize) -> Option<Rect> {
        with_state(key, |s| {
            let pos = s.positions.last()?;
            let g = s.blocks.get(block)?.laid_at(pos.width)?.toggles.get(index)?;
            Some(rect(g.r.x, pos.ys.get(block)? + g.r.y, g.r.w, g.r.h))
        })
        .flatten()
    }
}
