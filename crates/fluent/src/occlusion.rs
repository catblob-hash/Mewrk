//! What a solid surface covers need not be drawn. Every pipeline blends, so the GPU shades everything it is given, however much of it
//! ends up under something else; a dialog would pay for the page beneath it on every pixel, and the page for the ground beneath itself.
//!
//! Each quad is clamped to its clip in the vertex shader and its fragment works from the full box, so a box cut to the parts that show
//! draws exactly the pixels it would have. Only what is certain goes: what a surface's corner might let through stays, and nothing is
//! ever taken from above a surface or from under anything see-through.

use crate::gpu::{ImageDraw, ImageInst, Layer, RectInst, Scene, KIND_ACRYLIC, KIND_RECT};

/// Where a layer's images come in the order it draws: after all its boxes, however many.
const IMAGES_AFTER: usize = usize::MAX / 2;

/// How many surfaces to look under: the largest, which cover the most.
const SURFACES: usize = 12;
/// A surface narrower than this, in physical pixels, covers too little to look under.
const MIN_SIDE: f32 = 48.0;
/// A box is cut around a surface only when this much of it, in physical pixels, would go. Below that the extra instances cost more.
const MIN_CUT: f32 = 4096.0;
/// How many parts a box may end up in. One that would go further is left as it is.
const MAX_PARTS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Area {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Area {
    fn of_clip(clip: [f32; 4]) -> Self {
        Self { x0: clip[0], y0: clip[1], x1: clip[2], y1: clip[3] }
    }

    fn of_box(r: [f32; 4], grow: f32) -> Self {
        Self { x0: r[0] - grow, y0: r[1] - grow, x1: r[0] + r[2] + grow, y1: r[1] + r[3] + grow }
    }

    fn clip(self) -> [f32; 4] {
        [self.x0, self.y0, self.x1, self.y1]
    }

    fn meet(self, o: Self) -> Self {
        Self { x0: self.x0.max(o.x0), y0: self.y0.max(o.y0), x1: self.x1.min(o.x1), y1: self.y1.min(o.y1) }
    }

    fn is_empty(self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    fn holds(self, o: Self) -> bool {
        self.x0 <= o.x0 && self.y0 <= o.y0 && self.x1 >= o.x1 && self.y1 >= o.y1
    }

    fn size(self) -> f32 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }
}

/// A surface, and where it lies in the order things are drawn.
#[derive(Clone, Copy)]
pub(crate) struct Surface {
    area: Area,
    layer: usize,
    rect: usize,
}

/// The part of a box that certainly replaces whatever is under it: a solid fill, whatever its border, short of its corners and of the
/// antialiased edge a fraction of a pixel wide. `None` for anything else, a gradient between solid colours aside.
fn solid_part(inst: &RectInst) -> Option<Area> {
    let kind = inst.params[2];
    if (kind != KIND_RECT && kind != KIND_ACRYLIC) || inst.fill.a != 255 || inst.fill2.a != 255 {
        return None;
    }
    certain_part(inst.rect, inst.radii, inst.clip)
}

/// The same of an image: only one known to be solid, drawn in full.
fn solid_image_part(draw: &ImageDraw) -> Option<Area> {
    (draw.opaque && draw.inst.opacity[0] >= 1.0).then(|| certain_part(draw.inst.rect, draw.inst.radii, draw.inst.clip)).flatten()
}

/// What a rounded box's fill certainly reaches, within its clip.
fn certain_part(rect: [f32; 4], radii: [f32; 4], clip: [f32; 4]) -> Option<Area> {
    let [_, _, w, h] = rect;
    let radius = radii.iter().fold(0.0f32, |most, r| most.max(*r));
    // Past half its side a radius does not draw the shape CSS would.
    if radius > w.min(h) * 0.5 {
        return None;
    }
    // Where a corner's arc comes closest along the diagonal, and half a pixel more for its edge.
    let inset = if radius > 0.0 { (radius * (1.0 - std::f32::consts::FRAC_1_SQRT_2)).ceil() + 1.0 } else { 0.0 };
    let part = Area::of_box(rect, -inset).meet(Area::of_clip(clip));
    let part = Area { x0: part.x0.ceil(), y0: part.y0.ceil(), x1: part.x1.floor(), y1: part.y1.floor() };
    (part.x1 - part.x0 >= MIN_SIDE && part.y1 - part.y0 >= MIN_SIDE).then_some(part)
}

/// The parts of `quad` no surface covers. Returns how many there are, or `None` when that is all of it.
fn parts_left(quad: Area, surfaces: &[Surface], parts: &mut [Area; MAX_PARTS]) -> Option<usize> {
    parts[0] = quad;
    let mut count = 1;
    let mut cut = false;
    for surface in surfaces {
        let hider = surface.area;
        let mut at = 0;
        while at < count {
            let part = parts[at];
            let overlap = part.meet(hider);
            if overlap.is_empty() {
                at += 1;
            } else if hider.holds(part) {
                count -= 1;
                parts[at] = parts[count];
                cut = true;
            } else if overlap.size() >= MIN_CUT && count + 3 <= MAX_PARTS {
                let around = [
                    Area { y1: overlap.y0, ..part },
                    Area { y0: overlap.y1, ..part },
                    Area { y0: overlap.y0, x1: overlap.x0, y1: overlap.y1, ..part },
                    Area { x0: overlap.x1, y0: overlap.y0, y1: overlap.y1, ..part },
                ];
                count -= 1;
                parts[at] = parts[count];
                for piece in around.into_iter().filter(|piece| !piece.is_empty()) {
                    parts[count] = piece;
                    count += 1;
                }
                cut = true;
            } else {
                at += 1;
            }
        }
    }
    cut.then_some(count)
}

fn cull_rects(layer: &mut Layer, layer_at: usize, surfaces: &[Surface], extra: &mut Vec<(usize, RectInst)>) {
    let mut parts = [Area { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0 }; MAX_PARTS];
    let (mut original, mut kept) = (0, 0);
    layer.rects.retain_mut(|inst| {
        let at = original;
        original += 1;
        let later = surfaces.partition_point(|surface| (surface.layer, surface.rect) <= (layer_at, at));
        // The vertex shader grows every box by a pixel, for its antialiased edge.
        let quad = Area::of_box(inst.rect, 1.0).meet(Area::of_clip(inst.clip));
        if quad.is_empty() {
            return false;
        }
        match parts_left(quad, &surfaces[later..], &mut parts) {
            None => {}
            Some(0) => return false,
            Some(count) => {
                inst.clip = parts[0].clip();
                for part in &parts[1..count] {
                    extra.push((kept + 1, RectInst { clip: part.clip(), ..*inst }));
                }
            }
        }
        kept += 1;
        true
    });
    // From the back, so what is still to go in keeps its place. A box's parts don't overlap, so their own order is free.
    for (at, inst) in extra.drain(..).rev() {
        layer.rects.insert(at, inst);
    }
}

fn cull_images(layer: &mut Layer, surfaces: &[Surface]) {
    let mut parts = [Area { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0 }; MAX_PARTS];
    let mut at = 0;
    while at < layer.images.len() {
        let draw = layer.images[at];
        let quad = Area::of_box(draw.inst.rect, 0.0).meet(Area::of_clip(draw.inst.clip));
        match parts_left(quad, surfaces, &mut parts) {
            None => at += 1,
            Some(count) => {
                layer.images.splice(at..=at, parts[..count].iter().map(|part| ImageDraw { inst: ImageInst { clip: part.clip(), ..draw.inst }, ..draw }));
                at += count;
            }
        }
    }
}

fn cull_glyphs(layer: &mut Layer, surfaces: &[Surface]) {
    layer.glyphs.retain(|glyph| {
        let quad = Area { x0: glyph.pos[0], y0: glyph.pos[1], x1: glyph.pos[0] + f32::from(glyph.size[0]), y1: glyph.pos[1] + f32::from(glyph.size[1]) };
        let clip = Area { x0: f32::from(glyph.clip[0]), y0: f32::from(glyph.clip[1]), x1: f32::from(glyph.clip[2]), y1: f32::from(glyph.clip[3]) };
        let seen = quad.meet(clip);
        !seen.is_empty() && !surfaces.iter().any(|surface| surface.area.holds(seen))
    });
}

impl Scene {
    /// Takes out what solid surfaces cover. `size` is the window's, in physical pixels. Run it once a frame is painted and before it is
    /// rendered: what comes out of it is only good for drawing.
    pub fn cull_occluded(&mut self, size: (u32, u32)) {
        let mut surfaces = std::mem::take(&mut self.surfaces);
        surfaces.clear();
        for (layer_at, layer) in self.layers.drawn().enumerate() {
            surfaces.extend(layer.rects.iter().enumerate().filter_map(|(rect, inst)| solid_part(inst).map(|area| Surface { area, layer: layer_at, rect })));
            surfaces.extend(layer.images.iter().enumerate().filter_map(|(image, draw)| solid_image_part(draw).map(|area| Surface { area, layer: layer_at, rect: IMAGES_AFTER + image })));
        }
        surfaces.sort_unstable_by(|a, b| b.area.size().total_cmp(&a.area.size()));
        surfaces.truncate(SURFACES);
        let window = Area { x0: 0.0, y0: 0.0, x1: size.0 as f32, y1: size.1 as f32 };
        self.ground_covered = surfaces.iter().any(|surface| surface.area.holds(window));
        surfaces.sort_unstable_by_key(|surface| (surface.layer, surface.rect));

        let mut extra = std::mem::take(&mut self.extra_rects);
        for (layer_at, layer) in self.layers.drawn_mut().enumerate() {
            let in_this = surfaces.partition_point(|surface| surface.layer < layer_at);
            if in_this == surfaces.len() {
                break;
            }
            cull_rects(layer, layer_at, &surfaces[in_this..], &mut extra);
            // A layer draws its images and text after all its boxes, so only a later layer's surface covers them.
            let later = surfaces.partition_point(|surface| surface.layer <= layer_at);
            if later < surfaces.len() {
                cull_images(layer, &surfaces[later..]);
                cull_glyphs(layer, &surfaces[later..]);
            }
        }
        self.extra_rects = extra;
        self.surfaces = surfaces;
    }
}
