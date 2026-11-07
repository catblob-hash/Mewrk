//! What stands in for MathJax's output: `tex::parse`'s tree set as TeX sets it (the rules of appendix G of The TeXbook, with
//! plain TeX's font parameters), and a list of things to draw.
//!
//! Written here rather than taken from a crate: the pure-Rust ones are a typesetting system's worth of dependencies, or they
//! need a font of their own bundled. This one needs a run of text, a filled rectangle and a filled path. Letters come from the
//! system's math face when it has one, whose mathematical alphabets say by their code points what `\mathbf` or `\mathbb` mean;
//! without one, from the interface font, slanted and emboldened by hand. What a font has no glyph for at the size wanted, a
//! bracket as tall as a matrix, a brace over a sum, is drawn as a path.
//!
//! Heights come from what a glyph really draws rather than from a font's tables, which swash does not read: a superscript clears
//! the actual top of its letter and `ink` is what italic correction is worked out from. A formula holds no colour unless it asked
//! for one, so one laid out once follows the theme, and it is kept by source and size as `lib/mathJax.ts` keeps its results.

use super::tex::{self, AccentKind, Array, ArrowHead, BBox, Border, CdArrow, Class, Enclose, Frac, LineStyle, MathFont, MathStyle, Node, Notation, OverKind, Proof, ProofLine, VAlign, XArrow};
use fluent::gpu::GlyphInst;
use fluent::text::{hash_str, TextCtx};
use fluent::theme::Color;
use fluent::ui::{rect, Ui};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write;
use std::rc::Rc;
use std::sync::Arc;

/// `CACHE_LIMIT` in `lib/mathJax.ts`.
pub(super) const CACHE_LIMIT: usize = 4000;
/// Past this a drawn path would not fit the atlas's rule for them, and a shape goes back to straight rules.
const MAX_PATH_PX: f32 = 1900.0;
const SCRIPT_SCALE: f32 = 0.7;
const SCRIPT_SCRIPT_SCALE: f32 = 0.5;
/// Plain TeX's `\fontdimen`s, in `em`s: where scripts and fractions go.
const NUM1: f32 = 0.677;
const NUM2: f32 = 0.394;
const NUM3: f32 = 0.444;
const DENOM1: f32 = 0.686;
const DENOM2: f32 = 0.345;
const SUP1: f32 = 0.413;
const SUP2: f32 = 0.363;
const SUP3: f32 = 0.289;
const SUB1: f32 = 0.15;
const SUB2: f32 = 0.247;
const SUP_DROP: f32 = 0.386;
const SUB_DROP: f32 = 0.05;
const DELIM1: f32 = 2.39;
const DELIM2: f32 = 1.01;
const RULE: f32 = 0.05;
const BIG_OP_SPACING: [f32; 5] = [0.111, 0.167, 0.2, 0.6, 0.1];
const NULL_DELIMITER: f32 = 0.12;
const SCRIPT_SPACE: f32 = 0.05;
/// How much larger than its text-size glyph a large operator is in display. A font's own display forms would need its `MATH`
/// table, and scaling is not far from them.
const DISPLAY_OP: f32 = 1.4;
const DISPLAY_INTEGRAL: f32 = 1.75;

#[derive(Clone, Debug)]
pub(super) enum Item {
    Run { text: Rc<str>, size: f32, bold: bool, italic: bool, x: f32, y: f32, color: Option<Color> },
    Rule { x: f32, y: f32, w: f32, h: f32, color: Option<Color> },
    /// `data` is in physical pixels from (x, y), which is this path's top left.
    Path { data: Rc<str>, x: f32, y: f32, px: (u32, u32), color: Option<Color> },
}

impl Item {
    fn shift(&mut self, dx: f32, dy: f32) {
        match self {
            Item::Run { x, y, .. } | Item::Rule { x, y, .. } | Item::Path { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
        }
    }
}

/// A formula, ready to draw. `h` above the baseline, `d` below it; `items` from the left end of the baseline, y going down.
#[derive(Debug)]
pub(super) struct Formula {
    pub w: f32,
    pub h: f32,
    pub d: f32,
    pub items: Vec<Item>,
    /// `\tag`s: over at the right of the column, however wide that turns out to be. From the left of their own column.
    pub tags: Vec<Item>,
    pub tag_w: f32,
    /// A `multline`'s rows: each set against its side of the column, which is what their `items` are all about.
    pub spread: Vec<SpreadRow>,
}

/// One row of a `multline`, and how much of the room it does not use goes before it.
#[derive(Debug)]
pub(super) struct SpreadRow {
    pub w: f32,
    pub free: f32,
    pub items: Vec<Item>,
}

/// `multlineIndent`: how far the first and last rows of a `multline` stand off the sides.
const MULTLINE_INDENT: f32 = 1.0;
/// `tagIndent`, as the least room between a formula and its tag.
const TAG_GAP: f32 = 0.8;

/// `MathRender`.
#[derive(Debug)]
pub(super) enum Typeset {
    Ready(Formula),
    Error(Arc<str>),
}

impl Formula {
    /// How far along a box `avail` wide the formula starts and its tags do: centred as `displayAlign` has it, its tags at the right
    /// and clear of it. Past that the box has to be wider, and the box says so through `full_width`.
    pub fn tag_layout(&self, avail: f32, size: f32) -> (f32, f32) {
        if self.tags.is_empty() {
            return (0.0, 0.0);
        }
        let gap = TAG_GAP * size;
        let wide = avail.max(self.w + gap + self.tag_w);
        let at = ((wide - self.w) * 0.5).min(wide - self.w - gap - self.tag_w).max(0.0);
        (at, wide - self.tag_w)
    }

    /// As wide as a formula is laid out in a column of this width.
    pub fn full_width(&self, avail: f32, size: f32) -> f32 {
        if !self.spread.is_empty() {
            return avail.max(self.w + self.tag_w + if self.tags.is_empty() { 0.0 } else { TAG_GAP * size });
        }
        if self.tags.is_empty() { self.w } else { avail.max(self.w + TAG_GAP * size + self.tag_w) }
    }

    /// Draws at (x, baseline) in a box `full` wide. A rule lands on whole pixels, as a run of glyphs does: a fraction bar should be crisp.
    pub fn paint(&self, ui: &mut Ui<'_>, x: f32, baseline: f32, full: f32, size: f32, color: Color) {
        let (at, tags_at) = if self.spread.is_empty() { self.tag_layout(full, size) } else { (0.0, full - self.tag_w) };
        Self::paint_items(&self.items, ui, x + at, baseline, color);
        Self::paint_items(&self.tags, ui, x + tags_at, baseline, color);
        for row in &self.spread {
            let indent = MULTLINE_INDENT * size;
            let room = (full - self.tag_w - indent * 2.0 - row.w).max(0.0);
            Self::paint_items(&row.items, ui, x + indent + room * row.free, baseline, color);
        }
    }

