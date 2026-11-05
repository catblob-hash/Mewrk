//! Immediate-mode widgets drawn in the Fluent idiom.
//!
//! The whole scene is rebuilt whenever anything changes, which sounds wasteful and is not: text comes from
//! caches as prebuilt glyph runs, so rebuilding is a sequence of memcpys. When nothing changes, no frame
//! happens at all. All coordinates here are logical pixels. Physical pixels appear only at the Scene.

use std::collections::HashMap;
use std::sync::Arc;

use crate::gpu::*;
use crate::text::{Doc, FxBuild, TextCtx};
use crate::theme::*;

static REDUCED_MOTION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The system's "reduce motion": whatever only decorates stops moving. `prefers-reduced-motion`, which upstream's stylesheet
/// answers with a global rule, so it is one switch here too.
pub fn reduced_motion() -> bool {
    REDUCED_MOTION.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_reduced_motion(on: bool) {
    REDUCED_MOTION.store(on, std::sync::atomic::Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub const fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect { x, y, w, h }
}

impl Rect {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    pub fn inset(&self, d: f32) -> Rect {
        rect(self.x + d, self.y + d, (self.w - d * 2.0).max(0.0), (self.h - d * 2.0).max(0.0))
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        rect(x, y, (self.right().min(o.right()) - x).max(0.0), (self.bottom().min(o.bottom()) - y).max(0.0))
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
}

// ---------------------------------------------------------------- input

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Escape,
    Tab,
    PageUp,
    PageDown,
    F(u8),
    Char(char),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyPress {
    pub key: Key,
    pub shift: bool,
    /// Command on macOS, Control elsewhere.
    pub cmd: bool,
    pub alt: bool,
    /// The physical Control key, which a terminal needs apart from `cmd`.
    pub ctrl: bool,
}

/// What happened since the previous frame.
#[derive(Default, Debug)]
pub struct Input {
    pub pointer: Option<(f32, f32)>,
    pub down: bool,
    pub pressed: bool,
    pub released: bool,
    pub double_click: bool,
    pub right_pressed: bool,
    pub middle_pressed: bool,
    pub scroll: f32,
    /// Sideways wheel, for a box that scrolls horizontally inside a leaf.
    pub scroll_x: f32,
    pub scroll_is_pixels: bool,
    /// Command or Control is held while the wheel turns, or the wheel is a pinch: pictures zoom instead of scrolling.
    pub zoom_modifier: bool,
    /// Shift is held: a press extends a selection rather than starting one.
    pub shift: bool,
    /// Option or Alt is held: in a terminal, a press moves the shell's cursor there.
    pub alt: bool,
    pub keys: Vec<KeyPress>,
    pub text: String,
    pub ime_preedit: Option<String>,
    pub ime_commit: Option<String>,
    pub dropped: Vec<std::path::PathBuf>,
    pub hovered_file: bool,
    /// What a file drag over the window carries, for a zone that says what a drop would do. Empty when unknown.
    pub hovered_paths: Vec<std::path::PathBuf>,
}

impl Input {
    pub fn end_frame(&mut self) {
        self.pressed = false;
        self.released = false;
        self.double_click = false;
        self.right_pressed = false;
        self.middle_pressed = false;
        self.scroll = 0.0;
        self.scroll_x = 0.0;
        self.keys.clear();
        self.text.clear();
        self.ime_commit = None;
        self.dropped.clear();
    }
}

pub trait Clipboard {
    fn get(&mut self) -> Option<String>;
    fn set(&mut self, text: &str);
}

/// A clipboard that goes nowhere, for headless rendering.
pub struct NoClipboard;
impl Clipboard for NoClipboard {
    fn get(&mut self) -> Option<String> {
        None
    }
    fn set(&mut self, _: &str) {}
}

/// Survives from frame to frame: hover animations and which widget holds what.
#[derive(Default)]
pub struct UiMemory {
    anim: HashMap<u64, f32, FxBuild>,
    pub hot: u64,
    pub active: u64,
    pub focus: u64,
    /// Popups drawn last frame, with their layer. They shadow lower layers for input this frame.
    popup_rects: Vec<(usize, Rect)>,
    next_popup_rects: Vec<(usize, Rect)>,
    pub open_popup: u64,
    /// Set while painting a widget that something else lies on top of at the pointer.
    pub block_pointer: bool,
    /// This frame's press landed on something that leaves focus alone (`El::keep_focus`).
    pub press_keeps_focus: bool,
    /// A text input painted with focus this frame.
    pub text_input_focused: bool,
    /// What the pointer rests on that has hover text, and since when; infinite once a press has ended it.
    pub(crate) hover_note: Option<(u64, f32)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Response {
    pub hovered: bool,
    pub pressed: bool,
    pub clicked: bool,
    pub hover_t: f32,
    pub press_t: f32,
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum CursorKind {
    #[default]
    Default,
    Hand,
    Text,
    ColResize,
    RowResize,
    Grab,
    Grabbing,
    Crosshair,
}

pub struct Ui<'a> {
    pub scene: &'a mut Scene,
    pub text: &'a mut TextCtx,
    pub theme: Theme,
    pub input: &'a Input,
    pub mem: &'a mut UiMemory,
    pub clipboard: &'a mut dyn Clipboard,
    pub scale: f32,
    pub size: (f32, f32),
    pub dt: f32,
    pub time: f32,
    pub layer: usize,
    pub clip: Rect,
    /// Set by anything still moving, which asks for another frame.
    pub animating: bool,
    /// When something scheduled (a caret blink, say) next needs a frame, in seconds from now.
    pub wake_in: Option<f32>,
    pub cursor: CursorKind,
    pub ime_area: Option<Rect>,
    /// The whole of a line the leaf being painted just cut.
    pub(crate) cut_line: Option<Arc<str>>,
}

/// The part of a picture under `r`, as `[u, v, width, height]`, when the picture covers `window` and keeps `focus` in view.
pub fn fixed_uv(r: Rect, window: Rect, size: (u32, u32), focus: (f32, f32)) -> [f32; 4] {
    let (iw, ih) = (size.0.max(1) as f32, size.1.max(1) as f32);
    let (image_aspect, window_aspect) = (iw / ih, window.w / window.h.max(1.0));
    let (uw, uh) = if image_aspect > window_aspect { (window_aspect / image_aspect, 1.0) } else { (1.0, image_aspect / window_aspect) };
    let (u0, v0) = ((1.0 - uw) * focus.0.clamp(0.0, 1.0), (1.0 - uh) * focus.1.clamp(0.0, 1.0));
    let (per_x, per_y) = (uw / window.w.max(1.0), uh / window.h.max(1.0));
    [u0 + (r.x - window.x) * per_x, v0 + (r.y - window.y) * per_y, r.w * per_x, r.h * per_y]
}

impl<'a> Ui<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(scene: &'a mut Scene, text: &'a mut TextCtx, theme: Theme, input: &'a Input, mem: &'a mut UiMemory, clipboard: &'a mut dyn Clipboard, scale: f32, size: (f32, f32), dt: f32, time: f32) -> Self {
        mem.popup_rects = std::mem::take(&mut mem.next_popup_rects);
        mem.hot = 0;
        Self {
            scene,
            text,
            theme,
            input,
            mem,
            clipboard,
            scale,
            size,
            dt,
            time,
            layer: LAYER_BASE,
            clip: rect(0.0, 0.0, size.0, size.1),
            animating: false,
            wake_in: None,
            cursor: CursorKind::Default,
            ime_area: None,
            cut_line: None,
        }
    }

    pub fn wake_after(&mut self, secs: f32) {
        self.wake_in = Some(self.wake_in.map_or(secs, |w| w.min(secs)));
    }

    #[inline]
    fn px(&self, v: f32) -> f32 {
        (v * self.scale).round()
    }

    fn phys_clip(&self) -> [f32; 4] {
        [self.px(self.clip.x), self.px(self.clip.y), self.px(self.clip.right()), self.px(self.clip.bottom())]
    }

    fn glyph_clip(&self) -> [i16; 4] {
        let [a, b, c, d] = self.phys_clip();
        let i = |v: f32| v.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        [i(a), i(b), i(c), i(d)]
    }

    pub fn with_clip<R>(&mut self, r: Rect, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.clip;
        self.clip = saved.intersect(&r);
        let out = f(self);
        self.clip = saved;
        out
    }

    // ------------------------------------------------------------ drawing

    #[allow(clippy::too_many_arguments)]
    fn push_rect(&mut self, r: Rect, radii: [f32; 4], fill: Color, fill2: Color, border: Color, border2: Color, bw: f32, kind: f32) {
        let (x0, y0) = (self.px(r.x), self.px(r.y));
        let (x1, y1) = (self.px(r.right()), self.px(r.bottom()));
        let inst = RectInst {
            rect: [x0, y0, x1 - x0, y1 - y0],
            clip: self.phys_clip(),
            radii: radii.map(|v| v * self.scale),
            fill,
            fill2,
            border,
            border2,
            params: [(bw * self.scale).max(if bw > 0.0 { 1.0 } else { 0.0 }), 0.0, kind, 0.0],
        };
        self.scene.layers[self.layer].rects.push(inst);
    }

    /// Notes how much has been drawn, for [`Self::fade_since`] or [`Self::discard_since`] to close. Marks nest: the last taken is closed first.
    pub fn paint_mark(&mut self) -> usize {
        self.scene.mark()
    }

    /// Fades whatever was drawn since `mark` was taken, as if it were one picture.
    pub fn fade_since(&mut self, mark: usize, opacity: f32) {
        self.scene.close_mark(mark, |layer, (r, g, i)| {
            for inst in &mut layer.rects[r..] {
                inst.fill = inst.fill.fade(opacity);
                inst.fill2 = inst.fill2.fade(opacity);
                inst.border = inst.border.fade(opacity);
                inst.border2 = inst.border2.fade(opacity);
            }
            for inst in &mut layer.glyphs[g..] {
                inst.color = inst.color.fade(opacity);
            }
            for draw in &mut layer.images[i..] {
                draw.inst.opacity[0] *= opacity;
            }
        });
    }

    /// Takes back whatever was drawn since `mark` was taken.
    pub fn discard_since(&mut self, mark: usize) {
        self.scene.close_mark(mark, |layer, (r, g, i)| {
            layer.rects.truncate(r);
            layer.glyphs.truncate(g);
            layer.images.truncate(i);
        });
    }

    pub fn fill(&mut self, r: Rect, radius: f32, color: Color) {
        self.push_rect(r, [radius; 4], color, color, Color::TRANSPARENT, Color::TRANSPARENT, 0.0, KIND_RECT);
    }

    /// A vertical gradient.
    pub fn fill_gradient(&mut self, r: Rect, radii: [f32; 4], top: Color, bottom: Color) {
        self.push_rect(r, radii, top, bottom, Color::TRANSPARENT, Color::TRANSPARENT, 0.0, KIND_RECT);
    }

    /// A Lucide icon by the name Mewrk's components import it under, centred on (cx, cy).
    #[allow(clippy::too_many_arguments)]
    pub fn named_icon(&mut self, name: &str, cx: f32, cy: f32, size: f32, color: Color, rotate: f32) {
        let px = (size * self.scale).round().max(1.0) as u32;
        let (pcx, pcy) = (cx * self.scale, cy * self.scale);
        let clip = self.glyph_clip();
        if let Some(mut g) = self.text.named_icon(name, pcx, pcy, px, rotate, color) {
            g.clip = clip;
            self.scene.layers[self.layer].glyphs.push(g);
        }
    }

    /// A coloured SVG centred on (cx, cy): a file-type icon, say. `key` names the drawing for the atlas.
    pub fn svg_icon(&mut self, key: &str, svg: &str, cx: f32, cy: f32, size: f32) {
        let px = (size * self.scale).round().max(1.0) as u32;
        let (pcx, pcy) = (cx * self.scale, cy * self.scale);
        let clip = self.glyph_clip();
        if let Some(mut g) = self.text.svg_icon(key, svg, pcx, pcy, px) {
            g.clip = clip;
            self.scene.layers[self.layer].glyphs.push(g);
        }
    }

    pub fn fill_radii(&mut self, r: Rect, radii: [f32; 4], color: Color) {
        self.push_rect(r, radii, color, color, Color::TRANSPARENT, Color::TRANSPARENT, 0.0, KIND_RECT);
    }

    /// A card with its own corners: `[top-left, top-right, bottom-right, bottom-left]`.
    pub fn card_radii(&mut self, r: Rect, radii: [f32; 4], fill: Color, stroke: Color) {
        self.push_rect(r, radii, fill, fill, stroke, stroke, if stroke.a > 0 { 1.0 } else { 0.0 }, KIND_RECT);
    }

    pub fn card(&mut self, r: Rect, radius: f32, fill: Color, stroke: Color) {
        self.push_rect(r, [radius; 4], fill, fill, stroke, stroke, 1.0, KIND_RECT);
    }

    /// A control surface with Fluent's elevation stroke: lit at the top, weighted at the bottom edge.
    pub fn control(&mut self, r: Rect, radius: f32, fill: Color, stroke: Color, stroke_bottom: Color) {
        self.push_rect(r, [radius; 4], fill, fill, stroke, stroke_bottom, 1.0, KIND_RECT);
    }

    pub fn acrylic(&mut self, r: Rect, radius: f32, fill: Color, stroke: Color) {
        self.push_rect(r, [radius; 4], fill, fill, stroke, stroke, 1.0, KIND_ACRYLIC);
    }

    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Color) {
        let pad = width + 2.0;
        let (ax, ay) = (x0.min(x1) - pad, y0.min(y1) - pad);
        let r = rect(ax, ay, (x0 - x1).abs() + pad * 2.0, (y0 - y1).abs() + pad * 2.0);
        // Line endpoints are relative to the centre of the quad, in physical pixels and unsnapped.
        let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
        let s = self.scale;
        let radii = [(x0 - cx) * s, (y0 - cy) * s, (x1 - cx) * s, (y1 - cy) * s];
        let inst = RectInst {
            rect: [r.x * s, r.y * s, r.w * s, r.h * s],
            clip: self.phys_clip(),
            radii,
            fill: color,
            fill2: color,
            border: Color::TRANSPARENT,
            border2: Color::TRANSPARENT,
            params: [width * s, 0.0, KIND_LINE, 0.0],
        };
        self.scene.layers[self.layer].rects.push(inst);
    }

    /// An arc. Angles are in radians, clockwise from 12 o'clock.
    #[allow(clippy::too_many_arguments)]
    pub fn arc(&mut self, cx: f32, cy: f32, radius: f32, thickness: f32, start: f32, sweep: f32, color: Color) {
        let s = self.scale;
        let inst = RectInst {
            rect: [(cx - radius) * s, (cy - radius) * s, radius * 2.0 * s, radius * 2.0 * s],
            clip: self.phys_clip(),
            radii: [start.rem_euclid(std::f32::consts::TAU), sweep, 0.0, 0.0],
            fill: color,
            fill2: color,
            border: Color::TRANSPARENT,
            border2: Color::TRANSPARENT,
            params: [thickness * s, 0.0, KIND_ARC, 0.0],
        };
        self.scene.layers[self.layer].rects.push(inst);
    }

    pub fn image(&mut self, id: ImageId, r: Rect, radius: f32, opacity: f32) {
        let s = self.scale;
        let draw = ImageDraw {
            image: id,
            inst: ImageInst {
                rect: [self.px(r.x), self.px(r.y), (r.w * s).round(), (r.h * s).round()],
                clip: self.phys_clip(),
                uv: [0.0, 0.0, 1.0, 1.0],
                radii: [radius * s; 4],
                opacity: [opacity, 0.0, 0.0, 0.0],
            },
            opaque: false,
        };
        self.scene.layers[self.layer].images.push(draw);
    }

    /// Says of the image just drawn that all its pixels are solid, so nothing under it need be drawn.
    pub fn image_is_opaque(&mut self) {
        if let Some(draw) = self.scene.layers[self.layer].images.last_mut() {
            draw.opaque = true;
        }
    }

    /// An image scaled to cover `r` without distortion, cropping whichever dimension overflows.
    pub fn image_cover(&mut self, id: ImageId, r: Rect, size: (u32, u32), radius: f32, opacity: f32) {
        let (iw, ih) = (size.0.max(1) as f32, size.1.max(1) as f32);
        let (image_aspect, rect_aspect) = (iw / ih, r.w / r.h.max(1.0));
        let (uw, uh) = if image_aspect > rect_aspect { (rect_aspect / image_aspect, 1.0) } else { (1.0, image_aspect / rect_aspect) };
        self.image(id, r, radius, opacity);
        if let Some(draw) = self.scene.layers[self.layer].images.last_mut() {
            draw.inst.uv = [(1.0 - uw) * 0.5, (1.0 - uh) * 0.5, uw, uh];
        }
    }

    /// The same, keeping `focus` (fractions of the image) in view when cropping: CSS `object-position`.
    pub fn image_cover_at(&mut self, id: ImageId, r: Rect, size: (u32, u32), focus: (f32, f32), radius: f32, opacity: f32) {
        self.image_cover(id, r, size, radius, opacity);
        if let Some(draw) = self.scene.layers[self.layer].images.last_mut() {
            let [_, _, uw, uh] = draw.inst.uv;
            draw.inst.uv = [(1.0 - uw) * focus.0.clamp(0.0, 1.0), (1.0 - uh) * focus.1.clamp(0.0, 1.0), uw, uh];
        }
    }

    /// What lies under `r` of a picture that covers `window`, fitted as `image_cover_at` fits it: `background-attachment: fixed`.
    /// Wherever `r` goes it shows the part beneath it, so a surface that moves uncovers another part of one still picture.
    #[allow(clippy::too_many_arguments)]
    pub fn image_fixed(&mut self, id: ImageId, r: Rect, radii: [f32; 4], window: Rect, size: (u32, u32), focus: (f32, f32)) {
        let s = self.scale;
        self.image(id, r, 0.0, 1.0);
        if let Some(draw) = self.scene.layers[self.layer].images.last_mut() {
            draw.inst.uv = fixed_uv(r, window, size, focus);
            draw.inst.radii = radii.map(|radius| radius * s);
        }
    }

    /// Draws one line of text with its line box's top-left at (x, y), and returns its width.
    pub fn label(&mut self, x: f32, y: f32, text: &str, ty: &TypeStyle, color: Color) -> f32 {
        let run = self.text.line(text, ty, color);
        let clip = self.glyph_clip();
        let (px, py) = (self.px(x), self.px(y));
        run.paint(&mut self.scene.layers[self.layer].glyphs, px, py, clip);
        run.width / self.scale
    }

    /// The same in a named family, whatever the chains are set to: a font menu shows each family in itself.
    #[allow(clippy::too_many_arguments)]
    pub fn family_label(&mut self, x: f32, y: f32, text: &str, family: &str, ty: &TypeStyle, color: Color) -> f32 {
        let run = self.text.family_line(text, family, ty, color);
        let clip = self.glyph_clip();
        let (px, py) = (self.px(x), self.px(y));
        run.paint(&mut self.scene.layers[self.layer].glyphs, px, py, clip);
        run.width / self.scale
    }

    pub fn measure(&mut self, text: &str, ty: &TypeStyle) -> f32 {
        self.text.line(text, ty, Color::WHITE).width / self.scale
    }

    /// Where what follows `text` starts. Unlike `measure`, this counts its trailing spaces.
    pub fn advance_of(&mut self, text: &str, ty: &TypeStyle) -> f32 {
        self.text.line(text, ty, Color::WHITE).advance / self.scale
    }

    pub fn label_centered(&mut self, r: Rect, text: &str, ty: &TypeStyle, color: Color) {
        let w = self.measure(text, ty);
        self.label(r.x + (r.w - w) * 0.5, r.y + (r.h - ty.line) * 0.5, text, ty, color);
    }

    pub fn label_vcentered(&mut self, x: f32, r: Rect, text: &str, ty: &TypeStyle, color: Color) -> f32 {
        self.label(x, r.y + (r.h - ty.line) * 0.5, text, ty, color)
    }

    pub fn label_right(&mut self, right: f32, y: f32, text: &str, ty: &TypeStyle, color: Color) {
        let w = self.measure(text, ty);
        self.label(right - w, y, text, ty, color);
    }

    /// Cuts `text` with an ellipsis so that it fits in `max_w`. Single-line only.
    pub fn label_ellipsis(&mut self, x: f32, y: f32, max_w: f32, text: &str, ty: &TypeStyle, color: Color) {
        if self.measure(text, ty) <= max_w {
            self.label(x, y, text, ty, color);
            return;
        }
        let mut chars: Vec<char> = text.chars().collect();
        // Bisect on length: each probe is a shaping call, and titles can be long.
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let probe: String = chars[..mid].iter().chain(['…'].iter()).collect();
            if self.measure(&probe, ty) <= max_w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        chars.truncate(lo);
        chars.push('…');
        let s: String = chars.into_iter().collect();
        self.label(x, y, &s, ty, color);
    }

    /// For a leaf that fits its own words to its box, as an ellipsis does: the line at `r` is cut, and this is all of it. It comes up on hover as an ellipsis line's does.
    pub fn line_was_cut(&mut self, r: Rect, whole: &str) {
        if self.input.pointer.is_some_and(|(x, y)| r.contains(x, y)) {
            self.cut_line = Some(whole.into());
        }
    }

    /// Paints a document at (x, y), culled to the current clip.
    pub fn doc(&mut self, doc: &mut Doc, x: f32, y: f32) {
        let (px, py) = (self.px(x), self.px(y));
        let clip = self.glyph_clip();
        let top = self.px(self.clip.y) - py;
        let bottom = self.px(self.clip.bottom()) - py;
        doc.paint(self.text, &mut self.scene.layers[self.layer].glyphs, px, py, clip, top..bottom);
    }

    // ------------------------------------------------------------ interaction

    /// Animates a value towards a target. τ is chosen so that it visually settles in `duration`.
    pub fn animate(&mut self, id: u64, target: f32, duration: f32) -> f32 {
        let v = self.mem.anim.entry(id).or_insert(target);
        if reduced_motion() {
            *v = target;
            return target;
        }
        let k = 1.0 - (-self.dt / (duration * 0.35).max(1e-3)).exp();
        *v += (target - *v) * k;
        if (target - *v).abs() < 0.004 {
            *v = target;
        } else {
            self.animating = true;
        }
        *v
    }

    pub fn pointer_in(&self, r: &Rect) -> bool {
        if self.mem.block_pointer {
            return false;
        }
        let Some((x, y)) = self.input.pointer else { return false };
        if !r.contains(x, y) || !self.clip.contains(x, y) {
            return false;
        }
        // Popups shadow every layer under them for input.
        !self.mem.popup_rects.iter().any(|(l, p)| *l > self.layer && p.contains(x, y))
    }

    pub fn interact(&mut self, id: u64, r: Rect) -> Response {
        let hovered = self.pointer_in(&r) && (self.mem.active == 0 || self.mem.active == id);
        if hovered {
            self.mem.hot = id;
        }
        if hovered && self.input.pressed {
            self.mem.active = id;
        }
        let pressed = self.mem.active == id;
        let mut clicked = false;
        if pressed && self.input.released {
            clicked = hovered;
            self.mem.active = 0;
        }
        let hover_t = self.animate(id, if hovered { 1.0 } else { 0.0 }, motion::FASTER);
        let press_t = self.animate(id ^ 0x5bd1_e995, if pressed && hovered { 1.0 } else { 0.0 }, motion::FASTER);
        Response { hovered, pressed, clicked, hover_t, press_t }
    }

    pub fn register_popup(&mut self, r: Rect) {
        self.mem.next_popup_rects.push((self.layer, r));
    }

    /// Whether this frame's press landed on nothing interactive. Only meaningful after everything has been drawn.
    pub fn press_unclaimed(&self) -> bool {
        self.input.pressed && self.mem.active == 0
    }

    /// Returns a centred dialog box, which takes the window's input. Draw into it with `ui.layer` set to `LAYER_MODAL`.
    pub fn modal(&mut self, w: f32, h: f32) -> Rect {
        let (w, h) = (w.min(self.size.0 - 32.0), h.min(self.size.1 - 32.0));
        self.modal_at((self.size.1 - h) * 0.42, w, h)
    }

    /// The same, at a fixed distance from the window's top: a palette stays put as its results come and go.
    pub fn modal_at(&mut self, y: f32, w: f32, h: f32) -> Rect {
        self.layer = LAYER_MODAL;
        let (ww, wh) = self.size;
        self.clip = rect(0.0, 0.0, ww, wh);
        let t = self.theme;
        self.register_popup(rect(0.0, 0.0, ww, wh));
        let (w, h) = (w.min(ww - 32.0), h.min(wh - y - 16.0));
        let r = rect((ww - w) * 0.5, y, w, h);
        self.acrylic(r, OVERLAY_RADIUS, t.flyout, t.flyout_stroke);
        r
    }

    /// Back from `modal` to the page underneath.
    pub fn end_modal(&mut self, saved_clip: Rect) {
        self.layer = LAYER_BASE;
        self.clip = saved_clip;
    }

    /// Toggles the flyout `id`. Only one is open at a time.
    pub fn toggle_popup(&mut self, id: u64) {
        self.mem.open_popup = if self.mem.open_popup == id { 0 } else { id };
    }

    /// A flyout of rows, under `anchor` (or over it when there is no room). Returns the row chosen this frame.
    /// `ignore_press` is the rectangle of whatever toggles the flyout, whose press must not also dismiss it.
    pub fn menu(&mut self, id: u64, anchor: Rect, align_right: bool, width: f32, items: &[MenuItem<'_>]) -> Option<usize> {
        if self.mem.open_popup != id {
            return None;
        }
        let t = self.theme;
        const ROW: f32 = 30.0;
        const PAD: f32 = 4.0;
        const SEP: f32 = 9.0;
        let content: f32 = items.iter().map(|i| ROW + if i.separator { SEP } else { 0.0 }).sum();
        let h = content + PAD * 2.0;
        let (ww, wh) = self.size;
        let x = if align_right { anchor.right() - width } else { anchor.x };
        let x = x.clamp(4.0, (ww - width - 4.0).max(4.0));
        let below = anchor.bottom() + 4.0;
        let y = if below + h > wh - 4.0 && anchor.y - h - 4.0 > 4.0 { anchor.y - h - 4.0 } else { below.min((wh - h - 4.0).max(4.0)) };
        let box_ = rect(x, y, width, h);
        let open_t = self.animate(id ^ 0x9911, 1.0, motion::FAST);
        let saved = (self.layer, self.clip);
        self.layer = LAYER_POPUP;
        self.clip = rect(0.0, 0.0, ww, wh);
        self.register_popup(box_);
        self.acrylic(box_, OVERLAY_RADIUS, t.flyout.fade(open_t), t.flyout_stroke.fade(open_t));
        let mut choice = None;
        let mut row_y = box_.y + PAD;
        for (i, item) in items.iter().enumerate() {
            if item.separator {
                self.fill(rect(box_.x + 8.0, row_y + 4.0, box_.w - 16.0, 1.0), 0.0, t.divider);
                row_y += SEP;
            }
            let row = rect(box_.x + PAD, row_y, box_.w - PAD * 2.0, ROW - 2.0);
            row_y += ROW;
            let row_id = id.wrapping_add(1000 + i as u64);
            let rr = self.interact(row_id, row);
            let fg = if !item.enabled {
                t.text_disabled
            } else if item.danger {
                t.critical
            } else {
                t.text_primary
            };
            if item.enabled && rr.hover_t > 0.0 {
                self.fill(row, CONTROL_RADIUS, t.subtle_hover.fade(rr.hover_t));
            }
            let mut text_x = row.x + 10.0;
            if let Some(icon) = item.icon {
                self.icon(icon, text_x + 7.0, row.y + row.h * 0.5, 14.0, if item.enabled { t.text_secondary } else { fg });
                text_x += 22.0;
            }
            let right = row.right() - 10.0;
            let mut label_right = right;
            if item.checked {
                self.icon(Icon::Check, right - 6.0, row.y + row.h * 0.5, 12.0, t.accent_text);
                label_right -= 18.0;
            }
            if !item.hint.is_empty() {
                let w = self.measure(item.hint, &CAPTION);
                self.label(label_right - w, row.y + (row.h - CAPTION.line) * 0.5, item.hint, &CAPTION, t.text_tertiary);
                label_right -= w + 8.0;
            }
            self.label_ellipsis(text_x, row.y + (row.h - BODY_SMALL.line) * 0.5, label_right - text_x, item.label, &BODY_SMALL, fg);
            if item.enabled && rr.hovered {
                self.cursor = CursorKind::Hand;
            }
            if item.enabled && rr.clicked {
                choice = Some(i);
                self.mem.open_popup = 0;
            }
        }
        let escape = self.input.keys.iter().any(|k| k.key == Key::Escape);
        if escape || ((self.input.pressed || self.input.right_pressed) && !self.pointer_in(&box_) && !anchor.contains(self.input.pointer.map_or(-1.0, |p| p.0), self.input.pointer.map_or(-1.0, |p| p.1))) {
            self.mem.open_popup = 0;
        }
        self.layer = saved.0;
        self.clip = saved.1;
        choice
    }

    fn control_colors(&self, r: &Response) -> Color {
        let t = &self.theme;
        t.control_fill.mix(t.control_fill_hover, r.hover_t).mix(t.control_fill_pressed, r.press_t)
    }

    pub fn focus_ring(&mut self, r: Rect, radius: f32) {
        let t = self.theme;
        // Fluent's double ring: a contrasting outer stroke with a separating inner one.
        let outer = rect(r.x - 3.0, r.y - 3.0, r.w + 6.0, r.h + 6.0);
        self.push_rect(outer, [radius + 3.0; 4], Color::TRANSPARENT, Color::TRANSPARENT, t.text_primary, t.text_primary, 2.0, KIND_RECT);
    }

    pub fn button(&mut self, id: u64, r: Rect, text: &str) -> Response {
        let resp = self.interact(id, r);
        let t = self.theme;
        let fill = self.control_colors(&resp);
        self.control(r, CONTROL_RADIUS, fill, t.control_stroke, t.control_stroke_bottom);
        let color = t.text_primary.mix(t.text_secondary, resp.press_t);
        self.label_centered(r, text, &BODY, color);
        if resp.hovered {
            self.cursor = CursorKind::Hand;
        }
        resp
    }

    pub fn accent_button(&mut self, id: u64, r: Rect, text: &str, enabled: bool) -> Response {
        let mut resp = self.interact(id, r);
        let t = self.theme;
        if !enabled {
            resp.clicked = false;
            self.control(r, CONTROL_RADIUS, t.control_fill_disabled, t.control_stroke, t.control_stroke);
            self.label_centered(r, text, &BODY, t.text_disabled);
            return resp;
        }
        let fill = t.accent.mix(t.accent_hover, resp.hover_t).mix(t.accent_pressed, resp.press_t);
        self.control(r, CONTROL_RADIUS, fill, Color::argb(0x14FFFFFF), Color::argb(0x66000000));
        self.label_centered(r, text, &BODY_STRONG, t.text_on_accent);
        if resp.hovered {
            self.cursor = CursorKind::Hand;
        }
        resp
    }

    /// A borderless button holding an icon, as in a command bar.
    pub fn icon_button(&mut self, id: u64, r: Rect, icon: Icon, tint: Option<Color>) -> Response {
        let resp = self.interact(id, r);
        let t = self.theme;
        if resp.hover_t > 0.0 {
            let fill = t.subtle_hover.mix(t.subtle_pressed, resp.press_t).fade(resp.hover_t);
            self.fill(r, CONTROL_RADIUS, fill);
        }
        let color = tint.unwrap_or(t.text_primary).mix(t.text_secondary, resp.press_t);
        self.icon(icon, r.x + r.w * 0.5, r.y + r.h * 0.5, 16.0, color);
        if resp.hovered {
            self.cursor = CursorKind::Hand;
        }
        resp
    }

    /// A row in the navigation pane, with the sliding accent pill when selected.
    pub fn nav_item(&mut self, id: u64, r: Rect, icon: Icon, text: &str, selected: bool) -> Response {
        let resp = self.interact(id, r);
        let t = self.theme;
        let sel = self.animate(id ^ 0x77aa, if selected { 1.0 } else { 0.0 }, motion::FAST);
        let base = if selected { t.subtle_hover } else { Color::TRANSPARENT };
        let fill = base.mix(t.subtle_hover, resp.hover_t).mix(t.subtle_pressed, resp.press_t);
        if fill.a > 0 {
            self.fill(r, CONTROL_RADIUS, fill);
        }
        if sel > 0.01 {
            let h = 16.0 * motion::decelerate(sel);
            self.fill(rect(r.x, r.y + (r.h - h) * 0.5, 3.0, h), 1.5, t.accent);
        }
        self.icon(icon, r.x + 22.0, r.y + r.h * 0.5, 16.0, t.text_primary);
        self.label_vcentered(r.x + 44.0, r, text, &BODY, t.text_primary);
        if resp.hovered {
            self.cursor = CursorKind::Hand;
        }
        resp
    }

    pub fn progress_ring(&mut self, cx: f32, cy: f32, radius: f32, thickness: f32) {
        // WinUI's indeterminate ring: a head and tail that chase each other.
        let t = self.time;
        let phase = (t * 0.75).fract();
        let head = motion::ease((phase * 2.0).min(1.0));
        let tail = motion::ease(((phase - 0.5) * 2.0).max(0.0));
        let spin = t * 2.2;
        let sweep = ((head - tail) * 4.6 + 0.35).min(5.9);
        self.arc(cx, cy, radius, thickness, spin + tail * std::f32::consts::TAU, sweep, self.theme.accent);
        self.animating = true;
    }

    pub fn progress_bar(&mut self, r: Rect, value: f32, color: Color) {
        let track = self.theme.control_strong_stroke.fade(0.35);
        self.fill(rect(r.x, r.y + (r.h - 1.0) * 0.5, r.w, 1.0), 0.5, track);
        if value > 0.0 {
            self.fill(rect(r.x, r.y, r.w * value.clamp(0.0, 1.0), r.h), r.h * 0.5, color);
        }
    }

    pub fn toggle(&mut self, id: u64, r: Rect, on: &mut bool) -> Response {
        let resp = self.interact(id, r);
        if resp.clicked {
            *on = !*on;
        }
        if resp.hovered {
            self.cursor = CursorKind::Hand;
        }
        let t = self.theme;
        let v = self.animate(id ^ 0x3311, if *on { 1.0 } else { 0.0 }, motion::FAST);
        let track = rect(r.x, r.y + (r.h - 20.0) * 0.5, 40.0, 20.0);
        let off_fill = t.control_fill.mix(t.control_fill_hover, resp.hover_t);
        let fill = off_fill.mix(t.accent, v);
        let stroke = t.control_strong_stroke.mix(t.accent, v);
        self.card(track, 10.0, fill, stroke);
        let knob_size = 12.0 + 2.0 * resp.hover_t;
        let knob_x = track.x + 4.0 + (track.w - 8.0 - knob_size) * motion::decelerate(v);
        let knob_color = t.text_secondary.mix(t.text_on_accent, v);
        self.fill(rect(knob_x, track.y + (track.h - knob_size) * 0.5, knob_size, knob_size), knob_size * 0.5, knob_color);
        resp
    }

    /// Combo box. Returns the new selection when the user makes one.
    pub fn combo(&mut self, id: u64, r: Rect, options: &[&str], selected: usize) -> Option<usize> {
        let resp = self.interact(id, r);
        let t = self.theme;
        let fill = self.control_colors(&resp);
        self.control(r, CONTROL_RADIUS, fill, t.control_stroke, t.control_stroke_bottom);
        let label = options.get(selected).copied().unwrap_or("");
        self.label_vcentered(r.x + 11.0, r, label, &BODY, t.text_primary);
        self.icon(Icon::ChevronDown, r.right() - 16.0, r.y + r.h * 0.5, 10.0, t.text_secondary);
        if resp.hovered {
            self.cursor = CursorKind::Hand;
        }
        let was_open = self.mem.open_popup == id;
        if resp.clicked {
            self.mem.open_popup = if was_open { 0 } else { id };
        }
        if self.mem.open_popup != id {
            return None;
        }

        let row_h = 36.0;
        let pad = 4.0;
        let h = options.len() as f32 * row_h + pad * 2.0;
        // Open so that the current choice sits over the control, while staying inside the window.
        let y = (r.y - pad - selected as f32 * row_h).clamp(4.0, (self.size.1 - h - 4.0).max(4.0));
        let menu = rect(r.x - pad, y, r.w + pad * 2.0, h);
        let open_t = self.animate(id ^ 0x9911, 1.0, motion::FAST);
        let saved = (self.layer, self.clip);
        self.layer = LAYER_POPUP;
        self.clip = rect(0.0, 0.0, self.size.0, self.size.1);
        self.register_popup(menu);
        self.acrylic(menu, OVERLAY_RADIUS, t.flyout.fade(open_t), t.flyout_stroke.fade(open_t));
        let mut choice = None;
        for (i, option) in options.iter().enumerate() {
            let row = rect(menu.x + pad, menu.y + pad + i as f32 * row_h, menu.w - pad * 2.0, row_h - 2.0);
            let row_id = id.wrapping_add(1000 + i as u64);
            let rr = self.interact(row_id, row);
            let base = if i == selected { t.subtle_hover } else { Color::TRANSPARENT };
            let fill = base.mix(t.subtle_hover, rr.hover_t);
            if fill.a > 0 {
                self.fill(row, CONTROL_RADIUS, fill);
            }
            if i == selected {
                self.fill(rect(row.x, row.y + 9.0, 3.0, row.h - 18.0), 1.5, t.accent);
            }
            self.label_vcentered(row.x + 12.0, row, option, &BODY, t.text_primary.fade(open_t));
            if rr.hovered {
                self.cursor = CursorKind::Hand;
            }
            if rr.clicked {
                choice = Some(i);
                self.mem.open_popup = 0;
            }
        }
        // A press anywhere outside dismisses it, unless that press is what opened it just now.
        if self.input.pressed && !resp.hovered && !self.pointer_in(&menu) {
            self.mem.open_popup = 0;
        }
        self.layer = saved.0;
        self.clip = saved.1;
        choice
    }

    pub fn info_bar(&mut self, r: Rect, severity: Severity, title: &str, message: &str) {
        let t = self.theme;
        let (bg, fg) = match severity {
            Severity::Info => (t.attention_bg, t.attention),
            Severity::Success => (t.success_bg, t.success),
            Severity::Warning => (t.caution_bg, t.caution),
            Severity::Error => (t.critical_bg, t.critical),
        };
        self.card(r, CONTROL_RADIUS, bg, t.card_stroke);
        let cy = r.y + r.h * 0.5;
        self.fill(rect(r.x + 14.0, cy - 8.0, 16.0, 16.0), 8.0, fg);
        self.label_centered(rect(r.x + 14.0, cy - 10.0, 16.0, 20.0), if severity == Severity::Success { "✓" } else { "!" }, &CAPTION_STRONG, if t.dark { Color::BLACK } else { Color::WHITE });
        let w = self.label_vcentered(r.x + 42.0, r, title, &BODY_STRONG, t.text_primary);
        if !message.is_empty() {
            let x = r.x + 42.0 + w + 12.0;
            let y = r.y + (r.h - BODY.line) * 0.5;
            self.label_ellipsis(x, y, (r.right() - x - 12.0).max(0.0), message, &BODY, t.text_secondary);
        }
    }

    pub fn sparkline(&mut self, r: Rect, values: impl ExactSizeIterator<Item = f32>, max: f32, color: Color) {
        let n = values.len();
        if n < 2 || max <= 0.0 {
            return;
        }
        let dx = r.w / (n - 1) as f32;
        let mut prev: Option<(f32, f32)> = None;
        for (i, v) in values.enumerate() {
            let x = r.x + i as f32 * dx;
            let y = r.bottom() - (v / max).clamp(0.0, 1.0) * r.h;
            if let Some((px, py)) = prev {
                self.line(px, py, x, y, 1.5, color);
            }
            prev = Some((x, y));
        }
    }

    /// Stroke icons in the Fluent System regular weight, built from the line and arc primitives.
    pub fn icon(&mut self, icon: Icon, cx: f32, cy: f32, size: f32, color: Color) {
        let s = size / 16.0;
        let w = 1.25 * s.max(0.75);
        let p = |x: f32, y: f32| (cx + x * s, cy + y * s);
        macro_rules! seg {
            ($($x0:expr, $y0:expr, $x1:expr, $y1:expr);* $(;)?) => {
                $(let (a, b) = p($x0, $y0); let (c, d) = p($x1, $y1); self.line(a, b, c, d, w, color);)*
            };
        }
        let tau = std::f32::consts::TAU;
        match icon {
            Icon::Chat => {
                seg!(-6.0, -5.0, 6.0, -5.0; 7.0, -4.0, 7.0, 3.0; 6.0, 4.0, -1.0, 4.0; -1.0, 4.0, -4.5, 7.0; -4.5, 7.0, -4.5, 4.0; -4.5, 4.0, -6.0, 4.0; -7.0, 3.0, -7.0, -4.0);
                self.arc(cx - 6.0 * s, cy - 4.0 * s, 1.0 * s + w * 0.5, w, tau * 0.75, tau * 0.25, color);
                self.arc(cx + 6.0 * s, cy - 4.0 * s, 1.0 * s + w * 0.5, w, 0.0, tau * 0.25, color);
                self.arc(cx + 6.0 * s, cy + 3.0 * s, 1.0 * s + w * 0.5, w, tau * 0.25, tau * 0.25, color);
                self.arc(cx - 6.0 * s, cy + 3.0 * s, 1.0 * s + w * 0.5, w, tau * 0.5, tau * 0.25, color);
            }
            Icon::Grid => {
                for (x, y) in [(-6.5, -6.5), (1.5, -6.5), (-6.5, 1.5), (1.5, 1.5)] {
                    let (a, b) = p(x, y);
                    self.push_rect(rect(a, b, 5.0 * s, 5.0 * s), [1.0 * s; 4], Color::TRANSPARENT, Color::TRANSPARENT, color, color, w, KIND_RECT);
                }
            }
            Icon::Settings => {
                self.arc(cx, cy, 2.5 * s + w * 0.5, w, 0.0, tau, color);
                self.arc(cx, cy, 6.0 * s + w * 0.5, w, 0.0, tau, color);
                for i in 0..8 {
                    let a = i as f32 / 8.0 * tau;
                    let (sn, cs) = a.sin_cos();
                    seg!(sn * 6.3, cs * 6.3, sn * 7.8, cs * 7.8);
                }
            }
            Icon::Send => {
                seg!(-7.0, -6.0, 7.0, 0.0; 7.0, 0.0, -7.0, 6.0; -7.0, 6.0, -5.0, 0.0; -5.0, 0.0, -7.0, -6.0; -5.0, 0.0, 1.0, 0.0);
            }
            Icon::Stop => {
                let (a, b) = p(-5.0, -5.0);
                self.fill(rect(a, b, 10.0 * s, 10.0 * s), 1.5 * s, color);
            }
            Icon::Attach => {
                seg!(5.0, -2.0, 5.0, 3.0; -1.0, -2.0, -1.0, 3.0; 2.0, -5.0, 2.0, 2.0);
                self.arc(cx + 2.0 * s, cy + 3.0 * s, 3.0 * s + w * 0.5, w, tau * 0.25, tau * 0.5, color);
                self.arc(cx + 1.0 * s, cy - 5.0 * s, 1.0 * s + w * 0.5, w, tau * 0.75, tau * 0.25, color);
            }
            Icon::Image => {
                let (a, b) = p(-7.0, -6.0);
                self.push_rect(rect(a, b, 14.0 * s, 12.0 * s), [1.5 * s; 4], Color::TRANSPARENT, Color::TRANSPARENT, color, color, w, KIND_RECT);
                seg!(-6.5, 4.0, -2.0, 0.0; -2.0, 0.0, 1.0, 3.0; 1.0, 3.0, 3.0, 1.0; 3.0, 1.0, 6.5, 4.0);
                self.arc(cx + 3.0 * s, cy - 2.5 * s, 1.0 * s + w * 0.5, w, 0.0, tau, color);
            }
            Icon::Video => {
                let (a, b) = p(-7.5, -5.0);
                self.push_rect(rect(a, b, 10.0 * s, 10.0 * s), [1.5 * s; 4], Color::TRANSPARENT, Color::TRANSPARENT, color, color, w, KIND_RECT);
                seg!(2.5, -1.5, 7.5, -4.0; 7.5, -4.0, 7.5, 4.0; 7.5, 4.0, 2.5, 1.5);
            }
            Icon::Close => {
                seg!(-5.0, -5.0, 5.0, 5.0; 5.0, -5.0, -5.0, 5.0);
            }
            Icon::ChevronDown => {
                seg!(-5.0, -2.5, 0.0, 2.5; 0.0, 2.5, 5.0, -2.5);
            }
            Icon::ChevronRight => {
                seg!(-2.5, -5.0, 2.5, 0.0; 2.5, 0.0, -2.5, 5.0);
            }
            Icon::Play => {
                seg!(-4.0, -6.0, 6.0, 0.0; 6.0, 0.0, -4.0, 6.0; -4.0, 6.0, -4.0, -6.0);
            }
            Icon::Copy => {
                let (a, b) = p(-3.5, -7.0);
                self.push_rect(rect(a, b, 10.5 * s, 11.0 * s), [1.5 * s; 4], Color::TRANSPARENT, Color::TRANSPARENT, color, color, w, KIND_RECT);
                seg!(-6.5, -3.5, -6.5, 6.0; -6.5, 7.0, 2.5, 7.0);
            }
            Icon::Tool => {
                seg!(-6.0, 6.0, 1.0, -1.0);
                self.arc(cx + 3.0 * s, cy - 3.0 * s, 3.5 * s + w * 0.5, w, tau * 0.0, tau * 0.75, color);
            }
            Icon::Pulse => {
                seg!(-7.5, 0.5, -3.5, 0.5; -3.5, 0.5, -1.5, -5.5; -1.5, -5.5, 1.5, 5.5; 1.5, 5.5, 3.5, 0.5; 3.5, 0.5, 7.5, 0.5);
            }
            Icon::Check => {
                seg!(-6.0, 0.5, -2.0, 4.5; -2.0, 4.5, 6.0, -4.5);
            }
            Icon::SidebarToggle => {
                let (a, b) = p(-7.0, -6.0);
                self.push_rect(rect(a, b, 14.0 * s, 12.0 * s), [2.0 * s; 4], Color::TRANSPARENT, Color::TRANSPARENT, color, color, w, KIND_RECT);
                seg!(-2.5, -5.5, -2.5, 5.5);
            }
            Icon::ArrowLeft => {
                seg!(-6.0, 0.0, 6.0, 0.0; -6.0, 0.0, -1.5, -4.5; -6.0, 0.0, -1.5, 4.5);
            }
            Icon::ArrowRight => {
                seg!(-6.0, 0.0, 6.0, 0.0; 6.0, 0.0, 1.5, -4.5; 6.0, 0.0, 1.5, 4.5);
            }
            Icon::ArrowUp => {
                seg!(0.0, 6.0, 0.0, -6.0; 0.0, -6.0, -4.5, -1.5; 0.0, -6.0, 4.5, -1.5);
            }
            Icon::Search => {
                self.arc(cx - 1.5 * s, cy - 1.5 * s, 4.5 * s + w * 0.5, w, 0.0, tau, color);
                seg!(2.0, 2.0, 6.5, 6.5);
            }
            Icon::Plus => {
                seg!(-6.0, 0.0, 6.0, 0.0; 0.0, -6.0, 0.0, 6.0);
            }
            Icon::Folder => {
                seg!(-7.0, -5.0, -2.5, -5.0; -2.5, -5.0, -1.0, -3.5; -1.0, -3.5, 7.0, -3.5; 7.0, -3.5, 7.0, 5.5; 7.0, 5.5, -7.0, 5.5; -7.0, 5.5, -7.0, -5.0);
            }
            Icon::FolderPlus => {
                seg!(-7.0, -5.0, -2.5, -5.0; -2.5, -5.0, -1.0, -3.5; -1.0, -3.5, 7.0, -3.5; 7.0, -3.5, 7.0, 5.5; 7.0, 5.5, -7.0, 5.5; -7.0, 5.5, -7.0, -5.0; -2.5, 1.0, 2.5, 1.0; 0.0, -1.5, 0.0, 3.5);
            }
            Icon::File => {
                seg!(-5.0, -7.0, 1.5, -7.0; 1.5, -7.0, 5.0, -3.5; 5.0, -3.5, 5.0, 7.0; 5.0, 7.0, -5.0, 7.0; -5.0, 7.0, -5.0, -7.0; 1.5, -7.0, 1.5, -3.5; 1.5, -3.5, 5.0, -3.5);
            }
            Icon::Terminal => {
                let (a, b) = p(-7.0, -6.0);
                self.push_rect(rect(a, b, 14.0 * s, 12.0 * s), [2.0 * s; 4], Color::TRANSPARENT, Color::TRANSPARENT, color, color, w, KIND_RECT);
                seg!(-4.0, -2.5, -1.5, 0.0; -1.5, 0.0, -4.0, 2.5; 0.5, 2.5, 4.0, 2.5);
            }
            Icon::GitCompare => {
                self.arc(cx - 4.67 * s, cy - 4.0 * s, 1.5 * s + w * 0.5, w, 0.0, tau, color);
                self.arc(cx + 4.67 * s, cy + 4.0 * s, 1.5 * s + w * 0.5, w, 0.0, tau, color);
                seg!(0.0, -4.0, 3.3, -4.0; 3.3, -4.0, 4.67, -2.67; 4.67, -2.67, 4.67, 2.0; 2.0, -2.0, 0.0, -4.0; 0.0, -4.0, 2.0, -6.0);
                seg!(0.0, 4.0, -3.3, 4.0; -3.3, 4.0, -4.67, 2.67; -4.67, 2.67, -4.67, -2.0; -2.0, 2.0, 0.0, 4.0; 0.0, 4.0, -2.0, 6.0);
            }
            Icon::Globe => {
                self.arc(cx, cy, 7.0 * s + w * 0.5, w, 0.0, tau, color);
                seg!(-7.0, 0.0, 7.0, 0.0);
                let n = 12;
                let mut prev: Option<((f32, f32), (f32, f32))> = None;
                for i in 0..=n {
                    let a = i as f32 / n as f32 * tau * 0.5;
                    let (sn, cs) = a.sin_cos();
                    let (x, y) = (sn * 3.0, -cs * 7.0);
                    let (l, r) = (p(-x, y), p(x, y));
                    if let Some((pl, pr)) = prev {
                        self.line(pl.0, pl.1, l.0, l.1, w, color);
                        self.line(pr.0, pr.1, r.0, r.1, w, color);
                    }
                    prev = Some((l, r));
                }
            }
            Icon::MoreV | Icon::MoreH => {
                let r = 1.1 * s;
                for i in [-1.0f32, 0.0, 1.0] {
                    let (dx, dy) = if icon == Icon::MoreV { (0.0, i * 4.5) } else { (i * 4.5, 0.0) };
                    let (a, b) = p(dx, dy);
                    self.fill(rect(a - r, b - r, r * 2.0, r * 2.0), r, color);
                }
            }
            Icon::Expand => {
                seg!(1.0, -1.0, 6.5, -6.5; 2.5, -6.5, 6.5, -6.5; 6.5, -6.5, 6.5, -2.5; -1.0, 1.0, -6.5, 6.5; -6.5, 2.5, -6.5, 6.5; -6.5, 6.5, -2.5, 6.5);
            }
            Icon::Collapse => {
                seg!(6.5, -6.5, 1.0, -1.0; 1.0, -5.0, 1.0, -1.0; 1.0, -1.0, 5.0, -1.0; -6.5, 6.5, -1.0, 1.0; -5.0, 1.0, -1.0, 1.0; -1.0, 1.0, -1.0, 5.0);
            }
            Icon::History => {
                self.arc(cx, cy, 6.5 * s + w * 0.5, w, tau * 0.85, tau * 0.8, color);
                seg!(0.0, -3.5, 0.0, 0.0; 0.0, 0.0, 2.5, 1.5; -6.5, -6.0, -6.2, -2.5; -6.2, -2.5, -2.8, -3.0);
            }
            Icon::Clock => {
                self.arc(cx, cy, 6.5 * s + w * 0.5, w, 0.0, tau, color);
                seg!(0.0, -3.5, 0.0, 0.0; 0.0, 0.0, 2.5, 1.5);
            }
            Icon::ListChecks => {
                seg!(-6.5, -4.0, -5.0, -2.5; -5.0, -2.5, -2.5, -5.5; 0.0, -4.0, 7.0, -4.0; -6.5, 3.0, -5.0, 4.5; -5.0, 4.5, -2.5, 1.5; 0.0, 3.0, 7.0, 3.0);
            }
            Icon::Sliders => {
                seg!(-7.0, -3.5, 0.5, -3.5; 3.5, -3.5, 7.0, -3.5; -7.0, 3.5, -3.5, 3.5; -0.5, 3.5, 7.0, 3.5);
                self.arc(cx + 2.0 * s, cy - 3.5 * s, 1.5 * s + w * 0.5, w, 0.0, tau, color);
                self.arc(cx - 2.0 * s, cy + 3.5 * s, 1.5 * s + w * 0.5, w, 0.0, tau, color);
            }
            Icon::Bulb => {
                self.arc(cx, cy - 1.5 * s, 4.5 * s + w * 0.5, w, tau * 0.6, tau * 0.8, color);
                seg!(-2.0, 2.5, -2.0, 4.0; 2.0, 2.5, 2.0, 4.0; -2.0, 4.0, 2.0, 4.0; -1.5, 6.0, 1.5, 6.0);
            }
            Icon::Agents => {
                let (a, b) = p(-6.5, -6.5);
                self.push_rect(rect(a, b, 5.0 * s, 5.0 * s), [1.0 * s; 4], Color::TRANSPARENT, Color::TRANSPARENT, color, color, w, KIND_RECT);
                let (a, b) = p(1.5, 1.5);
                self.push_rect(rect(a, b, 5.0 * s, 5.0 * s), [1.0 * s; 4], Color::TRANSPARENT, Color::TRANSPARENT, color, color, w, KIND_RECT);
                seg!(-4.0, -1.5, -4.0, 4.0; -4.0, 4.0, 1.5, 4.0);
            }
            Icon::Branch => {
                self.arc(cx - 3.5 * s, cy - 5.0 * s, 1.5 * s + w * 0.5, w, 0.0, tau, color);
                self.arc(cx - 3.5 * s, cy + 5.0 * s, 1.5 * s + w * 0.5, w, 0.0, tau, color);
                self.arc(cx + 4.0 * s, cy - 3.0 * s, 1.5 * s + w * 0.5, w, 0.0, tau, color);
                seg!(-3.5, -3.3, -3.5, 3.3; 4.0, -1.3, 4.0, -0.5; 4.0, -0.5, -3.0, 2.0);
            }
            Icon::Refresh => {
                self.arc(cx, cy, 6.0 * s + w * 0.5, w, tau * 0.1, tau * 0.8, color);
                seg!(6.0, -6.5, 6.0, -2.5; 6.0, -2.5, 2.0, -2.5);
            }
            Icon::Trash => {
                seg!(-6.0, -4.0, 6.0, -4.0; -2.0, -4.0, -2.0, -6.5; -2.0, -6.5, 2.0, -6.5; 2.0, -6.5, 2.0, -4.0; -4.5, -4.0, -4.0, 6.5; -4.0, 6.5, 4.0, 6.5; 4.0, 6.5, 4.5, -4.0);
            }
            Icon::Pencil => {
                seg!(-6.0, 6.0, -5.5, 3.0; -5.5, 3.0, 3.0, -5.5; 3.0, -5.5, 5.5, -3.0; 5.5, -3.0, -3.0, 5.5; -3.0, 5.5, -6.0, 6.0);
            }
            Icon::Warning => {
                seg!(0.0, -6.5, 7.0, 6.0; 7.0, 6.0, -7.0, 6.0; -7.0, 6.0, 0.0, -6.5; 0.0, -2.0, 0.0, 2.0);
                let (a, b) = p(0.0, 4.0);
                self.fill(rect(a - 0.8 * s, b - 0.8 * s, 1.6 * s, 1.6 * s), 0.8 * s, color);
            }
            Icon::Info => {
                self.arc(cx, cy, 6.5 * s + w * 0.5, w, 0.0, tau, color);
                seg!(0.0, -0.5, 0.0, 3.5);
                let (a, b) = p(0.0, -3.0);
                self.fill(rect(a - 0.8 * s, b - 0.8 * s, 1.6 * s, 1.6 * s), 0.8 * s, color);
            }
            Icon::Shield => {
                seg!(0.0, -7.0, 6.0, -5.0; 6.0, -5.0, 6.0, 0.0; 6.0, 0.0, 0.0, 7.0; 0.0, 7.0, -6.0, 0.0; -6.0, 0.0, -6.0, -5.0; -6.0, -5.0, 0.0, -7.0);
            }
            Icon::Mark => {
                seg!(-3.5, -5.5, -1.5, -3.5; -1.5, -3.5, 1.5, -3.5; 1.5, -3.5, 3.5, -5.5; 3.5, -5.5, 3.5, 5.5; 3.5, 5.5, -3.5, 5.5; -3.5, 5.5, -3.5, -5.5);
            }
            Icon::MarkFilled => {
                let (a, b) = p(-3.5, -3.5);
                self.fill(rect(a, b, 7.0 * s, 9.0 * s), 0.5 * s, color);
                seg!(-3.0, -5.0, -1.5, -3.5; 3.0, -5.0, 1.5, -3.5; -3.0, -5.0, -3.0, -3.0; 3.0, -5.0, 3.0, -3.0);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Chat,
    Grid,
    Settings,
    Send,
    Stop,
    Attach,
    Image,
    Video,
    Close,
    ChevronDown,
    ChevronRight,
    Play,
    Copy,
    Tool,
    Pulse,
    Check,
    SidebarToggle,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    Search,
    Plus,
    Folder,
    FolderPlus,
    File,
    Terminal,
    GitCompare,
    Globe,
    MoreV,
    MoreH,
    Expand,
    Collapse,
    History,
    Clock,
    ListChecks,
    Sliders,
    Bulb,
    Agents,
    Branch,
    Refresh,
    Trash,
    Pencil,
    Warning,
    Info,
    Shield,
    Mark,
    MarkFilled,
}

/// One row of a `Ui::menu`.
#[derive(Clone, Copy, Debug)]
pub struct MenuItem<'a> {
    pub label: &'a str,
    pub hint: &'a str,
    pub icon: Option<Icon>,
    pub checked: bool,
    pub enabled: bool,
    pub danger: bool,
    pub separator: bool,
}

impl<'a> MenuItem<'a> {
    pub fn new(label: &'a str) -> Self {
        Self { label, hint: "", icon: None, checked: false, enabled: true, danger: false, separator: false }
    }
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn hint(mut self, hint: &'a str) -> Self {
        self.hint = hint;
        self
    }
    pub fn checked(mut self, on: bool) -> Self {
        self.checked = on;
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
    pub fn separated(mut self) -> Self {
        self.separator = true;
        self
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Info,
    Success,
    Warning,
    Error,
}

/// Smooth, stick-to-bottom scrolling. The bottom stickiness is what keeps a streaming answer in view without
/// fighting the user: scrolling up releases it, and scrolling back to the end re-engages it.
#[derive(Clone, Copy, Debug)]
pub struct Scroll {
    pub offset: f32,
    target: f32,
    pub content: f32,
    pub viewport: f32,
    pub stick: bool,
    drag: Option<(f32, f32)>,
}

impl Default for Scroll {
    fn default() -> Self {
        Self { offset: 0.0, target: 0.0, content: 0.0, viewport: 0.0, stick: true, drag: None }
    }
}

impl Scroll {
    /// For a list that starts at its top, instead of following what is added to its end.
    pub fn from_top() -> Self {
        Self { stick: false, ..Self::default() }
    }

    pub fn max_offset(&self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }

    pub fn scroll_to_end(&mut self) {
        self.stick = true;
        self.target = self.max_offset();
    }

    /// Jumps by `delta` with no easing. Content that grew above the viewport stays put on screen this way.
    pub fn shift(&mut self, delta: f32) {
        self.offset += delta;
        self.target += delta;
    }

    /// The same, held inside the content: a jump past either end lands on that end, rather than easing back from beyond it.
    pub fn shift_clamped(&mut self, delta: f32) {
        let max = self.max_offset();
        self.offset = (self.offset + delta).clamp(0.0, max);
        self.target = (self.target + delta).clamp(0.0, max);
    }

    /// Puts the viewport's top at `offset`, easing there.
    pub fn scroll_to(&mut self, offset: f32) {
        self.stick = false;
        self.target = offset.clamp(0.0, self.max_offset());
    }

    /// Call once per frame with the scrollable region, before drawing into it.
    pub fn update(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect, content: f32) {
        self.advance(ui, r, content);
        self.scrollbar(ui, id, r);
    }

    /// Takes in the wheel and moves the offset, drawing nothing.
    pub fn advance(&mut self, ui: &mut Ui<'_>, r: Rect, content: f32) {
        let delta = if ui.pointer_in(&r) { ui.input.scroll } else { 0.0 };
        self.advance_by(ui, r, content, delta);
    }

    /// How far the wheel can still move this: up for a positive `delta`, down for a negative one.
    pub fn wheel_room(&self, delta: f32) -> f32 {
        if delta > 0.0 { self.target } else { self.max_offset() - self.target }
    }

    /// `advance`, with the part of the wheel that is this scroller's already worked out.
    pub fn advance_by(&mut self, ui: &mut Ui<'_>, r: Rect, content: f32, delta: f32) {
        self.content = content;
        self.viewport = r.h;
        let max = self.max_offset();
        if delta != 0.0 {
            self.target = (self.target - delta).clamp(0.0, max);
            if ui.input.scroll_is_pixels {
                // Trackpads already deliver momentum, and smoothing it again would only add latency.
                self.offset = self.target;
            }
            self.stick = self.target >= max - 1.0;
        }
        if self.stick {
            self.target = max;
        }
        self.target = self.target.clamp(0.0, max);
        let diff = self.target - self.offset;
        if self.stick && diff.abs() < 200.0 {
            // Following a stream: move immediately, because easing would keep new text permanently behind.
            self.offset = self.target;
        } else if diff.abs() > 0.5 {
            let k = 1.0 - (-ui.dt / 0.06).exp();
            self.offset += diff * k;
            ui.animating = true;
        } else {
            self.offset = self.target;
        }
    }

    pub fn scrollbar(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let max = self.max_offset();
        if max <= 0.0 {
            return;
        }
        let track = rect(r.right() - 12.0, r.y + 2.0, 12.0, r.h - 4.0);
        let resp = ui.interact(id, track);
        let area_hover = ui.pointer_in(&r);
        let t = ui.animate(id ^ 0x8181, if resp.hovered || resp.pressed { 1.0 } else if area_hover { 0.45 } else { 0.0 }, motion::FAST);
        if t <= 0.01 {
            return;
        }
        let thumb_h = (r.h * r.h / self.content).max(24.0).min(track.h);
        let thumb_y = track.y + (track.h - thumb_h) * (self.offset / max).clamp(0.0, 1.0);
        if resp.pressed {
            let py = ui.input.pointer.map_or(0.0, |p| p.1);
            let (start_y, start_target) = *self.drag.get_or_insert((py, self.target));
            let per_px = max / (track.h - thumb_h).max(1.0);
            self.target = (start_target + (py - start_y) * per_px).clamp(0.0, max);
            self.offset = self.target;
            self.stick = self.target >= max - 1.0;
        } else {
            self.drag = None;
        }
        let width = 2.0 + 4.0 * t;
        let color = ui.theme.text_tertiary.fade(0.5 + 0.5 * t);
        ui.fill(rect(r.right() - width - 3.0, thumb_y, width, thumb_h), width * 0.5, color);
    }
}
