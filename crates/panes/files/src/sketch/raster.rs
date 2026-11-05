//! Strokes as pixels, for what leaves the surface: a PNG, or the strokes composited onto a capture at its own resolution
//! rather than the pane's CSS size (`SketchCanvas`'s `toPNG` and `toComposite`). What the view draws live is the same
//! geometry through the GPU; this runs on a thread, so it needs nothing the window has.

use super::strokes::*;
use crate::format::base64_encode;
use resvg::tiny_skia::{Color as SkColor, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke as SkStroke, Transform};

fn paint(color: u32) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(SkColor::from_rgba8((color >> 16) as u8, (color >> 8) as u8, color as u8, 255));
    p.anti_alias = true;
    p
}

fn sk_stroke(width: f32) -> SkStroke {
    SkStroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..SkStroke::default() }
}

fn draw(pixmap: &mut Pixmap, stroke: &Stroke, k: Transform, fonts: &mut dyn FnMut(&str, f32, u32) -> Option<(Vec<u8>, u32, u32)>) {
    match stroke {
        Stroke::Freehand { points, color, width } => {
            let Some(first) = points.first() else { return };
            if points.len() == 1 {
                let mut b = PathBuilder::new();
                b.push_circle(first.x, first.y, width / 2.0);
                if let Some(path) = b.finish() {
                    pixmap.fill_path(&path, &paint(*color), FillRule::Winding, k, None);
                }
                return;
            }
            let mut b = PathBuilder::new();
            b.move_to(first.x, first.y);
            for p in &points[1..] {
                b.line_to(p.x, p.y);
            }
            if let Some(path) = b.finish() {
                pixmap.stroke_path(&path, &paint(*color), &sk_stroke(*width), k, None);
            }
        }
        Stroke::Shape { kind, from, to, color, width } => match kind {
            Shape::Line => {
                let mut b = PathBuilder::new();
                b.move_to(from.x, from.y);
                b.line_to(to.x, to.y);
                if let Some(path) = b.finish() {
                    pixmap.stroke_path(&path, &paint(*color), &sk_stroke(*width), k, None);
                }
            }
            Shape::Rect => {
                let mut b = PathBuilder::new();
                b.move_to(from.x, from.y);
                b.line_to(to.x, from.y);
                b.line_to(to.x, to.y);
                b.line_to(from.x, to.y);
                b.close();
                if let Some(path) = b.finish() {
                    pixmap.stroke_path(&path, &paint(*color), &sk_stroke(*width), k, None);
                }
            }
            Shape::Ellipse => {
                let (cx, cy, rx, ry) = ((from.x + to.x) / 2.0, (from.y + to.y) / 2.0, (to.x - from.x).abs() / 2.0, (to.y - from.y).abs() / 2.0);
                let Some(oval) = resvg::tiny_skia::Rect::from_ltrb(cx - rx, cy - ry, cx + rx.max(0.01), cy + ry.max(0.01)) else { return };
                let Some(path) = PathBuilder::from_oval(oval) else { return };
                pixmap.stroke_path(&path, &paint(*color), &sk_stroke(*width), k, None);
            }
            Shape::Arrow => {
                let (shaft, head) = arrow_geometry(*from, *to, *width);
                let mut b = PathBuilder::new();
                b.move_to(from.x, from.y);
                b.line_to(shaft.x, shaft.y);
                if let Some(path) = b.finish() {
                    pixmap.stroke_path(&path, &paint(*color), &sk_stroke(*width), k, None);
                }
                let mut b = PathBuilder::new();
                b.move_to(head[0].x, head[0].y);
                b.line_to(head[1].x, head[1].y);
                b.line_to(head[2].x, head[2].y);
                b.close();
                if let Some(path) = b.finish() {
                    pixmap.fill_path(&path, &paint(*color), FillRule::Winding, k, None);
                }
            }
        },
        Stroke::Text { at, text, size, color } => {
            let scale = k.sx;
            for (i, line) in text.split('\n').enumerate() {
                let Some((rgba, w, h)) = fonts(line, size * scale, *color) else { continue };
                let (x, y) = ((at.x * scale + k.tx).round() as i32, ((at.y + i as f32 * size * 1.2) * scale + k.ty).round() as i32);
                blend(pixmap, &rgba, w, h, x, y);
            }
        }
    }
}