    fn paint_items(items: &[Item], ui: &mut Ui<'_>, x: f32, baseline: f32, color: Color) {
        let s = ui.scale;
        let clip = super::glyph_clip(ui);
        let (ox, oy) = ((x * s).round(), (baseline * s).round());
        let layer = ui.layer;
        for item in items {
            match item {
                Item::Run { text, size, bold, italic, x, y, color: own } => {
                    let run = ui.text.math_line(text, *size, *bold, *italic);
                    let top = oy + (*y * s).round() - run.baseline();
                    run.paint_tinted(&mut ui.scene.layers[layer].glyphs, ox + (*x * s).round(), top, clip, own.unwrap_or(color));
                }
                Item::Rule { x, y, w, h, color: own } => {
                    let (left, top) = (ox + (*x * s).round(), oy + (*y * s).round());
                    let r = rect(left / s, top / s, (*w * s).round().max(1.0) / s, (*h * s).round().max(1.0) / s);
                    ui.fill(r, 0.0, own.unwrap_or(color));
                }
                Item::Path { data, x, y, px, color: own } => {
                    if let Some(inst) = ui.text.path_mask(data, px.0, px.1) {
                        let pos = [ox + (*x * s).round(), oy + (*y * s).round()];
                        ui.scene.layers[layer].glyphs.push(GlyphInst { pos, clip, color: own.unwrap_or(color), ..inst });
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- the cache

struct Entry {
    source: String,
    display: bool,
    typeset: Rc<Typeset>,
    used: u64,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<u64, Entry>,
    clock: u64,
    /// How many formulas have been set, not found.
    set: u64,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// How many formulas have been set rather than found on this thread, and how many are kept. What tests count.
pub(super) fn cache_stats() -> (u64, usize) {
    CACHE.with(|c| {
        let c = c.borrow();
        (c.set, c.entries.len())
    })
}

/// A formula at a text size, from the cache if it was set before. A stream re-reads its whole text at every commit, and this
/// is what makes every formula already on screen a map lookup.
pub(super) fn typeset(text: &mut TextCtx, source: &str, display: bool, base_size: f32) -> Rc<Typeset> {
    // As MathJax's `ex`es do, the formula takes its size from the text around it: letters as tall as the letters beside them.
    let em = base_size * text.math_scale();
    let key = {
        use std::hash::Hasher;
        let mut h = fluent::text::FxHasher::default();
        h.write_u64(hash_str(source));
        h.write_u8(display as u8);
        h.write_u32(em.to_bits());
        h.write_u32(text.scale.to_bits());
        h.finish()
    };
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.clock += 1;
        let clock = cache.clock;
        if let Some(entry) = cache.entries.get_mut(&key)
            && entry.display == display
            && entry.source == source
        {
            entry.used = clock;
            return entry.typeset.clone();
        }
        let typeset = Rc::new(set_formula(text, source, display, em));
        cache.set += 1;
        if cache.entries.len() >= CACHE_LIMIT
            && let Some(oldest) = cache.entries.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| *k)
        {
            cache.entries.remove(&oldest);
        }
        cache.entries.insert(key, Entry { source: source.to_owned(), display, typeset: typeset.clone(), used: clock });
        typeset
    })
}

fn set_formula(text: &mut TextCtx, source: &str, display: bool, em: f32) -> Typeset {
    match tex::parse(source) {
        Err(message) => Typeset::Error(Arc::from(message)),
        Ok(nodes) => Typeset::Ready(lay_nodes(text, &nodes, display, em)),
    }
}

fn lay_nodes(text: &mut TextCtx, nodes: &[Node], display: bool, em: f32) -> Formula {
    // Measured large, so that one pixel's rounding is nothing against it.
    const PROBE: f32 = 64.0;
    let scale = text.scale;
    let x_height = text.math_line("x", PROBE, false, false).ink().map_or(0.45, |[_, above, ..]| above / (PROBE * scale));
    let axis = text.math_line("\u{2212}", PROBE, false, false).ink().map_or(0.25, |[_, above, _, below]| (above - below) * 0.5 / (PROBE * scale));
    let has_math = text.has_math_face();
    let mut layout = Layout { text, scale, em, has_math, x_height, axis, display, stretch_to: None, top_array: None, tags: Vec::new(), spread: Vec::new() };
    let style = if display { MathStyle::Display } else { MathStyle::Text };
    let st = St { style, cramped: false, color: None, mul: 1.0 };
    // `Tags.finalize`: a tag on the formula itself only means something in display, and it goes where the column ends.
    let (body, own_tag) = match nodes.split_last() {
        Some((Node::Tag(tag), rest)) => (rest, Some(tag)),
        _ => (nodes, None),
    };
    if display
        && let [Node::Array(array)] = body
    {
        layout.top_array = Some(&**array as *const Array);
    }
    let mut set = layout.seq(body, st);
    set.close_ic();
    let mut tags = std::mem::take(&mut layout.tags);
    if display
        && let Some(tag) = own_tag
    {
        let mut one = layout.seq(tag, St { style: MathStyle::Text, ..st });
        one.close_ic();
        tags.push(one);
    }
    let spread = std::mem::take(&mut layout.spread);
    let tag_w = tags.iter().map(|t| t.w).fold(0.0, f32::max);
    let mut tag_items = Vec::new();
    let (mut h, mut d) = (set.h, set.d);
    for mut tag in tags {
        h = h.max(tag.h);
        d = d.max(tag.d);
        let free = tag_w - tag.w;
        tag.items.iter_mut().for_each(|item| item.shift(free, 0.0));
        tag_items.append(&mut tag.items);
    }
    let w = if spread.is_empty() { set.w } else { set.w + MULTLINE_INDENT * em * 2.0 };
    Formula { w: w.max(0.0), h: h.max(0.0), d: d.max(0.0), items: set.items, tags: tag_items, tag_w, spread }
}

// ---------------------------------------------------------------- boxes

/// A box being put together: what it holds runs from (0, 0), y going down, and `h` and `d` are its extent about that line.
#[derive(Default)]
struct MBox {
    w: f32,
    h: f32,
    d: f32,
    /// How far the last glyph sticks out past `w`: TeX's italic correction, which a superscript clears and a subscript does not.
    ic: f32,
    /// A single glyph's middle, which an accent centres on.
    ink_mid: Option<f32>,
    /// A single character, which TeX puts scripts on at the font's own heights rather than the box's.
    is_char: bool,
    slanted: bool,
    items: Vec<Item>,
}

impl MBox {
    fn place(&mut self, mut child: MBox, dx: f32, dy: f32) {
        for item in &mut child.items {
            item.shift(dx, dy);
        }
        self.items.append(&mut child.items);
        self.h = self.h.max(child.h - dy);
        self.d = self.d.max(child.d + dy);
        self.w = self.w.max(dx + child.w + child.ic);
    }

    /// Takes the italic correction into the width: nothing is going to hang off this box any more.
    fn close_ic(&mut self) {
        self.w += self.ic;
        self.ic = 0.0;
    }

    fn shift_down(&mut self, dy: f32) {
        for item in &mut self.items {
            item.shift(0.0, dy);
        }
        self.h -= dy;
        self.d += dy;
    }
}

/// What a part of a formula is set in.
#[derive(Clone, Copy)]
struct St {
    style: MathStyle,
    cramped: bool,
    color: Option<Color>,
    mul: f32,
}

impl St {
    fn scripted(self) -> St {
        let style = match self.style {
            MathStyle::Display | MathStyle::Text => MathStyle::Script,
            MathStyle::Script | MathStyle::ScriptScript => MathStyle::ScriptScript,
        };
        St { style, ..self }
    }

    fn sub(self) -> St {
        St { cramped: true, ..self.scripted() }
    }

    fn num(self) -> St {
        let style = match self.style {
            MathStyle::Display => MathStyle::Text,
            MathStyle::Text => MathStyle::Script,
            MathStyle::Script | MathStyle::ScriptScript => MathStyle::ScriptScript,
        };
        St { style, ..self }
    }

    fn den(self) -> St {
        St { cramped: true, ..self.num() }
    }
}

struct Layout<'t> {
    text: &'t mut TextCtx,
    scale: f32,
    em: f32,
    has_math: bool,
    x_height: f32,
    axis: f32,
    display: bool,
    /// How tall the `\empheqlbrace` being set is to be.
    stretch_to: Option<f32>,
    /// The array that is the whole of the formula: its tags go to the side of the column, and a `multline`'s rows to its sides.
    top_array: Option<*const Array>,
    tags: Vec<MBox>,
    spread: Vec<SpreadRow>,
}

/// The table of TeXbook chapter 18, in `mu`: what goes between two classes of atom. Negative stands for "not in scripts".
fn atom_space(left: Class, right: Class) -> f32 {
    use Class::*;
    match (left, right) {
        (Ord, Op) | (Op, Ord) | (Op, Op) | (Close, Op) | (Inner, Op) => 3.0,
        (Ord, Bin) | (Bin, Ord) | (Bin, Op) | (Bin, Open) | (Bin, Inner) | (Close, Bin) | (Inner, Bin) => -4.0,
        (Ord, Rel) | (Op, Rel) | (Rel, Ord) | (Rel, Op) | (Rel, Open) | (Rel, Inner) | (Close, Rel) | (Inner, Rel) => -5.0,
        (Ord, Inner) | (Op, Inner) | (Close, Inner) | (Punct, _) | (Inner, Ord) | (Inner, Open) | (Inner, Punct) | (Inner, Inner) => -3.0,
        _ => 0.0,
    }
}

fn class_of(node: &Node) -> Class {
    match node {
        Node::Sym { class, .. } => *class,
        Node::Op { .. } => Class::Op,
        Node::Scripts { base, .. } => class_of(base),
        Node::Classed(class, _) => *class,
        Node::Frac(_) | Node::Fenced { .. } => Class::Inner,
        Node::Big { class, .. } => *class,
        Node::Stack { class: Some(class), .. } => *class,
        Node::Stack { base, class: None, .. } if base.len() == 1 => class_of(&base[0]),
        Node::Arrow { .. } => Class::Rel,
        Node::Colored { body, .. } if body.len() == 1 => class_of(&body[0]),
        Node::Not(inner) => class_of(inner),
        Node::SideSet { .. } => Class::Op,
        Node::CenterOver { base, .. } if base.len() == 1 => class_of(&base[0]),
        Node::Cramped { body, .. } if body.len() == 1 => class_of(&body[0]),
        Node::Choice(parts) if parts[1].len() == 1 => class_of(&parts[1][0]),
        Node::Array(array) if array.left != '\0' || array.right != '\0' => Class::Inner,
        _ => Class::Ord,
    }
}

enum SeqEntry {
    Atom { class: Class, set: MBox, scripted: bool },
    Kern(f32),
}

impl Layout<'_> {
    fn size(&self, st: St) -> f32 {
        let factor = match st.style {
            MathStyle::Display | MathStyle::Text => 1.0,
            MathStyle::Script => SCRIPT_SCALE,
            MathStyle::ScriptScript => SCRIPT_SCRIPT_SCALE,
        };
        self.em * st.mul * factor
    }

    fn thickness(&self, size: f32) -> f32 {
        (RULE * size * self.scale).round().max(1.0) / self.scale
    }

    fn seq(&mut self, nodes: &[Node], st: St) -> MBox {
        let mut st = st;
        let mut entries = Vec::with_capacity(nodes.len());
        let mut skip_kern = false;
        for node in nodes {
            match node {
                Node::SetStyle(style) => {
                    st.style = *style;
                    st.cramped = false;
                }
                Node::SetColor(color) => st.color = Some(*color),
                Node::SetSize(mul) => st.mul = *mul,
                Node::NonScript => skip_kern = matches!(st.style, MathStyle::Script | MathStyle::ScriptScript),
                Node::Space(em) => {
                    if !std::mem::take(&mut skip_kern) {
                        entries.push(SeqEntry::Kern(em * self.size(st)));
                    }
                }
                _ => {
                    let set = self.node(node, st);
                    let scripted = matches!(st.style, MathStyle::Script | MathStyle::ScriptScript);
                    entries.push(SeqEntry::Atom { class: class_of(node), set, scripted });
                }
            }
        }
        // One thing alone is that thing, with what scripts and accents want to know about it.
        if let [SeqEntry::Atom { set, .. }] = &mut entries[..] {
            return std::mem::take(set);
        }
        settle_binaries(&mut entries);
        let mu = self.size(st) / 18.0;
        let mut out = MBox::default();
        let (mut x, mut previous) = (0.0, None);
        for entry in entries {
            match entry {
                SeqEntry::Kern(kern) => x += kern,
                SeqEntry::Atom { class, mut set, scripted } => {
                    if let Some(left) = previous {
                        let space = atom_space(left, class);
                        if space > 0.0 || (space < 0.0 && !scripted) {
                            x += space.abs() * mu;
                        }
                    }
                    set.close_ic();
                    let w = set.w;
                    out.place(set, x, 0.0);
                    x += w;
                    previous = Some(class);
                }
            }
        }
        out.w = x;
        out
    }

    fn node(&mut self, node: &Node, st: St) -> MBox {
        match node {
            Node::Sym { text, font, bold, .. } => self.sym(text, *font, *bold, st),
            Node::Text { text, font } => self.sym(text, *font, false, st),
            Node::Undefined(name) => {
                // `noundefined`'s colour is the name `red`, whatever the theme.
                let size = self.size(st);
                let color = Some(Color::rgb(0xFF0000));
                self.glyph(name, size, false, false, color)
            }
            Node::Op { body, big, integral, .. } => self.operator(body, *big, *integral, st),
            Node::Scripts { base, sup, sub, primes } => self.scripts(base, sup.as_deref(), sub.as_deref(), *primes, st),
            Node::Group(nodes) | Node::Classed(_, nodes) => self.seq(nodes, st),
            Node::Frac(frac) => self.frac(frac, st),
            Node::Sqrt { index, body, up, left } => self.sqrt(index.as_deref(), body, *up, *left, st),
            Node::Fenced { left, parts, middles, right } => self.fenced(*left, parts, middles, *right, st),
            Node::Big { delim, height, .. } => {
                let size = self.size(st);
                self.delimiter(*delim, *height * size, st)
            }
            Node::Accent { kind, wide, body } => self.accent(*kind, *wide, body, st),
            Node::Over { kind, body } => self.over(*kind, body, st),
            Node::Stack { base, above, below, .. } => {
                let base = self.seq(base, st);
                self.limits(base, above.as_deref().map(StackPart::Nodes), below.as_deref().map(StackPart::Nodes), st)
            }
            Node::Arrow { arrow, above, below } => self.x_arrow(*arrow, above, below, st),
            Node::Array(array) => self.array(array, st),
            Node::Colored { color, body } => self.seq(body, St { color: Some(*color), ..st }),
            Node::ColorBox { fill, stroke, body } => self.color_box(*fill, *stroke, body, st),
            Node::Phantom { width, height, body } => {
                let set = self.seq(body, st);
                MBox { w: if *width { set.w + set.ic } else { 0.0 }, h: if *height { set.h } else { 0.0 }, d: if *height { set.d } else { 0.0 }, ..MBox::default() }
            }
            Node::Smash { body } => {
                let mut set = self.seq(body, st);
                set.h = 0.0;
                set.d = 0.0;
                set
            }
            Node::Lap { side, body } => {
                let mut set = self.seq(body, st);
                set.close_ic();
                let dx = match side {
                    -1 => -set.w,
                    0 => -set.w * 0.5,
                    _ => 0.0,
                };
                let (h, d) = (set.h, set.d);
                let mut out = MBox::default();
                out.place(set, dx, 0.0);
                MBox { w: 0.0, h, d, ..out }
            }
            Node::Boxed { body } => self.boxed(body, St { style: MathStyle::Display, ..st }),
            Node::Enclose(enclose) => self.enclose(enclose, st),
            Node::BBox(bbox) => self.bbox(bbox, st),
            Node::Choice(parts) => {
                let at = match st.style {
                    MathStyle::Display => 0,
                    MathStyle::Text => 1,
                    MathStyle::Script => 2,
                    MathStyle::ScriptScript => 3,
                };
                self.seq(&parts[at], st)
            }
            Node::VBox { align, body } => self.vbox(*align, body, st),
            Node::Move { by, body } => self.moved(*by, body, st),
            Node::Sized { width, align, body } => self.sized(*width, *align, body, st),
            Node::Prescript { base, sup, sub } => {
                let base = self.seq(base, st);
                self.prescript(base, sup, sub, st)
            }
            Node::SideSet { pre_sup, pre_sub, post_sup, post_sub, base } => {
                let base = self.node(base, st);
                let with_post = self.side_scripts_nodes(base, post_sup, post_sub, st);
                self.prescript(with_post, pre_sup, pre_sub, st)
            }
            Node::CenterOver { base, over } => self.center_over(base, over, st),
            Node::Cramped { style, body } => {
                let cramped = St { cramped: true, style: style.unwrap_or(st.style), ..st };
                self.seq(body, cramped)
            }
            Node::Proof(proof) => self.proof_tree(proof, st),
            Node::CdArrow(arrow) => self.cd_arrow(arrow, None, None, st),
            Node::Empheq { left, right, body } => self.empheq(left, right, body, st),
            Node::Stretch(delim) => {
                let total = self.stretch_to.unwrap_or(0.0);
                self.delimiter(*delim, total, st)
            }
            Node::Rule { width, height, raise } => {
                let size = self.size(st);
                let (w, h, raise) = (width * size, height * size, raise * size);
                let mut out = MBox { w, h: (h + raise).max(0.0), d: (-raise).max(0.0), ..MBox::default() };
                if w > 0.0 && h > 0.0 {
                    out.items.push(Item::Rule { x: 0.0, y: -(h + raise), w, h, color: st.color });
                }
                out
            }
            Node::Raise { by, body } => {
                let by = by * self.size(st);
                let mut set = self.seq(body, st);
                set.shift_down(-by);
                set
            }
            Node::Lines(lines) => self.lines(lines, st),
            Node::Not(inner) => self.not(inner, st),
            // A tag only ever stands at the end of the formula, which `lay_nodes` has taken care of.
            Node::Tag(_) | Node::Space(_) | Node::SetStyle(_) | Node::SetColor(_) | Node::SetSize(_) | Node::NonScript => MBox::default(),
        }
    }

    /// A run of text, sized by what it draws.
    fn glyph(&mut self, text: &str, size: f32, bold: bool, italic: bool, color: Option<Color>) -> MBox {
        let run = self.text.math_line(text, size, bold, italic);
        let s = self.scale;
        let w = run.advance / s;
        let mut out = MBox { w, slanted: italic, ..MBox::default() };
        if let Some([left, above, right, below]) = run.ink() {
            out.h = (above / s).max(0.0);
            out.d = (below / s).max(0.0);
            out.ic = (right / s - w).max(0.0);
            out.ink_mid = Some((left + right) * 0.5 / s);
        }
        out.items.push(Item::Run { text: Rc::from(text), size, bold, italic, x: 0.0, y: 0.0, color });
        out
    }

    fn sym(&mut self, text: &str, font: MathFont, bold: bool, st: St) -> MBox {
        let size = self.size(st);
        let shaped = shown_text(text, font, bold, self.has_math);
        let mut out = self.glyph(&shaped.text, size, shaped.bold, shaped.italic, st.color);
        out.is_char = text.chars().count() == 1;
        out.slanted = shaped.slanted;
        // A prime is drawn up where primes go, and as a script it is the box that should be up there.
        if text == "′" || text == "″" || text == "‴" {
            self.drop_to_baseline(&mut out);
        }
        out
    }

    fn drop_to_baseline(&mut self, out: &mut MBox) {
        let Some(Item::Run { text, size, bold, italic, .. }) = out.items.first() else { return };
        let run = self.text.math_line(text, *size, *bold, *italic);
        if let Some([_, above, _, below]) = run.ink()
            && below < 0.0
        {
            out.shift_down(-below / self.scale);
            out.h = ((above + below) / self.scale).max(0.0);
            out.d = 0.0;
        }
    }

    fn moved(&mut self, by: f32, body: &[Node], st: St) -> MBox {
        let size = self.size(st);
        let mut set = self.seq(body, st);
        set.close_ic();
        let mut out = MBox::default();
        let w = set.w + by * size;
        out.place(set, by * size, 0.0);
        out.w = w.max(0.0);
        out
    }

    fn sized(&mut self, width: Option<f32>, align: i8, body: &[Node], st: St) -> MBox {
        let size = self.size(st);
        let mut set = self.seq(body, st);
        set.close_ic();
        let w = width.map_or(set.w, |w| w * size);
        let dx = match align {
            -1 => 0.0,
            1 => w - set.w,
            _ => (w - set.w) * 0.5,
        };
        let mut out = MBox::default();
        out.place(set, dx, 0.0);
        out.w = w;
        out
    }

    fn center_over(&mut self, base: &[Node], over: &[Node], st: St) -> MBox {
        let mut set = self.seq(base, st);
        set.close_ic();
        let mut over = self.seq(over, st);
        over.close_ic();
        let w = set.w;
        let (ow, ic) = (over.w, set.ic);
        let mut out = MBox::default();
        out.place(set, 0.0, 0.0);
        out.place(over, (w - ow) * 0.5, 0.0);
        out.w = w;
        out.ic = ic;
        out
    }

    fn side_scripts_nodes(&mut self, base: MBox, sup: &[Node], sub: &[Node], st: St) -> MBox {
        if sup.is_empty() && sub.is_empty() {
            return base;
        }
        // The nodes are one script each, read off `\sideset`'s argument.
        let sup_node = sup.first();
        let sub_node = sub.first();
        self.side_scripts(base, false, sup_node, sub_node, 0, st)
    }

    /// `mmultiscripts`' left side: scripts before the base, lined up to their right.
    fn prescript(&mut self, base: MBox, sup: &[Node], sub: &[Node], st: St) -> MBox {
        let size = self.size(st);
        let script = st.scripted();
        let up_box = (!sup.is_empty()).then(|| {
            let mut set = self.seq(sup, script);
            set.close_ic();
            set
        });
        let down_box = (!sub.is_empty()).then(|| {
            let mut set = self.seq(sub, St { cramped: true, ..script });
            set.close_ic();
            set
        });
        let lead = up_box.iter().chain(down_box.iter()).map(|b| b.w).fold(0.0, f32::max) + SCRIPT_SPACE * self.em * st.mul;
        let up = (base.h - SUP_DROP * self.size(script)).max(SUP2 * size);
        let down = (base.d + SUB_DROP * self.size(script)).max(SUB1 * size);
        let mut out = MBox::default();
        if let Some(set) = up_box {
            let w = set.w;
            out.place(set, lead - SCRIPT_SPACE * self.em * st.mul - w, -up);
        }
        if let Some(set) = down_box {
            let w = set.w;
            out.place(set, lead - SCRIPT_SPACE * self.em * st.mul - w, down);
        }
        let (w, ic) = (base.w, base.ic);
        out.place(base, lead, 0.0);
        out.w = lead + w;
        out.ic = ic;
        out
    }

    /// `\vcenter`, `\vtop`, `\vbox`: where a tall thing's baseline is. A `\vbox` keeps its last line's.
    fn vbox(&mut self, align: VAlign, body: &[Node], st: St) -> MBox {
        let size = self.size(st);
        let mut set = self.seq(body, st);
        set.close_ic();
        match align {
            VAlign::Center => {
                let shift = (set.h - set.d) * 0.5 - self.axis * size;
                set.shift_down(shift);
            }
            VAlign::Top => {
                let strut = 0.75 * size;
                if set.h > strut {
                    let shift = set.h - strut;
                    set.shift_down(shift);
                }
            }
            VAlign::Bottom => {}
        }
        set
    }

    fn operator(&mut self, body: &Node, big: bool, integral: bool, st: St) -> MBox {
        let Node::Sym { text, .. } = body else {
            return self.node(body, st);
        };
        if !big {
            return self.node(body, st);
        }
        let size = self.size(st);
        let grow = match (st.style, integral) {
            (MathStyle::Display, true) => DISPLAY_INTEGRAL,
            (MathStyle::Display, false) => DISPLAY_OP,
            _ => 1.0,
        };
        let mut out = self.glyph(text, size * grow, false, false, st.color);
        // The operator's middle is on the axis, whatever the font makes of its depth.
        let middle = (out.h - out.d) * 0.5;
        out.shift_down(middle - self.axis * size);
        // An integral leans: its lower limit tucks in under the curl and the upper stands clear, which in TeX is this correction.
        if integral {
            let lean = 0.2 * size * grow;
            out.w -= lean;
            out.ic += lean;
        }
        out
    }

    fn scripts(&mut self, base: &Node, sup: Option<&Node>, sub: Option<&Node>, primes: u8, st: St) -> MBox {
        if let Node::SideSet { base: inner, .. } = base {
            let limited = matches!(&**inner, Node::Op { display_limits: true, limits: None, .. }) && st.style == MathStyle::Display;
            let set = self.node(base, st);
            if limited {
                return self.limits(set, sup.map(StackPart::Node), sub.map(StackPart::Node), st);
            }
            return self.side_scripts(set, false, sup, sub, primes, st);
        }
        if let Node::Op { body, big, integral, display_limits, limits } = base {
            let over_under = limits.unwrap_or(*display_limits && st.style == MathStyle::Display);
            let op = self.operator(body, *big, *integral, st);
            if over_under {
                return self.limits(op, sup.map(StackPart::Node), sub.map(StackPart::Node), st);
            }
            return self.side_scripts(op, false, sup, sub, primes, st);
        }
        let set = self.node(base, st);
        let is_char = set.is_char;
        self.side_scripts(set, is_char, sup, sub, primes, st)
    }

    fn side_scripts(&mut self, base: MBox, is_char: bool, sup: Option<&Node>, sub: Option<&Node>, primes: u8, st: St) -> MBox {
        let size = self.size(st);
        let x_height = self.x_height * size;
        let rule = RULE * size;
        let script_size = self.size(st.scripted());
        let sup_box = self.sup_box(sup, primes, st);
        let sub_box = sub.map(|node| self.node(node, st.sub()));
        let (mut up, mut down) = if is_char { (0.0, 0.0) } else { (base.h - SUP_DROP * script_size, base.d + SUB_DROP * script_size) };
        let up_min = if st.cramped {
            SUP3
        } else if st.style == MathStyle::Display {
            SUP1
        } else {
            SUP2
        } * size;
        let (base_w, base_ic) = (base.w, base.ic);
        let (mut h_end, mut out) = (base_w + base_ic, MBox::default());
        let space = SCRIPT_SPACE * self.em * st.mul;
        out.place(base, 0.0, 0.0);
        match (sup_box, sub_box) {
            (Some(sup), None) => {
                up = up.max(up_min).max(sup.d + x_height * 0.25);
                h_end = h_end.max(base_w + base_ic + sup.w + sup.ic);
                out.place(sup, base_w + base_ic, -up);
            }
            (None, Some(sub)) => {
                down = down.max(SUB1 * size).max(sub.h - x_height * 0.8);
                h_end = h_end.max(base_w + sub.w + sub.ic);
                out.place(sub, base_w, down);
            }
            (Some(sup), Some(sub)) => {
                up = up.max(up_min).max(sup.d + x_height * 0.25);
                down = down.max(SUB2 * size);
                let gap = (up - sup.d) - (sub.h - down);
                if gap < 4.0 * rule {
                    down += 4.0 * rule - gap;
                    let psi = x_height * 0.8 - (up - sup.d);
                    if psi > 0.0 {
                        up += psi;
                        down -= psi;
                    }
                }
                h_end = h_end.max(base_w + base_ic + sup.w + sup.ic).max(base_w + sub.w + sub.ic);
                out.place(sup, base_w + base_ic, -up);
                out.place(sub, base_w, down);
            }
            (None, None) => {}
        }
        out.w = h_end + space;
        out
    }

    fn sup_box(&mut self, sup: Option<&Node>, primes: u8, st: St) -> Option<MBox> {
        if primes == 0 {
            return sup.map(|node| self.node(node, st.scripted()));
        }
        let shown: String = match primes {
            1 => "′".to_owned(),
            2 => "″".to_owned(),
            3 => "‴".to_owned(),
            n => "′".repeat(n as usize),
        };
        let scripted = st.scripted();
        let mut out = self.sym(&shown, MathFont::Roman, false, scripted);
        if let Some(node) = sup {
            let mut rest = self.node(node, scripted);
            rest.close_ic();
            out.close_ic();
            let at = out.w;
            let w = at + rest.w;
            let (h, d) = (out.h, out.d);
            let mut joined = MBox { h, d, ..MBox::default() };
            joined.place(out, 0.0, 0.0);
            joined.place(rest, at, 0.0);
            joined.w = w;
            return Some(joined);
        }
        out.is_char = false;
        Some(out)
    }

    /// TeXbook rule 13a: over and under, centred.
    fn limits(&mut self, op: MBox, above: Option<StackPart<'_>>, below: Option<StackPart<'_>>, st: St) -> MBox {
        let size = self.size(st);
        let [one, two, three, four, five] = BIG_OP_SPACING.map(|v| v * size);
        let scripted = st.scripted();
        let above = above.map(|part| self.stack_part(part, scripted));
        let below = below.map(|part| self.stack_part(part, St { cramped: true, ..scripted }));
        let delta = op.ic;
        let w = [Some(op.w + op.ic), above.as_ref().map(|b| b.w + b.ic), below.as_ref().map(|b| b.w + b.ic)].into_iter().flatten().fold(0.0, f32::max);
        let mut out = MBox::default();
        let (op_h, op_d, op_w) = (op.h, op.d, op.w);
        out.place(op, (w - op_w) * 0.5 - delta * 0.5, 0.0);
        if let Some(above) = above {
            let gap = one.max(three - above.d);
            let y = op_h + gap + above.d;
            let (x, top) = ((w - above.w) * 0.5 + delta * 0.5, y + above.h + five);
            out.place(above, x, -y);
            out.h = out.h.max(top);
        }
        if let Some(below) = below {
            let gap = two.max(four - below.h);
            let y = op_d + gap + below.h;
            let (x, bottom) = ((w - below.w) * 0.5 - delta * 0.5, y + below.d + five);
            out.place(below, x, y);
            out.d = out.d.max(bottom);
        }
        out.w = w;
        out
    }

    fn stack_part(&mut self, part: StackPart<'_>, st: St) -> MBox {
        match part {
            StackPart::Node(node) => self.node(node, st),
            StackPart::Nodes(nodes) => self.seq(nodes, st),
        }
    }

    /// TeXbook rule 15.
    fn frac(&mut self, frac: &Frac, st: St) -> MBox {
        let cur = frac.style.map_or(st, |style| St { style, cramped: false, ..st });
        if frac.bevelled {
            return self.bevelled(frac, cur);
        }
        let size = self.size(cur);
        let (num_st, den_st) = if frac.continued { (cur, cur) } else { (cur.num(), cur.den()) };
        let mut num = self.seq(&frac.num, num_st);
        let mut den = self.seq(&frac.den, den_st);
        num.close_ic();
        den.close_ic();
        let display = cur.style == MathStyle::Display;
        let bar = frac.bar.map_or_else(|| self.thickness(size), |thick| if thick > 0.0 { (thick * size).max(1.0 / self.scale) } else { 0.0 });
        let axis = self.axis * size;
        let w = num.w.max(den.w);
        let (mut up, mut down) = if display {
            (NUM1 * size, DENOM1 * size)
        } else {
            ((if bar > 0.0 { NUM2 } else { NUM3 }) * size, DENOM2 * size)
        };
        if bar > 0.0 {
            let clearance = if display { 3.0 * RULE * size } else { RULE * size };
            up += (clearance - ((up - num.d) - (axis + bar * 0.5))).max(0.0);
            down += (clearance - ((axis - bar * 0.5) - (den.h - down))).max(0.0);
        } else {
            let clearance = if display { 7.0 * RULE * size } else { 3.0 * RULE * size };
            let gap = (up - num.d) - (den.h - down);
            if gap < clearance {
                up += (clearance - gap) * 0.5;
                down += (clearance - gap) * 0.5;
            }
        }
        let pad = NULL_DELIMITER * size;
        let mut out = MBox::default();
        let (num_w, den_w) = (num.w, den.w);
        let along = |side: i8, free: f32| match side {
            -1 => 0.0,
            1 => free,
            _ => free * 0.5,
        };
        out.place(num, pad + along(frac.align.0, w - num_w), -up);
        out.place(den, pad + along(frac.align.1, w - den_w), down);
        if bar > 0.0 {
            out.items.push(Item::Rule { x: pad, y: -(axis + bar * 0.5), w, h: bar, color: st.color });
            out.h = out.h.max(axis + bar * 0.5);
        }
        out.w = w + pad * 2.0;
        if frac.left == '\0' && frac.right == '\0' {
            return out;
        }
        // `\binom`: brackets as tall as rule 15e says, then the fraction between them with no bar's padding to keep.
        let least = if display { DELIM1 } else { DELIM2 } * size;
        let total = least.max(self.delimiter_target(out.h, out.d, size));
        let left = self.delimiter(frac.left, total, cur);
        let right = self.delimiter(frac.right, total, cur);
        hjoin(vec![left, out, right])
    }

    /// `bevelled`: the numerator up to the left of a slash and the denominator down to its right.
    fn bevelled(&mut self, frac: &Frac, cur: St) -> MBox {
        let size = self.size(cur);
        let mut num = self.seq(&frac.num, cur.num());
        let mut den = self.seq(&frac.den, cur.den());
        num.close_ic();
        den.close_ic();
        let axis = self.axis * size;
        let (up, down) = (0.25 * size, 0.25 * size);
        let h = (num.h + up + axis).max(0.5 * size);
        let d = (den.d + down - axis).max(0.0);
        let slash = self.delimiter('/', h + d, cur);
        let mut out = MBox::default();
        let num_w = num.w;
        out.place(num, 0.0, -(up + axis));
        let at = num_w - 0.05 * size;
        let slash_w = slash.w;
        out.place(slash, at, 0.0);
        let den_at = at + slash_w - 0.05 * size;
        let den_w = den.w;
        out.place(den, den_at, down - axis);
        out.w = den_at + den_w;
        out
    }

    fn delimiter_target(&self, h: f32, d: f32, size: f32) -> f32 {
        // `\delimiterfactor` and `\delimitershortfall`.
        let axis = self.axis * size;
        let half = (h - axis).max(d + axis);
        (2.0 * half * 0.901).max(2.0 * half - 0.5 * size)
    }

    fn fenced(&mut self, left: char, parts: &[Vec<Node>], middles: &[char], right: char, st: St) -> MBox {
        let size = self.size(st);
        let sets: Vec<MBox> = parts.iter().map(|part| self.seq(part, st)).collect();
        let h = sets.iter().map(|b| b.h).fold(0.0, f32::max);
        let d = sets.iter().map(|b| b.d).fold(0.0, f32::max);
        let total = self.delimiter_target(h, d, size);
        let mut row = Vec::with_capacity(sets.len() * 2 + 2);
        row.push(self.delimiter(left, total, st));
        for (i, set) in sets.into_iter().enumerate() {
            row.push(set);
            if let Some(middle) = middles.get(i) {
                row.push(self.delimiter(*middle, total, st));
            }
        }
        row.push(self.delimiter(right, total, st));
        hjoin(row)
    }

    /// TeXbook rule 11, with a radical drawn to the height it has to be.
    fn sqrt(&mut self, index: Option<&[Node]>, body: &[Node], up: f32, left: f32, st: St) -> MBox {
        let size = self.size(st);
        let mut set = self.seq(body, St { cramped: true, ..st });
        set.close_ic();
        let rule = self.thickness(size);
        let gap = rule + if st.style == MathStyle::Display { self.x_height * size * 0.25 } else { rule * 0.25 };
        let (top, bottom) = (set.h + gap + rule, set.d.max(0.02 * size));
        let total = (top + bottom).max(0.9 * size);
        let top = total - bottom;
        let radical_w = 0.72 * size;
        let body_x = radical_w + 0.03 * size;
        let w = body_x + set.w + 0.05 * size;
        let mut index_box = index.map(|nodes| {
            let mut set = self.seq(nodes, St { style: MathStyle::ScriptScript, cramped: false, ..st });
            set.close_ic();
            set
        });
        // An index narrower than the crook of the radical sits in it; a wider one pushes everything over.
        let lead = index_box.as_ref().map_or(0.0, |ix| (ix.w - 0.42 * size).max(0.0));
        let mut out = MBox { h: top, d: bottom, ..MBox::default() };
        if let Some(pen) = self.radical_pen(radical_w, total, rule, size) {
            out.items.push(self.path(pen, lead, -top, radical_w, total, st.color));
        } else {
            out.items.push(Item::Rule { x: lead + radical_w - rule, y: -top, w: rule, h: total, color: st.color });
        }
        out.items.push(Item::Rule { x: lead + radical_w - 0.5 / self.scale, y: -top, w: w - radical_w + 0.5 / self.scale, h: rule, color: st.color });
        if let Some(ix) = index_box.take() {
            let raise = (top + bottom) * 0.6 - bottom;
            let x = lead + (0.42 * size - ix.w).max(0.0) - left * size;
            out.place(ix, x, -raise - 0.02 * size - up * size);
        }
        out.place(set, lead + body_x, 0.0);
        out.w = lead + w;
        out
    }

    fn radical_pen(&self, w: f32, h: f32, rule: f32, size: f32) -> Option<Pen> {
        if h * self.scale > MAX_PATH_PX {
            return None;
        }
        // The shape of cmex's radical: a small rise, a heavy stroke down, a thin one all the way up. The bottom keeps its
        // proportions as the sign grows, and only the last stroke gets taller.
        let bottom = 0.34 * size;
        let hook = (0.40 * size).min(h * 0.62);
        let (heavy, thin) = (0.095 * size, (0.045 * size).max(rule * 0.8));
        let mut pen = Pen::new(self.scale);
        let (hook_y, top_of_hook) = (h - hook, h - hook - 0.06 * size);
        pen.m(0.0, hook_y + 0.02 * size);
        pen.l(0.135 * size, top_of_hook);
        pen.l(bottom + heavy * 0.1, h - 0.17 * size);
        pen.l(w - thin * 1.15, 0.0);
        pen.l(w, 0.0);
        pen.l(w, rule);
        pen.l(bottom + thin * 0.55, h);
        pen.l(bottom - thin * 0.55, h);
        pen.l(0.135 * size - heavy, top_of_hook + 0.08 * size);
        pen.l(0.02 * size, hook_y + 0.05 * size);
        pen.z();
        Some(pen)
    }

    fn path(&self, pen: Pen, x: f32, y: f32, w: f32, h: f32, color: Option<Color>) -> Item {
        let s = self.scale;
        Item::Path { data: Rc::from(pen.finish()), x, y, px: ((w * s).ceil() as u32 + 1, (h * s).ceil() as u32 + 1), color }
    }

    /// A delimiter as tall as asked: the font's own when that is tall enough, or else a drawn one centred on the axis.
    fn delimiter(&mut self, delim: char, total: f32, st: St) -> MBox {
        let size = self.size(st);
        if delim == '\0' {
            return MBox { w: NULL_DELIMITER * size, ..MBox::default() };
        }
        let mut buffer = [0u8; 4];
        let natural = self.glyph(delim.encode_utf8(&mut buffer), size, false, false, st.color);
        if total <= (natural.h + natural.d) * 1.04 {
            return natural;
        }
        self.drawn_delimiter(delim, total, size, st.color)
    }

    fn drawn_delimiter(&mut self, delim: char, total: f32, size: f32, color: Option<Color>) -> MBox {
        let axis = self.axis * size;
        let top = -(axis + total * 0.5);
        let bearing = 0.07 * size;
        let thick = (0.055 * size).max(1.0 / self.scale);
        let mut out = MBox { h: axis + total * 0.5, d: total * 0.5 - axis, ..MBox::default() };
        let tall = total * self.scale > MAX_PATH_PX;
        let rule = |x: f32, y: f32, w: f32, h: f32| Item::Rule { x, y, w, h, color };
        match delim {
            '|' => {
                out.items.push(rule(bearing * 1.3, top, thick, total));
                out.w = bearing * 2.6 + thick;
            }
            '\u{2016}' => {
                let gap = 0.13 * size;
                out.items.push(rule(bearing * 1.3, top, thick, total));
                out.items.push(rule(bearing * 1.3 + gap + thick, top, thick, total));
                out.w = bearing * 2.6 + thick * 2.0 + gap;
            }
            '[' | ']' | '⌊' | '⌋' | '⌈' | '⌉' => self.bracket_rules(&mut out, delim, top, total, size, thick, color),
            '(' | ')' if !tall => {
                let w = (0.33 * size + 0.05 * total).clamp(0.36 * size, 0.72 * size);
                let body = (0.075 * size + 0.012 * total).min(0.14 * size);
                let mut pen = Pen::new(self.scale);
                pen.flip_x = (delim == ')').then_some(w);
                paren(&mut pen, w, total, body, 0.03 * size);
                out.items.push(self.path(pen, bearing, top, w, total, color));
                out.w = w + bearing * 2.0;
            }
            '{' | '}' if !tall => {
                let w = (0.36 * size + 0.04 * total).clamp(0.42 * size, 0.7 * size);
                let mut pen = Pen::new(self.scale);
                pen.flip_x = (delim == '}').then_some(w);
                brace(&mut pen, w, total, thick * 1.25);
                out.items.push(self.path(pen, bearing, top, w, total, color));
                out.w = w + bearing * 2.0;
            }
            '(' | ')' | '{' | '}' => {
                // Taller than the atlas takes. Square corners read well enough at this height.
                let square = if matches!(delim, '(' | '{') { '[' } else { ']' };
                self.bracket_rules(&mut out, square, top, total, size, thick, color);
            }
            '⟨' | '⟩' if !tall => {
                let w = (0.28 * size + 0.08 * total).clamp(0.34 * size, 0.7 * size);
                let mut pen = Pen::new(self.scale);
                pen.flip_x = (delim == '⟩').then_some(w);
                angle(&mut pen, w, total, thick * 1.1);
                out.items.push(self.path(pen, bearing, top, w, total, color));
                out.w = w + bearing * 2.0;
            }
            '/' | '\\' if !tall => {
                let w = (0.25 * total).clamp(0.3 * size, 1.2 * size);
                let mut pen = Pen::new(self.scale);
                pen.flip_x = (delim == '\\').then_some(w);
                pen.m(0.0, total);
                pen.l(w - thick * 1.2, 0.0);
                pen.l(w, 0.0);
                pen.l(thick * 1.2, total);
                pen.z();
                out.items.push(self.path(pen, bearing, top, w, total, color));
                out.w = w + bearing * 2.0;
            }
            '↑' | '↓' | '↕' | '⇑' | '⇓' | '⇕' if !tall => {
                let w = 0.5 * size;
                let (up, down) = (matches!(delim, '↑' | '↕' | '⇑' | '⇕'), matches!(delim, '↓' | '↕' | '⇓' | '⇕'));
                let mut pen = Pen::new(self.scale);
                pen.swap = true;
                let head = if matches!(delim, '⇑' | '⇓' | '⇕') { ArrowHead::Double } else { ArrowHead::Single };
                arrow_shape(&mut pen, total, w, thick * 0.9, up, down, head, size);
                out.items.push(self.path(pen, bearing, top, w, total, color));
                out.w = w + bearing * 2.0;
            }
            _ => {
                let natural = self.glyph(&delim.to_string(), size, false, false, color);
                return natural;
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn bracket_rules(&self, out: &mut MBox, delim: char, top: f32, total: f32, size: f32, thick: f32, color: Option<Color>) {
        let bearing = 0.1 * size;
        let arm = (0.19 * size + 0.02 * total).min(0.36 * size);
        let thin = (thick * 0.8).max(1.0 / self.scale);
        let rule = |x: f32, y: f32, w: f32, h: f32| Item::Rule { x, y, w, h, color };
        let left = matches!(delim, '[' | '⌊' | '⌈');
        let stem = if left { bearing } else { bearing + arm + thin - thick };
        out.items.push(rule(stem, top, thick, total));
        if matches!(delim, '[' | ']' | '⌈' | '⌉') {
            out.items.push(rule(bearing, top, arm + thin, thin));
        }
        if matches!(delim, '[' | ']' | '⌊' | '⌋') {
            out.items.push(rule(bearing, top + total - thin, arm + thin, thin));
        }
        out.w = bearing * 2.0 + arm + thin;
    }

    /// TeXbook rule 12, with the mark drawn.
    fn accent(&mut self, kind: AccentKind, wide: bool, body: &[Node], st: St) -> MBox {
        let size = self.size(st);
        let set = self.seq(body, St { cramped: true, ..st });
        let slant = if set.slanted && !wide { 0.06 * size } else { 0.0 };
        let middle = set.ink_mid.unwrap_or((set.w + set.ic) * 0.5) + slant;
        let span = set.w + set.ic;
        let gap = 0.05 * size;
        let thick = self.thickness(size);
        let (w, h) = match kind {
            AccentKind::Hat | AccentKind::Check => {
                if wide { (span.max(0.4 * size), (0.14 * size + 0.03 * span).min(0.3 * size)) } else { (0.36 * size, 0.13 * size) }
            }
            AccentKind::Tilde => {
                if wide { (span.max(0.42 * size), (0.11 * size + 0.02 * span).min(0.22 * size)) } else { (0.4 * size, 0.09 * size) }
            }
            AccentKind::Acute | AccentKind::Grave => (0.16 * size, 0.17 * size),
            AccentKind::Dot => (0.1 * size, 0.1 * size),
            AccentKind::Ddot => (0.32 * size, 0.1 * size),
            AccentKind::Dddot => (0.54 * size, 0.1 * size),
            AccentKind::Ddddot => (0.76 * size, 0.1 * size),
            AccentKind::Breve => (0.34 * size, 0.12 * size),
            AccentKind::Bar => (0.42 * size, thick),
            AccentKind::Vec => (0.48 * size, 0.2 * size),
            AccentKind::Ring => (0.2 * size, 0.2 * size),
        };
        let body_h = set.h;
        let bottom = -(body_h + gap);
        let x = (middle - w * 0.5).max(-0.1 * size);
        let mut out = MBox { ink_mid: set.ink_mid, ..MBox::default() };
        let (ic, set_w) = (set.ic, set.w);
        out.place(set, 0.0, 0.0);
        let color = st.color;
        if kind == AccentKind::Bar {
            out.items.push(Item::Rule { x, y: bottom - thick, w, h: thick, color });
        } else {
            let mut pen = Pen::new(self.scale);
            accent_shape(&mut pen, kind, w, h, thick);
            out.items.push(self.path(pen, x, bottom - h, w, h, color));
        }
        out.h = out.h.max(-bottom + h);
        out.w = set_w;
        out.ic = ic;
        out
    }

    fn over(&mut self, kind: OverKind, body: &[Node], st: St) -> MBox {
        let size = self.size(st);
        let mut set = self.seq(body, St { cramped: true, ..st });
        set.close_ic();
        let w = set.w;
        let rule = self.thickness(size);
        let color = st.color;
        let mut out = MBox::default();
        let (set_h, set_d) = (set.h, set.d);
        out.place(set, 0.0, 0.0);
        match kind {
            OverKind::Overline => {
                let y = set_h + 3.0 * RULE * size;
                out.items.push(Item::Rule { x: 0.0, y: -(y + rule), w, h: rule, color });
                out.h = out.h.max(y + rule + RULE * size);
            }
            OverKind::Underline => {
                let y = set_d + 2.0 * RULE * size;
                out.items.push(Item::Rule { x: 0.0, y, w, h: rule, color });
                out.d = out.d.max(y + rule + RULE * size);
            }
            OverKind::Overbrace | OverKind::Underbrace => {
                let h = 0.32 * size;
                let gap = 0.1 * size;
                let over = kind == OverKind::Overbrace;
                let y = if over { -(set_h + gap + h) } else { set_d + gap };
                if w * self.scale > MAX_PATH_PX {
                    out.items.push(Item::Rule { x: 0.0, y: if over { y + h - rule } else { y }, w, h: rule, color });
                } else {
                    let mut pen = Pen::new(self.scale);
                    pen.swap = true;
                    pen.flip_x = (!over).then_some(h);
                    brace(&mut pen, h, w, (0.055 * size).max(1.0 / self.scale));
                    out.items.push(self.path(pen, 0.0, y, w, h, color));
                }
                if over {
                    out.h = out.h.max(set_h + gap + h);
                } else {
                    out.d = out.d.max(set_d + gap + h);
                }
            }
            OverKind::OverArrow { left, right } | OverKind::UnderArrow { left, right } => {
                let h = 0.26 * size;
                let gap = 0.08 * size;
                let over = matches!(kind, OverKind::OverArrow { .. });
                let y = if over { -(set_h + gap + h) } else { set_d + gap };
                if w * self.scale <= MAX_PATH_PX {
                    let mut pen = Pen::new(self.scale);
                    arrow_shape(&mut pen, w.max(0.4 * size), h, rule, left, right, ArrowHead::Single, size);
                    out.items.push(self.path(pen, 0.0, y, w.max(0.4 * size), h, color));
                }
                if over {
                    out.h = out.h.max(set_h + gap + h);
                } else {
                    out.d = out.d.max(set_d + gap + h);
                }
            }
            OverKind::OverParen | OverKind::UnderParen => {
                let h = (0.16 * size + 0.04 * w).min(0.32 * size);
                let gap = 0.08 * size;
                let is_over = kind == OverKind::OverParen;
                let y = if is_over { -(set_h + gap + h) } else { set_d + gap };
                if w * self.scale <= MAX_PATH_PX {
                    let mut pen = Pen::new(self.scale);
                    pen.swap = true;
                    pen.flip_x = (!is_over).then_some(h);
                    paren(&mut pen, h, w, (0.05 * size).max(1.0 / self.scale), 0.015 * size);
                    out.items.push(self.path(pen, 0.0, y, w, h, color));
                } else {
                    out.items.push(Item::Rule { x: 0.0, y: if is_over { y } else { y + h - rule }, w, h: rule, color });
                }
                if is_over {
                    out.h = out.h.max(set_h + gap + h);
                } else {
                    out.d = out.d.max(set_d + gap + h);
                }
            }
            OverKind::OverBracket { thick, height } | OverKind::UnderBracket { thick, height } => {
                let (t, tall) = ((thick * size).max(1.0 / self.scale), height * size);
                let gap = 0.1 * size;
                let is_over = matches!(kind, OverKind::OverBracket { .. });
                let rule_at = |x: f32, y: f32, w: f32, h: f32| Item::Rule { x, y, w, h, color };
                if is_over {
                    let top = -(set_h + gap + tall);
                    out.items.push(rule_at(0.0, top, w, t));
                    out.items.push(rule_at(0.0, top, t, tall));
                    out.items.push(rule_at(w - t, top, t, tall));
                    out.h = out.h.max(set_h + gap + tall);
                } else {
                    let bottom = set_d + gap + tall;
                    out.items.push(rule_at(0.0, bottom - t, w, t));
                    out.items.push(rule_at(0.0, bottom - tall, t, tall));
                    out.items.push(rule_at(w - t, bottom - tall, t, tall));
                    out.d = out.d.max(bottom);
                }
            }
        }
        out.w = w;
        out
    }

    fn x_arrow(&mut self, arrow: XArrow, above: &[Node], below: &[Node], st: St) -> MBox {
        let size = self.size(st);
        let scripted = st.scripted();
        let mut above = self.seq(above, scripted);
        let mut below = self.seq(below, St { cramped: true, ..scripted });
        above.close_ic();
        below.close_ic();
        // `xArrow`'s two numbers: how many `mu` a label stands clear of the arrow's tail and of its head.
        let mu = size / 18.0;
        let (pad_left, pad_right) = (arrow.pad.0 * mu, arrow.pad.1 * mu);
        let w = (above.w.max(below.w) + pad_left + pad_right).max(1.5 * size);
        let h = if matches!(arrow.head, ArrowHead::Harpoons | ArrowHead::ToFrom | ArrowHead::Equal) { 0.42 * size } else { 0.34 * size };
        let axis = self.axis * size;
        let top = -(axis + h * 0.5);
        let rule = self.thickness(size);
        let mut out = MBox { h: axis + h * 0.5, d: (h * 0.5 - axis).max(0.0), ..MBox::default() };
        if let ArrowHead::Glyph(glyph) = arrow.head {
            // `\Newextarrow`: what it names at the end of a rule as long as it has to be.
            let mut buffer = [0u8; 4];
            let set = self.glyph(glyph.encode_utf8(&mut buffer), size, false, false, st.color);
            let glyph_w = set.w;
            out.items.push(Item::Rule { x: 0.0, y: -axis - rule * 0.5, w: (w - glyph_w * 0.6).max(0.0), h: rule, color: st.color });
            out.place(set, w - glyph_w, 0.0);
            out.h = out.h.max(axis + h * 0.5);
            out.d = out.d.max((h * 0.5 - axis).max(0.0));
        } else if w * self.scale <= MAX_PATH_PX {
            let mut pen = Pen::new(self.scale);
            arrow_shape(&mut pen, w, h, rule, arrow.left, arrow.right, arrow.head, size);
            out.items.push(self.path(pen, 0.0, top, w, h, st.color));
        } else {
            out.items.push(Item::Rule { x: 0.0, y: -axis - rule * 0.5, w, h: rule, color: st.color });
        }
        let gap = 0.08 * size;
        let free = |set_w: f32| ((w - pad_left - pad_right - set_w) * 0.5).max(0.0) + pad_left;
        let (above_w, above_d) = (above.w, above.d);
        out.place(above, free(above_w), top - gap - above_d);
        let (below_w, below_h) = (below.w, below.h);
        out.place(below, free(below_w), top + h + gap + below_h);
        out.w = w;
        out
    }

    fn lines(&mut self, lines: &[Vec<Node>], st: St) -> MBox {
        // MathJax's `linebreaks: { inline: false }`: in the flow of a sentence a line break is nothing.
        if !self.display {
            let sets: Vec<MBox> = lines.iter().map(|line| self.seq(line, st)).collect();
            return hjoin(sets);
        }
        let size = self.size(st);
        let sets: Vec<MBox> = lines.iter().map(|line| {
            let mut set = self.seq(line, st);
            set.close_ic();
            set
        }).collect();
        let w = sets.iter().map(|b| b.w).fold(0.0, f32::max);
        let mut out = MBox::default();
        let mut y = 0.0;
        let baseline = 1.2 * size;
        let mut previous_d: Option<f32> = None;
        for set in sets {
            if let Some(previous) = previous_d {
                y += baseline.max(previous + set.h + 0.2 * size);
            }
            let (set_w, set_d) = (set.w, set.d);
            out.place(set, (w - set_w) * 0.5, y);
            previous_d = Some(set_d);
        }
        out.w = w;
        out
    }

    fn not(&mut self, inner: &Node, st: St) -> MBox {
        let size = self.size(st);
        let mut set = self.node(inner, st);
        set.close_ic();
        let (w, h) = (0.3 * size, 0.9 * size);
        let mut pen = Pen::new(self.scale);
        let thick = 0.05 * size;
        pen.m(0.0, h);
        pen.l(w - thick * 1.3, 0.0);
        pen.l(w, 0.0);
        pen.l(thick * 1.3, h);
        pen.z();
        let x = (set.w - w) * 0.5;
        let y = -(self.axis * size + h * 0.5);
        set.items.push(self.path(pen, x, y, w, h, st.color));
        set
    }

    fn color_box(&mut self, fill: Option<Color>, stroke: Option<Color>, body: &[Node], st: St) -> MBox {
        let size = self.size(st);
        let pad = 0.2 * size;
        let mut set = self.seq(body, st);
        set.close_ic();
        let (w, h, d) = (set.w + pad * 2.0, set.h + pad, set.d + pad);
        let mut out = MBox { w, h, d, ..MBox::default() };
        if let Some(fill) = fill {
            out.items.push(Item::Rule { x: 0.0, y: -h, w, h: h + d, color: Some(fill) });
        }
        if stroke.is_some() {
            self.frame(&mut out, w, h, d, stroke);
        }
        out.place(set, pad, 0.0);
        out
    }

    fn boxed(&mut self, body: &[Node], st: St) -> MBox {
        let size = self.size(st);
        let pad = 0.3 * size;
        let mut set = self.seq(body, st);
        set.close_ic();
        let (w, h, d) = (set.w + pad * 2.0, set.h + pad, set.d + pad);
        let mut out = MBox { w, h, d, ..MBox::default() };
        self.frame(&mut out, w, h, d, st.color);
        out.place(set, pad, 0.0);
        out
    }

    fn frame(&self, out: &mut MBox, w: f32, h: f32, d: f32, color: Option<Color>) {
        let t = self.thickness(self.em);
        let rule = |x: f32, y: f32, w: f32, h: f32| Item::Rule { x, y, w, h, color };
        out.items.push(rule(0.0, -h, w, t));
        out.items.push(rule(0.0, d - t, w, t));
        out.items.push(rule(0.0, -h, t, h + d));
        out.items.push(rule(w - t, -h, t, h + d));
    }

    fn line(&self, x: f32, y: f32, w: f32, h: f32, style: LineStyle, color: Option<Color>, items: &mut Vec<Item>) {
        let rule = |x: f32, y: f32, w: f32, h: f32| Item::Rule { x, y, w, h, color };
        let horizontal = w >= h;
        match style {
            LineStyle::Solid => items.push(rule(x, y, w, h)),
            LineStyle::Double => {
                let third = (if horizontal { h } else { w }) / 3.0;
                if horizontal {
                    items.push(rule(x, y, w, third));
                    items.push(rule(x, y + third * 2.0, w, third));
                } else {
                    items.push(rule(x, y, third, h));
                    items.push(rule(x + third * 2.0, y, third, h));
                }
            }
            LineStyle::Dashed | LineStyle::Dotted => {
                let (long, thick) = if horizontal { (w, h) } else { (h, w) };
                let (dash, gap) = if style == LineStyle::Dashed { (3.0 * thick, 3.0 * thick) } else { (thick, thick) };
                let (dash, gap) = (dash.max(1.0 / self.scale), gap.max(1.0 / self.scale));
                let count = ((long + gap) / (dash + gap)).floor().clamp(1.0, 2000.0);
                let step = if count > 1.0 { (long - dash) / (count - 1.0) } else { 0.0 };
                for i in 0..count as usize {
                    let at = step * i as f32;
                    items.push(if horizontal { rule(x + at, y, dash, thick) } else { rule(x, y + at, thick, dash) });
                }
            }
        }
    }

    /// A stroke from one corner to another of a `w` by `h` box at (x, y), `flip` going the other way up.
    #[allow(clippy::too_many_arguments)]
    fn diagonal(&self, x: f32, y: f32, w: f32, h: f32, t: f32, flip: bool, color: Option<Color>, items: &mut Vec<Item>) {
        if w * self.scale > MAX_PATH_PX || h * self.scale > MAX_PATH_PX {
            return;
        }
        let mut pen = Pen::new(self.scale);
        pen.flip_y = flip.then_some(h);
        pen.m(0.0, h);
        pen.l(0.0, h - t * 1.3);
        pen.l(w - t * 1.3, 0.0);
        pen.l(w, 0.0);
        pen.l(w, t * 1.3);
        pen.l(t * 1.3, h);
        pen.z();
        items.push(self.path(pen, x, y, w, h, color));
    }

    /// What `menclose` draws, as `output/common/Notation.ts` sets it out: padding around what it holds, a line as thick as it was
    /// told, and the strikes across all of it.
    fn enclose(&mut self, e: &Enclose, st: St) -> MBox {
        const PADDING: f32 = 0.2;
        const THICKNESS: f32 = 0.067;
        let size = self.size(st);
        let color = e.color.or(st.color);
        let inner = St { color: e.color.or(st.color), ..st };
        let mut set = self.seq(&e.body, inner);
        set.close_ic();
        let (w, h, d) = (set.w, set.h, set.d);
        let t = (e.thickness.unwrap_or(THICKNESS) * size).max(1.0 / self.scale);
        let p = e.padding.unwrap_or(PADDING) * size;
        let has = |n: Notation| e.notations.contains(&n);
        let framed = has(Notation::Box) || has(Notation::RoundedBox);
        // How far the drawing reaches past what it holds, on each side.
        let (mut top, mut right, mut bottom, mut left) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let side = p + t;
        if framed || has(Notation::Top) || has(Notation::Actuarial) || has(Notation::LongDiv) || has(Notation::Radical) {
            top = side;
        }
        if framed || has(Notation::Bottom) || has(Notation::Madruwb) || has(Notation::PhasorAngle) {
            bottom = side;
        }
        if framed || has(Notation::Right) || has(Notation::Actuarial) || has(Notation::Madruwb) {
            right = side;
        }
        if framed || has(Notation::Left) {
            left = side;
        }
        if has(Notation::LongDiv) {
            left = 0.45 * (h + d + 2.0 * p).max(0.8 * size) + p;
            bottom = bottom.max(p);
        }
        if has(Notation::Radical) {
            left = 0.72 * size + p;
            bottom = bottom.max(p);
        }
        if has(Notation::PhasorAngle) {
            left = 0.55 * (h + d + 2.0 * p) + p;
        }
        let arrows = [Notation::UpDiagonalArrow, Notation::NorthEastArrow, Notation::SouthEastArrow, Notation::SouthWestArrow, Notation::NorthWestArrow, Notation::UpArrow, Notation::DownArrow, Notation::LeftArrow, Notation::RightArrow, Notation::UpDownArrow, Notation::LeftRightArrow];
        let circled = has(Notation::Circle);
        if circled {
            // The ellipse that goes through the corners of what it holds, with the padding on top.
            let (rx, ry) = ((w * 0.5) * std::f32::consts::SQRT_2 + p + t, ((h + d) * 0.5) * std::f32::consts::SQRT_2 + p + t);
            left = left.max(rx - w * 0.5);
            right = right.max(rx - w * 0.5);
            top = top.max(ry - (h + d) * 0.5);
            bottom = bottom.max(ry - (h + d) * 0.5);
        }
        let reach = 0.35 * size * e.arrowhead.1;
        if e.notations.iter().any(|n| arrows.contains(n)) {
            top = top.max(reach);
            right = right.max(reach);
            bottom = bottom.max(if has(Notation::DownArrow) || has(Notation::UpDownArrow) || has(Notation::SouthEastArrow) || has(Notation::SouthWestArrow) { reach } else { 0.0 });
            left = left.max(if has(Notation::LeftArrow) || has(Notation::LeftRightArrow) || has(Notation::SouthWestArrow) || has(Notation::NorthWestArrow) { reach } else { 0.0 });
        }
        let (x0, y0) = (0.0f32, -(h + top));
        let (bw, bh) = (left + w + right, top + h + d + bottom);
        let mut out = MBox { w: bw, h: h + top, d: d + bottom, ..MBox::default() };
        let items = &mut out.items;
        if let Some(fill) = e.fill {
            items.push(Item::Rule { x: x0, y: y0, w: bw, h: bh, color: Some(fill) });
        }
        let rule = |x: f32, y: f32, w: f32, h: f32| Item::Rule { x, y, w, h, color };
        if has(Notation::Box) {
            items.push(rule(x0, y0, bw, t));
            items.push(rule(x0, y0 + bh - t, bw, t));
            items.push(rule(x0, y0, t, bh));
            items.push(rule(x0 + bw - t, y0, t, bh));
        }
        if has(Notation::RoundedBox) && bw * self.scale <= MAX_PATH_PX && bh * self.scale <= MAX_PATH_PX {
            let mut pen = Pen::new(self.scale);
            rounded_rect(&mut pen, bw, bh, 0.27 * size, t);
            items.push(self.path(pen, x0, y0, bw, bh, color));
        }
        if has(Notation::Top) || has(Notation::Actuarial) {
            items.push(rule(x0, y0, bw, t));
        }
        if has(Notation::Bottom) || has(Notation::Madruwb) {
            items.push(rule(x0, y0 + bh - t, bw, t));
        }
        if has(Notation::Right) || has(Notation::Actuarial) || has(Notation::Madruwb) {
            items.push(rule(x0 + bw - t, y0, t, bh));
        }
        if has(Notation::Left) {
            items.push(rule(x0, y0, t, bh));
        }
        if has(Notation::LongDiv) {
            items.push(rule(x0 + left * 0.6, y0, bw - left * 0.6, t));
            if bh * self.scale <= MAX_PATH_PX {
                let mut pen = Pen::new(self.scale);
                let curve = left * 0.7;
                paren_right(&mut pen, curve, bh, (0.09 * size).max(t), 0.02 * size);
                items.push(self.path(pen, x0, y0, curve, bh, color));
            }
        }
        if has(Notation::Radical)
            && let Some(pen) = self.radical_pen(0.72 * size, bh, t, size)
        {
            items.push(self.path(pen, x0, y0, 0.72 * size, bh, color));
            items.push(rule(x0 + 0.72 * size, y0, bw - 0.72 * size, t));
        }
        if has(Notation::PhasorAngle) {
            items.push(rule(x0, y0 + bh - t, bw, t));
            if left * self.scale <= MAX_PATH_PX && bh * self.scale <= MAX_PATH_PX {
                let mut pen = Pen::new(self.scale);
                pen.m(0.0, bh);
                pen.l(left * 0.8 - t, 0.0);
                pen.l(left * 0.8, 0.0);
                pen.l(t * 1.2, bh);
                pen.z();
                items.push(self.path(pen, x0, y0, left * 0.8, bh, color));
            }
        }
        if circled && bw * self.scale <= MAX_PATH_PX && bh * self.scale <= MAX_PATH_PX {
            let mut pen = Pen::new(self.scale);
            ellipse_ring(&mut pen, bw, bh, t);
            items.push(self.path(pen, x0, y0, bw, bh, color));
        }
        if has(Notation::HorizontalStrike) {
            items.push(rule(x0, y0 + bh * 0.5 - t * 0.5, bw, t));
        }
        if has(Notation::VerticalStrike) {
            items.push(rule(x0 + bw * 0.5 - t * 0.5, y0, t, bh));
        }
        let (over_x, over_y) = (0.08 * size, 0.08 * size);
        let strike_all = bw == w && bh == h + d;
        let (sx, sy, sw, sh) = if strike_all { (-over_x, -(h + over_y), w + over_x * 2.0, h + d + over_y * 2.0) } else { (x0, y0, bw, bh) };
        let up_arrow = has(Notation::UpDiagonalArrow) || has(Notation::NorthEastArrow);
        if has(Notation::UpDiagonalStrike) && !up_arrow {
            self.diagonal(sx, sy, sw, sh, t * 0.9, false, color, items);
        }
        if has(Notation::DownDiagonalStrike) {
            self.diagonal(sx, sy, sw, sh, t * 0.9, true, color, items);
        }
        // Each arrow from one side or corner across to the other, as a line and a head.
        for notation in &e.notations {
            let (dx, dy, double) = match notation {
                Notation::UpDiagonalArrow | Notation::NorthEastArrow => (1.0, -1.0, false),
                Notation::SouthEastArrow => (1.0, 1.0, false),
                Notation::SouthWestArrow => (-1.0, 1.0, false),
                Notation::NorthWestArrow => (-1.0, -1.0, false),
                Notation::UpArrow => (0.0, -1.0, false),
                Notation::DownArrow => (0.0, 1.0, false),
                Notation::LeftArrow => (-1.0, 0.0, false),
                Notation::RightArrow => (1.0, 0.0, false),
                Notation::UpDownArrow => (0.0, -1.0, true),
                Notation::LeftRightArrow => (1.0, 0.0, true),
                _ => continue,
            };
            let (cx, cy) = (w * 0.5 + 0.0, (d - h) * 0.5);
            let half_x = if dx != 0.0 { w * 0.5 + right.max(left).min(reach + p) } else { 0.0 };
            let half_y = if dy != 0.0 { (h + d) * 0.5 + top.max(bottom).min(reach + p) } else { 0.0 };
            let (tip_x, tip_y) = (cx + dx * half_x, cy + dy * half_y);
            let (tail_x, tail_y) = (cx - dx * half_x, cy - dy * half_y);
            if let Some(item) = self.arrow_between((tail_x, tail_y), (tip_x, tip_y), t * 0.9, 0.35 * size * e.arrowhead.0, 0.2 * size * e.arrowhead.1, 0.1 * size * e.arrowhead.2, double, color) {
                items.push(item);
            }
        }
        out.place(set, left, 0.0);
        out
    }

    /// A straight arrow as one path.
    #[allow(clippy::too_many_arguments)]
    fn arrow_between(&self, tail: (f32, f32), tip: (f32, f32), t: f32, long: f32, wide: f32, dart: f32, double: bool, color: Option<Color>) -> Option<Item> {
        let (min_x, max_x) = (tail.0.min(tip.0) - wide, tail.0.max(tip.0) + wide);
        let (min_y, max_y) = (tail.1.min(tip.1) - wide, tail.1.max(tip.1) + wide);
        let (bw, bh) = (max_x - min_x, max_y - min_y);
        if bw * self.scale > MAX_PATH_PX || bh * self.scale > MAX_PATH_PX {
            return None;
        }
        let (dx, dy) = (tip.0 - tail.0, tip.1 - tail.1);
        let length = (dx * dx + dy * dy).sqrt().max(0.001);
        let (ux, uy) = (dx / length, dy / length);
        let (nx, ny) = (-uy, ux);
        let mut pen = Pen::new(self.scale);
        let point = |along: f32, across: f32, from: (f32, f32)| (from.0 + ux * along + nx * across - min_x, from.1 + uy * along + ny * across - min_y);
        let mut poly = |points: &[(f32, f32)]| {
            let Some(first) = points.first() else { return };
            pen.m(first.0, first.1);
            for p in &points[1..] {
                pen.l(p.0, p.1);
            }
            pen.z();
        };
        let start = if double { long - dart } else { 0.0 };
        let end = length - (long - dart);
        poly(&[point(start, -t * 0.5, tail), point(end, -t * 0.5, tail), point(end, t * 0.5, tail), point(start, t * 0.5, tail)]);
        poly(&[point(length, 0.0, tail), point(length - long, -wide, tail), point(length - long + dart, 0.0, tail), point(length - long, wide, tail)]);
        if double {
            poly(&[point(0.0, 0.0, tail), point(long, wide, tail), point(long - dart, 0.0, tail), point(long, -wide, tail)]);
        }
        Some(self.path(pen, min_x, min_y, bw, bh, color))
    }

    /// `\bbox`: padding, then a background and a border around that.
    fn bbox(&mut self, b: &BBox, st: St) -> MBox {
        let size = self.size(st);
        let mut set = self.seq(&b.body, st);
        set.close_ic();
        let widths = b.borders.map(|border: Option<Border>| border.map_or(0.0, |x| (x.width * size).max(if x.width > 0.0 { 1.0 / self.scale } else { 0.0 })));
        let [pt, pr, pb, pl] = b.padding.map(|p| p * size);
        let (left, right, top, bottom) = (pl + widths[3], pr + widths[1], pt + widths[0], pb + widths[2]);
        let (w, h, d) = (set.w + left + right, set.h + top, set.d + bottom);
        let mut out = MBox { w, h, d, ..MBox::default() };
        if let Some(fill) = b.fill {
            out.items.push(Item::Rule { x: 0.0, y: -h, w, h: h + d, color: Some(fill) });
        }
        let mut items = std::mem::take(&mut out.items);
        let color_of = |border: &Border| border.color.or(st.color);
        if let Some(border) = &b.borders[0] {
            self.line(0.0, -h, w, widths[0], border.style, color_of(border), &mut items);
        }
        if let Some(border) = &b.borders[2] {
            self.line(0.0, d - widths[2], w, widths[2], border.style, color_of(border), &mut items);
        }
        if let Some(border) = &b.borders[3] {
            self.line(0.0, -h, widths[3], h + d, border.style, color_of(border), &mut items);
        }
        if let Some(border) = &b.borders[1] {
            self.line(w - widths[1], -h, widths[1], h + d, border.style, color_of(border), &mut items);
        }
        out.items = items;
        out.place(set, left, 0.0);
        out.w = w;
        out.h = h;
        out.d = d;
        out
    }

    /// What goes either side of a whole alignment. The `\empheqlbrace` in them is as tall as the alignment is.
    fn empheq(&mut self, left: &[Node], right: &[Node], body: &[Node], st: St) -> MBox {
        let mut set = self.seq(body, st);
        set.close_ic();
        let total = set.h + set.d;
        let axis = self.axis * self.size(st);
        let shift = (set.h - set.d) * 0.5 - axis;
        set.shift_down(shift);
        let saved = self.stretch_to.replace(total);
        let left = self.seq(left, st);
        let right = self.seq(right, st);
        self.stretch_to = saved;
        hjoin(vec![left, set, right])
    }

    /// A `prooftree`, set as `BussproofsUtil` balances it: each rule as long as the wider of its conclusion and the span from the
    /// first premise's conclusion to the last's, the conclusion under the middle of the rule, and the labels at its ends.
    fn proof_tree(&mut self, proof: &Proof, st: St) -> MBox {
        let cur = St { style: MathStyle::Display, ..st };
        let (set, ..) = self.proof_step(proof, cur);
        set
    }

    fn proof_conclusion(&mut self, proof: &Proof, st: St) -> (MBox, f32) {
        let size = self.size(st);
        let mut left = self.seq(&proof.conclusion, st);
        left.close_ic();
        let Some(right) = &proof.centered else {
            let w = left.w;
            return (left, w * 0.5);
        };
        let mut right = self.seq(right, st);
        right.close_ic();
        let gap = 0.215 * size;
        let center = left.w + gap;
        (hjoin(vec![left, MBox { w: gap * 2.0, ..MBox::default() }, right]), center)
    }

    /// The step's box, how far along it its conclusion begins and ends, and where it lines up with the rule above it.
    fn proof_step(&mut self, proof: &Proof, st: St) -> (MBox, f32, f32, Option<f32>) {
        let size = self.size(st);
        let (conclusion, center) = self.proof_conclusion(proof, st);
        let fcenter = proof.centered.is_some();
        if !proof.inference {
            let w = conclusion.w;
            return (conclusion, 0.0, w, fcenter.then_some(center));
        }
        // Premises side by side, on one line: their conclusions' baselines.
        const PREMISE_GAP: f32 = 1.6;
        let steps: Vec<_> = proof.premises.iter().map(|p| self.proof_step(p, st)).collect();
        let mut premises = MBox::default();
        let (mut x, mut span_left, mut span_right) = (0.0, f32::MAX, f32::MIN);
        let mut premise_center = None;
        let count = steps.len();
        for (set, from, to, centered) in steps {
            span_left = span_left.min(x + from);
            span_right = span_right.max(x + to);
            if count == 1 {
                premise_center = centered;
            }
            let w = set.w;
            premises.place(set, x, 0.0);
            x += w + PREMISE_GAP * size;
        }
        if count == 0 {
            (span_left, span_right) = (0.0, 0.0);
        }
        let middle = match (premise_center, fcenter) {
            (Some(at), true) => at,
            _ => (span_left + span_right) * 0.5,
        };
        // The rule reaches over both, and its labels stand off its ends.
        let c_w = conclusion.w;
        let c_from = middle - center;
        let rule_from = span_left.min(c_from);
        let rule_to = span_right.max(c_from + c_w);
        let gap = 0.215 * size;
        let left_label = proof.left.as_ref().map(|nodes| {
            let mut set = self.seq(nodes, St { style: MathStyle::Text, ..st });
            set.close_ic();
            set
        });
        let right_label = proof.right.as_ref().map(|nodes| {
            let mut set = self.seq(nodes, St { style: MathStyle::Text, ..st });
            set.close_ic();
            set
        });
        let thick = self.thickness(size);
        let clear = 0.2 * size;
        let (rows_gap_above, rows_gap_below) = if proof.line == ProofLine::None { (0.1 * size, 0.1 * size) } else { (clear, clear) };
        let mut out = MBox::default();
        // The baseline of the step is the conclusion's: the premises stand above it, or below when the root is on top.
        let (rule_y, premise_y) = if proof.root_at_top {
            let rule_y = conclusion.d + rows_gap_above;
            (rule_y, rule_y + thick + rows_gap_below + premises.h)
        } else {
            let rule_y = -(conclusion.h + rows_gap_below + thick);
            (rule_y, rule_y - rows_gap_above - premises.d)
        };
        let shift = (-rule_from + left_label.as_ref().map_or(0.0, |l| l.w + gap)).max(0.0);
        let (premise_h, premise_d) = (premises.h, premises.d);
        out.place(premises, shift, premise_y);
        out.place(conclusion, shift + c_from, 0.0);
        let rule_w = rule_to - rule_from;
        let mut items = std::mem::take(&mut out.items);
        match proof.line {
            ProofLine::Solid => items.push(Item::Rule { x: shift + rule_from, y: rule_y, w: rule_w, h: thick, color: st.color }),
            ProofLine::Dashed => self.line(shift + rule_from, rule_y, rule_w, thick, LineStyle::Dashed, st.color, &mut items),
            ProofLine::None => {}
        }
        out.items = items;
        let label_y = rule_y + thick * 0.5 + self.axis * size;
        if let Some(label) = left_label {
            let w = label.w;
            out.place(label, shift + rule_from - gap - w, label_y);
        }
        if let Some(label) = right_label {
            out.place(label, shift + rule_to + gap, label_y);
        }
        out.h = out.h.max(-premise_y + premise_h);
        out.d = out.d.max(premise_y + premise_d);
        out.w = out.w.max(shift + rule_to);
        (out, shift + c_from, shift + c_from + c_w, fcenter.then_some(shift + middle))
    }

    /// An arrow of a `\begin{CD}`. With no size, it says how small it can be: its labels and `\minCDarrowwidth`.
    fn cd_arrow(&mut self, arrow: &CdArrow, width: Option<f32>, height: Option<f32>, st: St) -> MBox {
        let size = self.size(st);
        let (least_w, least_h) = (arrow.least.0 * size, arrow.least.1 * size);
        let rule = self.thickness(size);
        let axis = self.axis * size;
        match arrow.kind {
            '>' | '<' | '=' => {
                let script = st.scripted();
                let mut above = self.seq(&arrow.above, script);
                let mut below = self.seq(&arrow.below, St { cramped: true, ..script });
                above.close_ic();
                below.close_ic();
                // `{ width: '+.67em', lspace: '.33em' }` around each label.
                let w = width.unwrap_or(0.0).max(least_w).max(above.w.max(below.w) + 0.67 * size);
                let h = 0.34 * size;
                let top = -(axis + h * 0.5);
                let mut out = MBox { w, h: axis + h * 0.5, d: (h * 0.5 - axis).max(0.0), ..MBox::default() };
                if arrow.kind == '=' {
                    let gap = 0.1 * size;
                    out.items.push(Item::Rule { x: 0.0, y: -axis - gap - rule * 0.5, w, h: rule, color: st.color });
                    out.items.push(Item::Rule { x: 0.0, y: -axis + gap - rule * 0.5, w, h: rule, color: st.color });
                } else if w * self.scale <= MAX_PATH_PX {
                    let mut pen = Pen::new(self.scale);
                    arrow_shape(&mut pen, w, h, rule, arrow.kind == '<', arrow.kind == '>', ArrowHead::Single, size);
                    out.items.push(self.path(pen, 0.0, top, w, h, st.color));
                }
                let gap = 0.1 * size;
                let (above_w, above_d) = (above.w, above.d);
                out.place(above, (w - above_w) * 0.5, top - gap - above_d);
                let (below_w, below_h) = (below.w, below.h);
                out.place(below, (w - below_w) * 0.5, top + h + gap + below_h);
                out.w = w;
                out
            }
            _ => {
                let script = St { style: MathStyle::Script, ..st };
                let mut left = self.seq(&arrow.above, script);
                let mut right = self.seq(&arrow.below, script);
                left.close_ic();
                right.close_ic();
                let total = height.unwrap_or(0.0).max(least_h);
                let top = -(axis + total * 0.5);
                let w = 0.5 * size;
                let gap = 0.15 * size;
                let lead = left.w + if left.w > 0.0 { gap } else { 0.0 };
                let mut out = MBox { h: axis + total * 0.5, d: total * 0.5 - axis, ..MBox::default() };
                if arrow.kind == '|' {
                    let at = lead + w * 0.5;
                    let apart = 0.13 * size;
                    out.items.push(Item::Rule { x: at - apart * 0.5 - rule, y: top, w: rule, h: total, color: st.color });
                    out.items.push(Item::Rule { x: at + apart * 0.5, y: top, w: rule, h: total, color: st.color });
                } else if total * self.scale <= MAX_PATH_PX {
                    let mut pen = Pen::new(self.scale);
                    pen.swap = true;
                    arrow_shape(&mut pen, total, w, rule, arrow.kind == 'A', arrow.kind == 'V', ArrowHead::Single, size);
                    out.items.push(self.path(pen, lead, top, w, total, st.color));
                }
                // `\raise.125em{\vcenter{\llap{...}}}`.
                let center = |set: &MBox| -> f32 { (set.h - set.d) * 0.5 - axis - 0.125 * size };
                let (ly, ry) = (center(&left), center(&right));
                let left_w = left.w;
                out.place(left, 0.0, ly);
                let right_at = lead + w + gap;
                let right_w = right.w;
                out.place(right, right_at, ry);
                out.w = right_at + right_w - if right_w > 0.0 { 0.0 } else { gap };
                out.w = out.w.max(left_w + w);
                out
            }
        }
    }

    fn array(&mut self, array: &Array, st: St) -> MBox {
        let cur = St { style: array.style, cramped: false, ..st };
        let size = self.size(cur);
        let outer_size = self.size(st);
        let columns = array.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
        let mut cells: Vec<Vec<MBox>> = Vec::with_capacity(array.rows.len());
        let mut widths = vec![0.0f32; columns];
        for (col, width) in array.widths.iter().enumerate() {
            if let (Some(width), Some(slot)) = (width, widths.get_mut(col)) {
                *slot = width * outer_size;
            }
        }
        let baseline = array.baseline * size;
        let (strut_h, strut_d) = (0.7 * baseline, 0.3 * baseline);
        let mut heights = Vec::with_capacity(array.rows.len());
        let mut depths = Vec::with_capacity(array.rows.len());
        for row in &array.rows {
            let mut set_row = Vec::with_capacity(row.cells.len());
            let (mut h, mut d) = (strut_h, strut_d);
            for (col, cell) in row.cells.iter().enumerate() {
                let mut set = self.seq(cell, cur);
                set.close_ic();
                // In a `multline` each row is on its own, and the column is only as wide as the widest of them.
                widths[col] = widths[col].max(set.w);
                h = h.max(set.h);
                d = d.max(set.d);
                set_row.push(set);
            }
            cells.push(set_row);
            heights.push(h);
            depths.push(d);
        }
        // Columns: the rules before each, and the gap.
        let edge_rules = array.rules.first().copied().unwrap_or(0) > 0 || array.rules.get(columns).copied().unwrap_or(0) > 0;
        let edge = if edge_rules { 0.5 * outer_size } else { 0.0 };
        let thick = self.thickness(outer_size);
        let mut xs = Vec::with_capacity(columns + 1);
        let mut gaps = vec![0.0f32; columns + 1];
        let mut x = edge;
        let is_dashed = |at: usize| array.dashed.get(at).copied().unwrap_or(false);
        let mut rule_xs: Vec<(f32, bool)> = vec![(0.0, is_dashed(0)); array.rules.first().copied().unwrap_or(0) as usize];
        for (col, width) in widths.iter().enumerate() {
            xs.push(x);
            x += width;
            if col + 1 < columns {
                let gap = if array.no_gap.get(col).copied().unwrap_or(false) {
                    0.0
                } else if array.paired {
                    if col % 2 == 1 { array.col_gap } else { 0.0 }
                } else {
                    array.col_gap
                } * outer_size;
                gaps[col] = gap;
                let count = array.rules.get(col + 1).copied().unwrap_or(0);
                for i in 0..count {
                    rule_xs.push((x + gap * 0.5 + i as f32 * thick * 3.0, is_dashed(col + 1)));
                }
                x += gap;
            }
        }
        x += edge;
        let right_rules = array.rules.get(columns).copied().unwrap_or(0);
        for i in 0..right_rules {
            rule_xs.push((x - thick - (right_rules - 1 - i) as f32 * thick * 3.0, is_dashed(columns)));
        }
        let width = x;

        let jot = array.jot * size;
        let mut ys = Vec::with_capacity(array.rows.len());
        let mut y = 0.0;
        let hline_pad = 0.1 * size;
        let mut hlines = Vec::new();
        for (i, row) in array.rows.iter().enumerate() {
            if row.rule_above {
                hlines.push((y, row.dashed_above));
                y += hline_pad;
            }
            y += heights[i];
            ys.push(y);
            y += depths[i] + row.extra * size;
            if i + 1 < array.rows.len() {
                y += jot;
            }
        }
        if array.rule_below {
            y += hline_pad;
            hlines.push((y, array.dashed_below));
        }
        let total = y.max(0.0);
        let axis = self.axis * outer_size;
        let top = match array.valign {
            VAlign::Center => -(axis + total * 0.5),
            VAlign::Top => -heights.first().copied().unwrap_or(0.0),
            VAlign::Bottom => -(total - depths.last().copied().unwrap_or(0.0)),
        };
        let mut body = MBox { h: -top, d: total + top, ..MBox::default() };
        // Backgrounds under everything: a cell's reaches half the gap to its neighbours.
        for (r, row) in array.rows.iter().enumerate() {
            let row_top = top + ys[r] - heights[r] - jot * 0.5;
            let row_h = heights[r] + depths[r] + jot;
            for c in 0..columns {
                let fill = row.fills.get(c).copied().flatten().or(row.fill).or(array.fills.get(c).copied().flatten());
                let Some(fill) = fill else { continue };
                let before = if c == 0 { edge } else { gaps[c - 1] * 0.5 };
                let after = if c + 1 == columns { edge } else { gaps[c] * 0.5 };
                body.items.push(Item::Rule { x: xs[c] - before, y: row_top, w: widths[c] + before + after, h: row_h, color: Some(fill) });
            }
        }
        let last_row = array.rows.len().saturating_sub(1);
        let on_top = self.top_array == Some(array as *const Array);
        for (r, row) in cells.into_iter().enumerate() {
            let shove = array.rows.get(r).and_then(|row| row.shove);
            for (c, mut set) in row.into_iter().enumerate() {
                let source = array.rows.get(r).and_then(|row| row.cells.get(c));
                // A diagram's arrow is as wide as its column or as tall as its row, whichever way it points.
                if array.diagram
                    && let Some([Node::CdArrow(arrow)]) = source.map(Vec::as_slice)
                {
                    set = self.cd_arrow(arrow, Some(widths[c]), Some(heights[r] + depths[r] + jot), cur);
                }
                let align = if array.multline {
                    shove.unwrap_or(if array.rows.len() == 1 {
                        'c'
                    } else if r == 0 {
                        'l'
                    } else if r == last_row {
                        'r'
                    } else {
                        'c'
                    })
                } else {
                    array.columns.get(if array.paired { c % array.columns.len().max(1) } else { c }).or(array.columns.last()).copied().unwrap_or('c')
                };
                let free = widths[c] - set.w;
                let dx = match align {
                    'l' => 0.0,
                    'r' => free,
                    _ => free * 0.5,
                };
                if array.multline && on_top {
                    let share = match align {
                        'l' => 0.0,
                        'r' => 1.0,
                        _ => 0.5,
                    };
                    let w = set.w;
                    set.items.iter_mut().for_each(|item| item.shift(0.0, top + ys[r]));
                    self.spread.push(SpreadRow { w, free: share, items: set.items });
                    continue;
                }
                body.place(set, xs[c] + dx, top + ys[r]);
            }
        }
        let mut items = std::mem::take(&mut body.items);
        for (rule_x, dashed) in rule_xs {
            let style = if dashed { LineStyle::Dashed } else { LineStyle::Solid };
            self.line(rule_x, top, thick, total, style, st.color, &mut items);
        }
        for (line_y, dashed) in hlines {
            let style = if dashed { LineStyle::Dashed } else { LineStyle::Solid };
            self.line(0.0, top + line_y, width, thick, style, st.color, &mut items);
        }
        body.items = items;
        body.w = width;
        // A `\tag` on a row: off to the side of them all, on that row's baseline. Where the formula is nothing else that side is the
        // column's, and otherwise it is the array's.
        let tag_gap = 2.0 * size;
        let mut tag_w = 0.0f32;
        let mut tags = Vec::new();
        for (r, row) in array.rows.iter().enumerate() {
            if let Some(tag) = &row.tag {
                let mut set = self.seq(tag, St { style: MathStyle::Text, ..cur });
                set.close_ic();
                tag_w = tag_w.max(set.w);
                tags.push((r, set));
            }
        }
        for (r, mut set) in tags {
            if on_top {
                set.shift_down(top + ys[r]);
                self.tags.push(set);
            } else {
                let free = tag_w - set.w;
                body.place(set, width + tag_gap + free, top + ys[r]);
                body.w = width + tag_gap + tag_w;
            }
        }
        if array.left == '\0' && array.right == '\0' {
            return body;
        }
        // The brackets are centred on the axis, and a `[t]` array has none.
        let target = total + 0.15 * outer_size;
        let mut row = Vec::with_capacity(3);
        let delim_st = St { style: if array.style == MathStyle::Script { MathStyle::Text } else { st.style }, ..st };
        let pad = 0.08 * outer_size;
        if array.left != '\0' {
            row.push(self.delimiter(array.left, target, delim_st));
            row.push(MBox { w: pad, ..MBox::default() });
        }
        row.push(body);
        if array.right != '\0' {
            row.push(MBox { w: pad, ..MBox::default() });
            row.push(self.delimiter(array.right, target, delim_st));
        }
        hjoin(row)
    }
}

/// What goes over or under: a node of a script, or the nodes of a `\overset`.
enum StackPart<'a> {
    Node(&'a Node),
    Nodes(&'a [Node]),
}

fn settle_binaries(entries: &mut [SeqEntry]) {
    let mut previous: Option<usize> = None;
    for i in 0..entries.len() {
        let SeqEntry::Atom { class, .. } = &entries[i] else { continue };
        let class = *class;
        let before = previous.and_then(|p| if let SeqEntry::Atom { class, .. } = &entries[p] { Some(*class) } else { None });
        if class == Class::Bin
            && before.is_none_or(|b| matches!(b, Class::Bin | Class::Op | Class::Rel | Class::Open | Class::Punct))
            && let SeqEntry::Atom { class, .. } = &mut entries[i]
        {
            *class = Class::Ord;
        }
        if before == Some(Class::Bin)
            && matches!(class, Class::Rel | Class::Close | Class::Punct)
            && let Some(p) = previous
            && let SeqEntry::Atom { class, .. } = &mut entries[p]
        {
            *class = Class::Ord;
        }
        previous = Some(i);
    }
    if let Some(p) = previous
        && let SeqEntry::Atom { class, .. } = &mut entries[p]
        && *class == Class::Bin
    {
        *class = Class::Ord;
    }
}

fn hjoin(boxes: Vec<MBox>) -> MBox {
    let mut out = MBox::default();
    let mut x = 0.0;
    for mut set in boxes {
        set.close_ic();
        let w = set.w;
        out.place(set, x, 0.0);
        x += w;
    }
    out.w = x;
    out
}

// ---------------------------------------------------------------- what a font's letters say

struct Shown {
    text: String,
    /// Bold and slanted by hand: there is no letter for it.
    bold: bool,
    italic: bool,
    /// The letters lean, however they got that way.
    slanted: bool,
}

fn shown_text(text: &str, font: MathFont, bold: bool, has_math: bool) -> Shown {
    let mut shown = String::with_capacity(text.len() + 8);
    let (mut any_mapped, mut any_plain, mut any_letter) = (false, false, false);
    for c in text.chars() {
        let mapped = if has_math { math_char(c, font) } else { letterlike_char(c, font) };
        any_letter |= c.is_alphabetic();
        match mapped {
            Some(m) => {
                any_mapped = true;
                shown.push(m);
            }
            None => {
                any_plain |= !c.is_whitespace();
                shown.push(c);
            }
        }
    }
    let leaning = matches!(font, MathFont::Italic | MathFont::BoldItalic | MathFont::SansItalic) || (font == MathFont::Auto && any_letter);
    let wants_bold = bold || matches!(font, MathFont::Bold | MathFont::BoldItalic | MathFont::SansBold | MathFont::BoldScript | MathFont::BoldFraktur);
    // With a math face the code points have said it all, and only what has no bold form is emboldened. Without one, a hand has
    // to do what the alphabets would have.
    let (bold, italic) = if has_math { (wants_bold && any_plain, false) } else { (wants_bold, leaning && any_letter && !any_mapped) };
    Shown { text: shown, bold, italic, slanted: leaning && any_letter }
}

/// A letter's place in the alphabets of the Mathematical Alphanumeric Symbols block, and the few that are elsewhere.
fn math_char(c: char, font: MathFont) -> Option<char> {
    let (upper, lower, digit) = match font {
        MathFont::Auto | MathFont::Italic => (0x1D434, 0x1D44E, None),
        MathFont::Roman => return None,
        MathFont::Bold => (0x1D400, 0x1D41A, Some(0x1D7CE)),
        MathFont::BoldItalic => (0x1D468, 0x1D482, Some(0x1D7CE)),
        MathFont::Sans => (0x1D5A0, 0x1D5BA, Some(0x1D7E2)),
        MathFont::SansBold => (0x1D5D4, 0x1D5EE, Some(0x1D7EC)),
        MathFont::SansItalic => (0x1D608, 0x1D622, Some(0x1D7E2)),
        MathFont::Mono => (0x1D670, 0x1D68A, Some(0x1D7F6)),
        MathFont::Script => (0x1D49C, 0x1D4B6, None),
        MathFont::BoldScript => (0x1D4D0, 0x1D4EA, None),
        MathFont::Fraktur => (0x1D504, 0x1D51E, None),
        MathFont::BoldFraktur => (0x1D56C, 0x1D586, None),
        MathFont::DoubleStruck => (0x1D538, 0x1D552, Some(0x1D7D8)),
    };
    if let Some(special) = alphabet_hole(c, font) {
        return Some(special);
    }
    let code = match c {
        'A'..='Z' => upper + (c as u32 - 'A' as u32),
        'a'..='z' => lower + (c as u32 - 'a' as u32),
        '0'..='9' => digit? + (c as u32 - '0' as u32),
        _ => return greek_char(c, font),
    };
    char::from_u32(code)
}

/// Letters the alphabets leave out, because Unicode had them first.
fn alphabet_hole(c: char, font: MathFont) -> Option<char> {
    Some(match (font, c) {
        (MathFont::Auto | MathFont::Italic, 'h') => 'ℎ',
        (MathFont::Script, 'B') => 'ℬ',
        (MathFont::Script, 'E') => 'ℰ',
        (MathFont::Script, 'F') => 'ℱ',
        (MathFont::Script, 'H') => 'ℋ',
        (MathFont::Script, 'I') => 'ℐ',
        (MathFont::Script, 'L') => 'ℒ',
        (MathFont::Script, 'M') => 'ℳ',
        (MathFont::Script, 'R') => 'ℛ',
        (MathFont::Script, 'e') => 'ℯ',
        (MathFont::Script, 'g') => 'ℊ',
        (MathFont::Script, 'o') => 'ℴ',
        (MathFont::Fraktur, 'C') => 'ℭ',
        (MathFont::Fraktur, 'H') => 'ℌ',
        (MathFont::Fraktur, 'I') => 'ℑ',
        (MathFont::Fraktur, 'R') => 'ℜ',
        (MathFont::Fraktur, 'Z') => 'ℨ',
        (MathFont::DoubleStruck, 'C') => 'ℂ',
        (MathFont::DoubleStruck, 'H') => 'ℍ',
        (MathFont::DoubleStruck, 'N') => 'ℕ',
        (MathFont::DoubleStruck, 'P') => 'ℙ',
        (MathFont::DoubleStruck, 'Q') => 'ℚ',
        (MathFont::DoubleStruck, 'R') => 'ℝ',
        (MathFont::DoubleStruck, 'Z') => 'ℤ',
        _ => return None,
    })
}

fn greek_char(c: char, font: MathFont) -> Option<char> {
    // Capitals stand upright unless a font says otherwise; small letters lean by default, as TeX's do.
    let (upper, lower) = match font {
        MathFont::Auto => (None, Some(0x1D6FC)),
        MathFont::Italic => (Some(0x1D6E2), Some(0x1D6FC)),
        MathFont::Bold => (Some(0x1D6A8), Some(0x1D6C2)),
        MathFont::BoldItalic => (Some(0x1D71C), Some(0x1D736)),
        MathFont::SansBold => (Some(0x1D756), Some(0x1D770)),
        _ => return None,
    };
    let code = match c {
        'Α'..='Ω' => upper? + (c as u32 - 'Α' as u32),
        '∇' => upper? + 25,
        'α'..='ω' => lower? + (c as u32 - 'α' as u32),
        '∂' => lower? + 25,
        'ϵ' => lower? + 26,
        'ϑ' => lower? + 27,
        'ϰ' => lower? + 28,
        'ϕ' => lower? + 29,
        'ϱ' => lower? + 30,
        'ϖ' => lower? + 31,
        'ı' => return (font == MathFont::Auto || font == MathFont::Italic).then_some('\u{1D6A4}'),
        'ȷ' => return (font == MathFont::Auto || font == MathFont::Italic).then_some('\u{1D6A5}'),
        _ => return None,
    };
    char::from_u32(code)
}

/// With no math face all that can be trusted to exist is what the Letterlike Symbols block holds.
fn letterlike_char(c: char, font: MathFont) -> Option<char> {
    match font {
        MathFont::DoubleStruck | MathFont::Script | MathFont::Fraktur => alphabet_hole(c, font),
        _ => None,
    }
}

// ---------------------------------------------------------------- shapes

/// SVG path data, written in logical units and kept in physical pixels. The flips and the swap let one drawing serve for its
/// mirror image and its rotation.
struct Pen {
    scale: f32,
    data: String,
    flip_x: Option<f32>,
    flip_y: Option<f32>,
    swap: bool,
}

impl Pen {
    fn new(scale: f32) -> Self {
        Pen { scale, data: String::with_capacity(96), flip_x: None, flip_y: None, swap: false }
    }

    fn at(&self, x: f32, y: f32) -> (f32, f32) {
        let x = self.flip_x.map_or(x, |w| w - x);
        let y = self.flip_y.map_or(y, |h| h - y);
        let (x, y) = if self.swap { (y, x) } else { (x, y) };
        (x * self.scale, y * self.scale)
    }

    fn m(&mut self, x: f32, y: f32) {
        let (x, y) = self.at(x, y);
        let _ = write!(self.data, "M{x:.2} {y:.2}");
    }

    fn l(&mut self, x: f32, y: f32) {
        let (x, y) = self.at(x, y);
        let _ = write!(self.data, "L{x:.2} {y:.2}");
    }

    fn c(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (a, b) = self.at(x1, y1);
        let (c, d) = self.at(x2, y2);
        let (e, f) = self.at(x, y);
        let _ = write!(self.data, "C{a:.2} {b:.2} {c:.2} {d:.2} {e:.2} {f:.2}");
    }

    fn z(&mut self) {
        self.data.push('Z');
    }

    fn finish(self) -> String {
        self.data
    }
}

/// `(` in a `w` by `h` box: `body` thick at its waist, `tip` at its ends.
fn paren(pen: &mut Pen, w: f32, h: f32, body: f32, tip: f32) {
    let outer = -w / 3.0;
    let inner = (4.0 * body - w) / 3.0;
    pen.m(w - tip, 0.0);
    pen.c(outer, h * 0.28, outer, h * 0.72, w - tip, h);
    pen.l(w, h);
    pen.c(inner, h * 0.72, inner, h * 0.28, w, 0.0);
    pen.z();
}

/// `{` in a `w` by `h` box, its point to the left.
fn brace(pen: &mut Pen, w: f32, h: f32, t: f32) {
    let spine = w * 0.5;
    let (l, r) = (spine - t * 0.5, spine + t * 0.5);
    let bend = (w * 0.55).min(h * 0.22);
    let mid = h * 0.5;
    // Down the outside to the point, and back up the inside.
    pen.m(w, 0.0);
    pen.c(l + (w - l) * 0.35, 0.0, l, bend * 0.25, l, bend);
    pen.l(l, mid - bend);
    pen.c(l, mid - bend * 0.3, spine * 0.55, mid - t * 0.15, 0.0, mid);
    pen.c(spine * 0.55, mid + t * 0.15, l, mid + bend * 0.3, l, mid + bend);
    pen.l(l, h - bend);
    pen.c(l, h - bend * 0.25, l + (w - l) * 0.35, h, w, h);
    pen.c(r + (w - r) * 0.4, h - t * 0.4, r, h - bend * 0.3, r, h - bend);
    pen.l(r, mid + bend);
    pen.c(r, mid + bend * 0.35, spine * 0.8, mid + t * 0.3, 0.0, mid);
    pen.c(spine * 0.8, mid - t * 0.3, r, mid - bend * 0.35, r, mid - bend);
    pen.l(r, bend);
    pen.c(r, bend * 0.3, r + (w - r) * 0.4, t * 0.4, w, 0.0);
    pen.z();
}

fn angle(pen: &mut Pen, w: f32, h: f32, t: f32) {
    // A stroke `t` thick, measured along a line across it.
    let across = t * (1.0 + (2.0 * w / h).powi(2)).sqrt();
    pen.m(w, 0.0);
    pen.l(0.0, h * 0.5);
    pen.l(w, h);
    pen.l(w - across, h);
    pen.l(across, h * 0.5);
    pen.l(w - across, 0.0);
    pen.z();
}

/// A horizontal arrow across a `w` by `h` box, with its head or heads, centred vertically.
#[allow(clippy::too_many_arguments)]
fn arrow_shape(pen: &mut Pen, w: f32, h: f32, t: f32, left: bool, right: bool, head: ArrowHead, size: f32) {
    let mid = h * 0.5;
    let reach = (0.34 * size).min(w * 0.45);
    match head {
        ArrowHead::Double => {
            let gap = (h * 0.32).max(t * 1.6);
            let (x0, x1) = (if left { reach * 0.5 } else { 0.0 }, if right { w - reach * 0.5 } else { w });
            bar(pen, x0, x1, mid - gap * 0.5 - t * 0.5, t);
            bar(pen, x0, x1, mid + gap * 0.5 - t * 0.5, t);
            if right {
                chevron(pen, w, mid, reach, gap * 0.5 + h * 0.3, t, true);
            }
            if left {
                chevron(pen, 0.0, mid, reach, gap * 0.5 + h * 0.3, t, false);
            }
        }
        ArrowHead::Harpoons => {
            let upper = mid - h * 0.2;
            let lower = mid + h * 0.2;
            bar(pen, 0.0, w, upper - t * 0.5, t);
            bar(pen, 0.0, w, lower - t * 0.5, t);
            // The upper arrow's barb above it, pointing right; the lower's below it, pointing left.
            barb(pen, w, upper, reach, -h * 0.3, t, true);
            barb(pen, 0.0, lower, reach, h * 0.3, t, false);
        }
        ArrowHead::Equal => {
            let gap = h * 0.2;
            bar(pen, 0.0, w, mid - gap - t * 0.5, t);
            bar(pen, 0.0, w, mid + gap - t * 0.5, t);
        }
        ArrowHead::ToFrom => {
            let (upper, lower) = (mid - h * 0.22, mid + h * 0.22);
            bar(pen, 0.0, w, upper - t * 0.5, t);
            bar(pen, 0.0, w, lower - t * 0.5, t);
            chevron(pen, w, upper, reach, h * 0.26, t, true);
            chevron(pen, 0.0, lower, reach, h * 0.26, t, false);
        }
        ArrowHead::Harpoon(up) => {
            let rise = if up { -h * 0.45 } else { h * 0.45 };
            bar(pen, 0.0, w, mid - t * 0.5, t);
            barb(pen, if right { w } else { 0.0 }, mid, reach, rise, t, right);
        }
        ArrowHead::TwoHead | ArrowHead::Glyph(_) => {
            bar(pen, 0.0, w, mid - t * 0.5, t);
            if right {
                chevron(pen, w, mid, reach, h * 0.48, t, true);
                chevron(pen, w - reach * 0.6, mid, reach, h * 0.48, t, true);
            }
            if left {
                chevron(pen, 0.0, mid, reach, h * 0.48, t, false);
                chevron(pen, reach * 0.6, mid, reach, h * 0.48, t, false);
            }
        }
        ArrowHead::Single | ArrowHead::Mapsto | ArrowHead::Hook => {
            bar(pen, 0.0, w, mid - t * 0.5, t);
            if right {
                chevron(pen, w, mid, reach, h * 0.48, t, true);
            }
            if left {
                chevron(pen, 0.0, mid, reach, h * 0.48, t, false);
            }
            if head == ArrowHead::Mapsto {
                bar_v(pen, 0.0, mid - h * 0.4, mid + h * 0.4, t);
            }
            if head == ArrowHead::Hook {
                let (x, dir) = if right { (0.0, 1.0) } else { (w, -1.0) };
                let bow = reach * 0.8;
                pen.m(x, mid - t * 0.5);
                pen.c(x - dir * bow, mid - t * 0.5, x - dir * bow, mid - h * 0.48, x, mid - h * 0.48);
                pen.l(x, mid - h * 0.48 + t);
                pen.c(x - dir * (bow - t), mid - h * 0.48 + t, x - dir * (bow - t), mid + t * 0.5, x, mid + t * 0.5);
                pen.z();
            }
        }
    }
}

fn bar(pen: &mut Pen, x0: f32, x1: f32, y: f32, t: f32) {
    pen.m(x0, y);
    pen.l(x1, y);
    pen.l(x1, y + t);
    pen.l(x0, y + t);
    pen.z();
}

fn bar_v(pen: &mut Pen, x: f32, y0: f32, y1: f32, t: f32) {
    pen.m(x, y0);
    pen.l(x + t, y0);
    pen.l(x + t, y1);
    pen.l(x, y1);
    pen.z();
}

/// A curved arrowhead whose point is at (x, y).
fn chevron(pen: &mut Pen, x: f32, y: f32, reach: f32, half: f32, t: f32, right: bool) {
    let dir = if right { -1.0 } else { 1.0 };
    let back = x + dir * reach;
    pen.m(x, y - t * 0.5);
    pen.c(x + dir * reach * 0.45, y - t * 0.8, x + dir * reach * 0.75, y - half * 0.6, back, y - half);
    pen.l(back + dir * t * 0.3, y - half + t * 0.5);
    pen.c(x + dir * reach * 0.95, y - half * 0.35, x + dir * reach * 0.55, y - t * 0.2, x + dir * t * 2.0, y);
    pen.c(x + dir * reach * 0.55, y + t * 0.2, x + dir * reach * 0.95, y + half * 0.35, back + dir * t * 0.3, y + half - t * 0.5);
    pen.l(back, y + half);
    pen.c(x + dir * reach * 0.75, y + half * 0.6, x + dir * reach * 0.45, y + t * 0.8, x, y + t * 0.5);
    pen.z();
}

fn barb(pen: &mut Pen, x: f32, y: f32, reach: f32, rise: f32, t: f32, right: bool) {
    let dir = if right { -1.0 } else { 1.0 };
    pen.m(x, y - t * 0.5 * rise.signum().min(0.0).abs());
    pen.c(x + dir * reach * 0.4, y, x + dir * reach * 0.7, y + rise * 0.6, x + dir * reach, y + rise);
    pen.l(x + dir * (reach + t * 0.6), y + rise - rise.signum() * t * 0.5);
    pen.c(x + dir * reach * 0.8, y + rise * 0.3, x + dir * reach * 0.5, y + rise.signum() * t, x, y + rise.signum() * t * 0.5);
    pen.z();
}

/// `)` in a `w` by `h` box, as `\enclose{longdiv}` wants it.
fn paren_right(pen: &mut Pen, w: f32, h: f32, body: f32, tip: f32) {
    pen.flip_x = Some(w);
    paren(pen, w, h, body, tip);
    pen.flip_x = None;
}

/// The outline of a rectangle with round corners, as thick as `t`.
fn rounded_rect(pen: &mut Pen, w: f32, h: f32, r: f32, t: f32) {
    const K: f32 = 0.5523;
    let r = r.min(w * 0.5).min(h * 0.5);
    pen.m(r, 0.0);
    pen.l(w - r, 0.0);
    pen.c(w - r + K * r, 0.0, w, r - K * r, w, r);
    pen.l(w, h - r);
    pen.c(w, h - r + K * r, w - r + K * r, h, w - r, h);
    pen.l(r, h);
    pen.c(r - K * r, h, 0.0, h - r + K * r, 0.0, h - r);
    pen.l(0.0, r);
    pen.c(0.0, r - K * r, r - K * r, 0.0, r, 0.0);
    pen.z();
    // Against the outer line's winding, so a non-zero fill leaves the middle.
    let i = (r - t).max(0.0);
    pen.m(i + t, t);
    pen.c(i + t - K * i, t, t, i + t - K * i, t, i + t);
    pen.l(t, h - i - t);
    pen.c(t, h - i - t + K * i, i + t - K * i, h - t, i + t, h - t);
    pen.l(w - i - t, h - t);
    pen.c(w - i - t + K * i, h - t, w - t, h - i - t + K * i, w - t, h - i - t);
    pen.l(w - t, i + t);
    pen.c(w - t, i + t - K * i, w - i - t + K * i, t, w - i - t, t);
    pen.z();
}

/// An ellipse's outline filling a `w` by `h` box.
fn ellipse_ring(pen: &mut Pen, w: f32, h: f32, t: f32) {
    const K: f32 = 0.5523;
    let (cx, cy) = (w * 0.5, h * 0.5);
    let (rx, ry) = (w * 0.5, h * 0.5);
    pen.m(cx - rx, cy);
    pen.c(cx - rx, cy - K * ry, cx - K * rx, cy - ry, cx, cy - ry);
    pen.c(cx + K * rx, cy - ry, cx + rx, cy - K * ry, cx + rx, cy);
    pen.c(cx + rx, cy + K * ry, cx + K * rx, cy + ry, cx, cy + ry);
    pen.c(cx - K * rx, cy + ry, cx - rx, cy + K * ry, cx - rx, cy);
    pen.z();
    let (ix, iy) = ((rx - t).max(0.0), (ry - t).max(0.0));
    pen.m(cx - ix, cy);
    pen.c(cx - ix, cy + K * iy, cx - K * ix, cy + iy, cx, cy + iy);
    pen.c(cx + K * ix, cy + iy, cx + ix, cy + K * iy, cx + ix, cy);
    pen.c(cx + ix, cy - K * iy, cx + K * ix, cy - iy, cx, cy - iy);
    pen.c(cx - K * ix, cy - iy, cx - ix, cy - K * iy, cx - ix, cy);
    pen.z();
}

fn circle(pen: &mut Pen, cx: f32, cy: f32, r: f32) {
    const K: f32 = 0.5523;
    pen.m(cx - r, cy);
    pen.c(cx - r, cy - K * r, cx - K * r, cy - r, cx, cy - r);
    pen.c(cx + K * r, cy - r, cx + r, cy - K * r, cx + r, cy);
    pen.c(cx + r, cy + K * r, cx + K * r, cy + r, cx, cy + r);
    pen.c(cx - K * r, cy + r, cx - r, cy + K * r, cx - r, cy);
    pen.z();
}

/// An accent, filling a `w` by `h` box.
fn accent_shape(pen: &mut Pen, kind: AccentKind, w: f32, h: f32, thick: f32) {
    match kind {
        AccentKind::Hat | AccentKind::Check => {
            if kind == AccentKind::Check {
                pen.flip_y = Some(h);
            }
            let stroke = (h * 0.34).max(thick);
            pen.m(0.0, h - stroke * 0.3);
            pen.l(w * 0.5, 0.0);
            pen.l(w, h - stroke * 0.3);
            pen.l(w - stroke * 0.5, h);
            pen.l(w * 0.5, stroke);
            pen.l(stroke * 0.5, h);
            pen.z();
        }
        AccentKind::Tilde => {
            pen.m(0.0, h);
            pen.c(w * 0.08, h * 0.25, w * 0.25, 0.0, w * 0.42, h * 0.3);
            pen.c(w * 0.58, h * 0.6, w * 0.78, h * 0.72, w * 0.92, 0.0);
            pen.l(w, 0.0);
            pen.c(w * 0.92, h * 0.75, w * 0.75, h, w * 0.58, h * 0.7);
            pen.c(w * 0.42, h * 0.4, w * 0.22, h * 0.3, w * 0.08, h);
            pen.z();
        }
        AccentKind::Acute | AccentKind::Grave => {
            if kind == AccentKind::Grave {
                pen.flip_x = Some(w);
            }
            pen.m(w * 0.5, 0.0);
            pen.l(w, 0.0);
            pen.l(w * 0.3, h);
            pen.l(0.0, h);
            pen.z();
        }
        AccentKind::Dot => circle(pen, w * 0.5, h * 0.5, w * 0.5),
        AccentKind::Ddot => {
            circle(pen, h * 0.5, h * 0.5, h * 0.5);
            circle(pen, w - h * 0.5, h * 0.5, h * 0.5);
        }
        AccentKind::Dddot => {
            circle(pen, h * 0.5, h * 0.5, h * 0.5);
            circle(pen, w * 0.5, h * 0.5, h * 0.5);
            circle(pen, w - h * 0.5, h * 0.5, h * 0.5);
        }
        AccentKind::Ddddot => {
            let step = (w - h) / 3.0;
            for i in 0..4 {
                circle(pen, h * 0.5 + step * i as f32, h * 0.5, h * 0.5);
            }
        }
        AccentKind::Breve => {
            let t = (h * 0.4).max(thick);
            pen.m(0.0, 0.0);
            pen.c(w * 0.08, h * 1.3, w * 0.92, h * 1.3, w, 0.0);
            pen.l(w - t * 0.5, 0.0);
            pen.c(w * 0.8, h * 0.7, w * 0.2, h * 0.7, t * 0.5, 0.0);
            pen.z();
        }
        AccentKind::Vec => {
            let t = thick;
            arrow_shape(pen, w, h, t, false, true, ArrowHead::Single, w * 0.85);
        }
        AccentKind::Ring => {
            let r = w * 0.5;
            circle(pen, r, r, r);
            // Against the outer circle's winding, so a non-zero fill leaves a hole.
            let t = (r * 0.35).max(thick);
            let (cx, cy, inner) = (r, r, r - t);
            const K: f32 = 0.5523;
            pen.m(cx - inner, cy);
            pen.c(cx - inner, cy + K * inner, cx - K * inner, cy + inner, cx, cy + inner);
            pen.c(cx + K * inner, cy + inner, cx + inner, cy + K * inner, cx + inner, cy);
            pen.c(cx + inner, cy - K * inner, cx + K * inner, cy - inner, cx, cy - inner);
            pen.c(cx - K * inner, cy - inner, cx - inner, cy - K * inner, cx - inner, cy);
            pen.z();
        }
        AccentKind::Bar => bar(pen, 0.0, w, 0.0, h),
    }
}