/// Straight RGBA over premultiplied pixels.
fn blend(pixmap: &mut Pixmap, rgba: &[u8], w: u32, h: u32, x: i32, y: i32) {
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    let data = pixmap.data_mut();
    for sy in 0..h as i32 {
        for sx in 0..w as i32 {
            let (tx, ty) = (x + sx, y + sy);
            if tx < 0 || ty < 0 || tx >= pw || ty >= ph {
                continue;
            }
            let s = ((sy * w as i32 + sx) * 4) as usize;
            let t = ((ty * pw + tx) * 4) as usize;
            let a = rgba[s + 3] as u32;
            for c in 0..3 {
                data[t + c] = ((rgba[s + c] as u32 * a + data[t + c] as u32 * (255 - a)) / 255) as u8;
            }
            data[t + 3] = (a + data[t + 3] as u32 * (255 - a) / 255) as u8;
        }
    }
}

fn to_png(pixmap: &Pixmap) -> Option<String> {
    let mut straight = pixmap.data().to_vec();
    for p in straight.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let image = image::RgbaImage::from_raw(pixmap.width(), pixmap.height(), straight)?;
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(format!("data:image/png;base64,{}", base64_encode(&png)))
}

/// One line of text as RGBA, from the same fonts the window uses.
fn text_pixels(ctx: &mut fluent::TextCtx, line: &str, px: f32, color: u32) -> Option<(Vec<u8>, u32, u32)> {
    let ty = fluent::TypeStyle { size: px, line: px * 1.2, weight: fluent::Weight::Regular, mono: false, italic: false, serif: false };
    ctx.rasterize_line(line, &ty, [(color >> 16) as u8, (color >> 8) as u8, color as u8])
}

/// The strokes alone over `background`, at the surface's size in device pixels, as its backing store was.
pub fn to_png_data_url(strokes: &[Stroke], size: (f32, f32), background: Option<u32>, dpr: f32) -> Option<String> {
    let mut pixmap = Pixmap::new((size.0 * dpr).round().max(1.0) as u32, (size.1 * dpr).round().max(1.0) as u32)?;
    if let Some(c) = background {
        pixmap.fill(SkColor::from_rgba8((c >> 16) as u8, (c >> 8) as u8, c as u8, 255));
    }
    let mut ctx = fluent::TextCtx::new(1.0);
    let mut fonts = |line: &str, px: f32, color: u32| text_pixels(&mut ctx, line, px, color);
    for stroke in strokes {
        draw(&mut pixmap, stroke, Transform::from_scale(dpr, dpr), &mut fonts);
    }
    to_png(&pixmap)
}

/// The scale a composite draws strokes at: the backdrop's own pixels per surface pixel.
pub fn composite_scale(backdrop: (u32, u32), surface: (f32, f32)) -> f32 {
    (backdrop.0 as f32 / surface.0).min(backdrop.1 as f32 / surface.1)
}

/// Composites at the backdrop's own pixel size, so the attachment keeps the screenshot's resolution. A surface never
/// measured gives the backdrop back untouched.
pub fn to_composite_data_url(strokes: &[Stroke], size: (f32, f32), backdrop_data_url: &str) -> Option<String> {
    if size.0 <= 0.0 || size.1 <= 0.0 {
        return Some(backdrop_data_url.to_owned());
    }
    let bytes = crate::format::data_url_bytes(backdrop_data_url)?;
    let backdrop = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = backdrop.dimensions();
    let mut pixmap = Pixmap::new(w, h)?;
    for (t, s) in pixmap.data_mut().chunks_exact_mut(4).zip(backdrop.as_raw().chunks_exact(4)) {
        let a = s[3] as u32;
        t[0] = (s[0] as u32 * a / 255) as u8;
        t[1] = (s[1] as u32 * a / 255) as u8;
        t[2] = (s[2] as u32 * a / 255) as u8;
        t[3] = s[3];
    }
    let scale = composite_scale((w, h), size);
    let mut ctx = fluent::TextCtx::new(1.0);
    let mut fonts = |line: &str, px: f32, color: u32| text_pixels(&mut ctx, line, px, color);
    for stroke in strokes {
        draw(&mut pixmap, stroke, Transform::from_scale(scale, scale), &mut fonts);
    }
    to_png(&pixmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(data_url: &str) -> image::RgbaImage {
        image::load_from_memory(&crate::format::data_url_bytes(data_url).unwrap()).unwrap().to_rgba8()
    }

    fn line() -> Stroke {
        Stroke::Shape { kind: Shape::Line, from: Point { x: 10.0, y: 10.0 }, to: Point { x: 90.0, y: 10.0 }, color: 0xE03131, width: 4.0 }
    }

    #[test]
    fn a_png_carries_the_ink_over_its_background() {
        let image = pixels(&to_png_data_url(&[line()], (100.0, 50.0), Some(0xFFFFFF), 1.0).unwrap());
        assert_eq!(image.dimensions(), (100, 50));
        assert_eq!(image.get_pixel(50, 10).0, [0xE0, 0x31, 0x31, 255]);
        assert_eq!(image.get_pixel(50, 40).0, [255, 255, 255, 255]);
    }

    #[test]
    fn sizes_a_png_in_device_pixels_and_draws_in_surface_pixels() {
        let image = pixels(&to_png_data_url(&[line()], (100.0, 50.0), Some(0xFFFFFF), 2.0).unwrap());
        assert_eq!(image.dimensions(), (200, 100));
        assert_eq!(image.get_pixel(100, 20).0, [0xE0, 0x31, 0x31, 255], "the stroke, at twice the size");
        assert_eq!(image.get_pixel(100, 80).0, [255, 255, 255, 255]);
    }

    #[test]
    fn rounds_a_rectangles_corners_as_the_canvas_line_join_does() {
        let boxed = Stroke::Shape { kind: Shape::Rect, from: Point { x: 10.0, y: 10.0 }, to: Point { x: 40.0, y: 40.0 }, color: 0xE03131, width: 8.0 };
        let image = pixels(&to_png_data_url(&[boxed], (50.0, 50.0), Some(0xFFFFFF), 1.0).unwrap());
        assert_eq!(image.get_pixel(25, 10).0, [0xE0, 0x31, 0x31, 255], "the edge");
        assert_eq!(image.get_pixel(6, 6).0, [255, 255, 255, 255], "a mitred corner would have filled this");
    }

    #[test]
    fn composites_onto_the_backdrop_at_the_backdrops_own_resolution() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(400, 200, image::Rgba([0, 0, 255, 255])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let backdrop = format!("data:image/png;base64,{}", base64_encode(&png));
        assert_eq!(composite_scale((400, 200), (100.0, 50.0)), 4.0);
        let image = pixels(&to_composite_data_url(&[line()], (100.0, 50.0), &backdrop).unwrap());
        assert_eq!(image.dimensions(), (400, 200));
        assert_eq!(image.get_pixel(200, 40).0, [0xE0, 0x31, 0x31, 255], "the stroke, scaled up");
        assert_eq!(image.get_pixel(200, 150).0, [0, 0, 255, 255], "the capture around it");
    }

    #[test]
    fn returns_the_backdrop_untouched_when_nothing_has_been_measured_yet() {
        assert_eq!(to_composite_data_url(&[line()], (0.0, 0.0), "data:x").as_deref(), Some("data:x"));
    }

    #[test]
    fn draws_a_single_freehand_point_as_a_dot_and_text_at_its_place() {
        let dot = Stroke::Freehand { points: vec![Point { x: 8.0, y: 9.0 }], color: 0x2F9E44, width: 4.0 };
        let label = Stroke::Text { at: Point { x: 20.0, y: 20.0 }, text: "one\ntwo".to_owned(), size: SKETCH_TEXT_SIZE, color: 0x1971C2 };
        let image = pixels(&to_png_data_url(&[dot, label], (100.0, 80.0), Some(0xFFFFFF), 1.0).unwrap());
        assert_eq!(image.get_pixel(8, 9).0, [0x2F, 0x9E, 0x44, 255]);
        let inked = |y0: u32, y1: u32| (y0..y1).any(|y| (20..60).any(|x| image.get_pixel(x, y).0[0] < 200));
        assert!(inked(20, 36), "the first line");
        assert!(inked(39, 56), "the second, 1.2 x the size below it");
    }
}
