//! `backdrop.css`'s two switches, `lib/glassPlate.ts` and `GlassPlate.tsx`. `data-backdrop="custom"`: the ground is not the theme's
//! own, so whatever paints the ground turns transparent over it. `data-glass="true"`: the same, and the sidebar and tiles turn to glass.
//!
//! Glass is the window's background blurred and saturated, with each surface's tint laid over it. Nothing blurs anything as it is
//! drawn: the background is a still picture, so the filter runs once over it, off the UI thread, and the one small texture that makes
//! is what lies under every surface, fixed to the window and fitted as the picture is, so a surface shows the part beneath it wherever
//! it goes. It is baked again only when the picture changes or the window's fit moves far enough to show. Only the background is
//! baked in, so glass is one layer deep: what floats over the tiles stays solid.

use crate::chrome::TILE_RADIUS;
use fluent::{queue_image, release_image, ImageId, Theme};
use mewrk_client::appearance::replaces_theme_ground;
use mewrk_client::prelude::*;
use mewrk_settings_global::logic::background::{parse_background, Background, BackgroundScheme};
use mewrk_settings_global::logic::background_image::{area_resize, decode, decode_data_url, Pixels};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layers {
    pub custom: bool,
    pub glass: bool,
}

impl Layers {
    /// Whether the window shows `AppBackdrop` behind its gutter.
    pub fn layered(self) -> bool {
        self.custom || self.glass
    }
}

thread_local! {
    // The grounds are asked for all over the tree, and these change with a setting: one read a frame, not one argument each.
    static LAYERS: Cell<Layers> = const { Cell::new(Layers { custom: false, glass: false }) };
    /// The glass surfaces of this frame, as the view lays them out, so the plate is under them in the frame they move in.
    static SURFACES: RefCell<Vec<(Rect, [f32; 4])>> = const { RefCell::new(Vec::new()) };
}

/// Reads the settings for this frame's grounds, and sees to the plate.
pub fn begin_frame(app: &mut App) {
    let appearance = &app.doc.global_settings.appearance;
    let layers = Layers { custom: replaces_theme_ground(&appearance.background), glass: appearance.liquid_glass };
    LAYERS.with(|cell| cell.set(layers));
    SURFACES.with(|surfaces| surfaces.borrow_mut().clear());
    keep_plate(app);
}

/// Lays the glass under a surface, `r` being where it lies in the window: `<GlassPlate />` as a surface's first child.
pub fn surface(r: Rect, radii: [f32; 4]) {
    if layers().glass {
        SURFACES.with(|surfaces| surfaces.borrow_mut().push((r, radii)));
    }
}

/// What this frame's surfaces are, for tests.
pub fn surfaces() -> Vec<(Rect, [f32; 4])> {
    SURFACES.with(|surfaces| surfaces.borrow().clone())
}

pub fn layers() -> Layers {
    LAYERS.with(Cell::get)
}

/// `--glass-tile`: the conversation and the sidebar. Dark glass lets more of a bright picture through, so it is denser.
pub fn glass_tile() -> Color {
    if theme().dark { Color::rgb(0x212121).with_alpha(0.84) } else { Color::rgb(0xF7F7F5).with_alpha(0.78) }
}

/// `--glass-pane`: a side pane.
pub fn glass_pane() -> Color {
    if theme().dark { Color::rgb(0x1B1B1B).with_alpha(0.86) } else { Color::rgb(0xEFEFEB).with_alpha(0.80) }
}

/// `--glass-edge`: night glass still catches light on its rim.
pub fn glass_edge() -> Color {
    if theme().dark { Color::rgb(0xFFFFFF).with_alpha(0.13) } else { Color::rgb(0xFFFFFF).with_alpha(0.45) }
}

/// `--strip-tint`: what keeps the top bar legible on any picture.
fn strip_tint() -> Color {
    let t_ = theme();
    let base = if t_.dark { Color::rgb(0x161616) } else { Color::rgb(0xE7E7E2) };
    base.with_alpha(if t_.dark { 0.78 } else { 0.72 })
}

/// `--strip-reach`: how far below the bar the strip fades.
const STRIP_REACH: f32 = 24.0;

fn fade(tint: Color, share: f32) -> Color {
    tint.with_alpha(tint.a as f32 / 255.0 * share.clamp(0.0, 1.0))
}

/// How much of the tint is left at `y` of the strip: solid for its first 30%, then fading out.
fn strip_share(y: f32, bar: f32) -> f32 {
    let height = bar + STRIP_REACH;
    let solid = height * 0.3;
    if y <= solid { 1.0 } else { 1.0 - (y - solid) / (height - solid) }
}

/// `--tile-rise`: how far the tiles rise into the bar's foot.
pub const TILE_RISE: f32 = 4.0;

/// `.topbar::before`, the part behind the bar's own content, and `.topbar::after`, the notch: a corner's worth of the tint that the
/// sidebar's round corner leaves showing. All of it goes down under the shell, so the title and the buttons lie over it, and the
/// sidebar and the tiles cover what is not theirs to show.
fn paint_strip(ui: &mut Ui<'_>, bar: Rect, sidebar_open: bool) {
    let tint = strip_tint();
    let solid = ((bar.h + STRIP_REACH) * 0.3).min(bar.h);
    ui.fill(rect(bar.x, bar.y, bar.w, solid), 0.0, tint);
    ui.fill_gradient(rect(bar.x, bar.y + solid, bar.w, bar.h - solid), [0.0; 4], tint, fade(tint, strip_share(bar.h, bar.h)));
    if sidebar_open {
        ui.fill(rect(bar.x - TILE_RADIUS, bar.y, TILE_RADIUS, TILE_RADIUS), 0.0, tint);
    }
}

/// The rest of `.topbar::before`: it lies over the tiles' top edge, risen as they are, so it is drawn after them and takes no input.
pub fn strip_over_tiles(bar: f32) -> Option<E> {
    if !layers().layered() {
        return None;
    }
    let tint = strip_tint();
    let top = bar - TILE_RISE;
    Some(div().absolute().left(0.0).right(0.0).top(top).h(STRIP_REACH + TILE_RISE).pass_through().gradient(fade(tint, strip_share(top, bar)), fade(tint, 0.0)))
}

// ---------------------------------------------------------------- the bake

/// `--glass-blur`: the blur's standard deviation, in the window's pixels.
const GLASS_BLUR: f32 = 22.0;
/// `--glass-saturate`.
const GLASS_SATURATE: f32 = 1.3;
/// How many pixels of the baked copy the blur's standard deviation spans: enough that scaling it up shows nothing.
const PLATE_SIGMA: f32 = 3.0;
/// The longest side a baked copy may have, should a small blur over a large picture ask for more, and of the copy of the picture kept
/// to bake from.
const PLATE_MAX: u32 = 1024;
/// The blur is set in the window's pixels, so a window resized far enough wants another bake; a few percent either way is not to be seen.
const FIT_STEP: f32 = 1.05;
/// How long the window has to keep its size before that bake.
const FIT_SETTLES: Duration = Duration::from_millis(150);
/// What to ask of an imported picture's tiers: the smallest one this wide holds all that the blur leaves.
const IMPORTED_TIER: (u32, u32) = (640, 1);

/// The colour `saturate()` makes of a flat one. A blur leaves it as it is.
pub fn filter_color(color: [f32; 3], saturate: f32) -> [f32; 3] {
    let [r, g, b] = color;
    let s = saturate;
    // Each step of a filter clamps what it makes, as the engines' do.
    [(0.213 + 0.787 * s) * r + (0.715 - 0.715 * s) * g + (0.072 - 0.072 * s) * b, (0.213 - 0.213 * s) * r + (0.715 + 0.285 * s) * g + (0.072 - 0.072 * s) * b, (0.213 - 0.213 * s) * r + (0.715 - 0.715 * s) * g + (0.072 + 0.928 * s) * b].map(|channel| channel.clamp(0.0, 255.0))
}

/// The radii of three box blurs that, run one after another, make a Gaussian blur of `sigma` (Kovesi's widths: the two nearest odd
/// ones, as many of each as the variance asks for).
pub fn box_radii(sigma: f32) -> [usize; 3] {
    let variance = 12.0 * sigma * sigma;
    let mut lower = (variance / 3.0 + 1.0).sqrt().floor() as i64;
    if lower % 2 == 0 {
        lower -= 1;
    }
    let lower = lower.max(1);
    let narrow = ((variance - (3 * lower * lower + 12 * lower + 9) as f32) / (-4 * lower - 4) as f32).round() as i64;
    let radius = |index: i64| ((if index < narrow { lower } else { lower + 2 } - 1) / 2) as usize;
    [radius(0), radius(1), radius(2)]
}

/// One box blur along a line of `length` values `stride` apart from `start`; the ends repeat outwards.
fn blur_line(source: &[f32], target: &mut [f32], start: usize, stride: usize, length: usize, radius: usize) {
    let last = length - 1;
    let scale = 1.0 / (2 * radius + 1) as f32;
    let mut sum = source[start] * (radius + 1) as f32;
    for offset in 1..=radius {
        sum += source[start + offset.min(last) * stride];
    }
    for index in 0..length {
        target[start + index * stride] = sum * scale;
        sum += source[start + (index + radius + 1).min(last) * stride] - source[start + index.saturating_sub(radius) * stride];
    }
}

/// Blurs the colour of an RGBA image in place, as `blur(sigma)` does, with `sigma` in its pixels. Alpha is left as it was.
pub fn blur_pixels(pixels: &mut [u8], width: usize, height: usize, sigma: f32) {
    if sigma <= 0.0 {
        return;
    }
    let radii = box_radii(sigma);
    let count = width * height;
    let (mut plane, mut scratch) = (vec![0.0f32; count], vec![0.0f32; count]);
    for channel in 0..3 {
        for index in 0..count {
            plane[index] = f32::from(pixels[index * 4 + channel]);
        }
        for &radius in radii.iter().filter(|radius| **radius > 0) {
            for row_at in 0..height {
                blur_line(&plane, &mut scratch, row_at * width, 1, width, radius);
            }
            for column in 0..width {
                blur_line(&scratch, &mut plane, column, width, height, radius);
            }
        }
        for index in 0..count {
            pixels[index * 4 + channel] = plane[index].round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Runs `saturate()` over every pixel of an RGBA image, in place.
pub fn filter_pixels(pixels: &mut [u8], saturate: f32) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let [r, g, b] = filter_color([f32::from(pixel[0]), f32::from(pixel[1]), f32::from(pixel[2])], saturate);
        pixel[0] = r.round() as u8;
        pixel[1] = g.round() as u8;
        pixel[2] = b.round() as u8;
    }
}

/// How many of the window's pixels one of a picture's makes when it covers the window.
pub fn cover_scale(window: (f32, f32), picture: (u32, u32)) -> f32 {
    (window.0 / picture.0.max(1) as f32).max(window.1 / picture.1.max(1) as f32)
}

/// Which bake a fit belongs to: fits in one step share one.
pub fn fit_step(scale: f32) -> i32 {
    (scale.max(1e-3).ln() / FIT_STEP.ln()).round() as i32
}

/// A picture with the glass's filter run over it, small: blurring takes away everything finer than the blur, so a copy a few pixels
/// per standard deviation across holds all that is left, and bilinear scaling draws it at the window's size.
pub fn bake(picture: &Pixels, scale: f32) -> Result<Pixels, String> {
    let sigma = GLASS_BLUR / scale.max(1e-3);
    let shrink = (PLATE_SIGMA / sigma.max(1e-3)).min(PLATE_MAX as f32 / picture.width.max(picture.height) as f32).min(1.0);
    let (width, height) = (((picture.width as f32 * shrink).round() as u32).max(1), ((picture.height as f32 * shrink).round() as u32).max(1));
    let mut plate = area_resize(picture, width, height)?;
    blur_pixels(&mut plate.data, width as usize, height as usize, sigma * (width as f32 / picture.width as f32));
    filter_pixels(&mut plate.data, GLASS_SATURATE);
    Ok(plate)
}

/// The copy of a picture to keep for baking: a bake never wants more of it than this.
fn bake_source(picture: Pixels) -> Result<Pixels, String> {
    let shrink = PLATE_MAX as f32 / picture.width.max(picture.height) as f32;
    if shrink >= 1.0 {
        return Ok(picture);
    }
    area_resize(&picture, ((picture.width as f32 * shrink).round() as u32).max(1), ((picture.height as f32 * shrink).round() as u32).max(1))
}

/// The baked glass, on the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plate {
    pub image: ImageId,
    pub size: (u32, u32),
}

#[derive(Default)]
struct Glass {
    /// The `background` setting all this is for, while the glass is on and it is a picture.
    value: Option<String>,
    source: Option<Arc<Pixels>>,
    plate: Option<Plate>,
    /// `object-position` of the picture.
    focus: (f32, f32),
    /// The fit the plate on screen was baked for, and the one on its way.
    baked_step: i32,
    baking_step: Option<i32>,
    /// A fit the window has moved to, and since when.
    moved: Option<(i32, Instant)>,
    /// The newest load or bake: anything older that arrives is dropped.
    token: u64,
    bakes: u32,
}

fn pixels_of(task: &Task, value: &str) -> Option<Pixels> {
    match parse_background(value) {
        Background::Solid(_) => None,
        Background::Builtin(picture) => decode(picture.picture).ok(),
        Background::Imported(id) => {
            let args = json!({ "imageId": id, "width": IMPORTED_TIER.0, "height": IMPORTED_TIER.1 });
            let tier = task.invoke::<Value>("background_image_data", args).ok()?;
            decode_data_url(tier.get("dataUrl")?.as_str()?).ok()
        }
    }
}

/// Bakes on the thread it is called from and hands the frame the one texture to show. `new_source`: the picture is new, so the copy
/// to bake from goes along too.
fn bake_for(task: &Task, token: u64, window: (f32, f32), source: Arc<Pixels>, new_source: bool) {
    let scale = cover_scale(window, (source.width, source.height));
    let Ok(plate) = bake(&source, scale) else { return };
    let size = (plate.width, plate.height);
    let image = queue_image(plate.data, plate.width, plate.height);
    task.post(move |app| {
        let glass = app.local::<Glass>();
        if glass.token != token {
            release_image(image);
            return;
        }
        if new_source {
            glass.source = Some(source);
        }
        if let Some(old) = glass.plate.replace(Plate { image, size }) {
            release_image(old.image);
        }
        glass.baked_step = fit_step(scale);
        glass.baking_step = None;
        glass.bakes += 1;
        repaint();
    });
}

/// Takes the glass away, and whatever is on its way.
fn clear_plate(glass: &mut Glass) {
    if let Some(old) = glass.plate.take() {
        release_image(old.image);
    }
    glass.value = None;
    glass.source = None;
    glass.baking_step = None;
    glass.moved = None;
    glass.token += 1;
}

/// `bakeWindow`, once a frame: nothing happens unless the setting, the picture or the window's fit has moved.
fn keep_plate(app: &mut App) {
    let appearance = &app.doc.global_settings.appearance;
    let value = appearance.background.clone();
    let wanted = appearance.liquid_glass && !matches!(parse_background(&value), Background::Solid(_));
    let glass = app.local::<Glass>();
    if !wanted {
        if glass.value.is_some() {
            clear_plate(glass);
        }
        return;
    }
    let window = window_size();
    if glass.value.as_deref() != Some(value.as_str()) {
        clear_plate(glass);
        glass.value = Some(value.clone());
        glass.focus = match parse_background(&value) {
            Background::Builtin(picture) => picture.focus,
            _ => (0.5, 0.5),
        };
        let token = glass.token;
        app.spawn(move |task| {
            if let Some(source) = pixels_of(&task, &value).and_then(|picture| bake_source(picture).ok()) {
                bake_for(&task, token, window, Arc::new(source), true);
            }
        });
        return;
    }
    let Some(source) = glass.source.clone() else { return };
    let step = fit_step(cover_scale(window, (source.width, source.height)));
    if step == glass.baking_step.unwrap_or(glass.baked_step) {
        glass.moved = None;
        return;
    }
    match glass.moved {
        Some((moved_to, since)) if moved_to == step => {
            if since.elapsed() >= FIT_SETTLES {
                glass.moved = None;
                glass.baking_step = Some(step);
                glass.token += 1;
                let token = glass.token;
                app.spawn(move |task| bake_for(&task, token, window, source, false));
            } else {
                repaint();
            }
        }
        _ => {
            glass.moved = Some((step, Instant::now()));
            repaint();
        }
    }
}

/// The plate on screen, for tests.
pub fn plate(app: &mut App) -> Option<Plate> {
    app.local::<Glass>().plate
}

/// How many times the glass has been baked, for tests.
pub fn bakes(app: &mut App) -> u32 {
    app.local::<Glass>().bakes
}

/// What lies under a surface when the ground is a solid one: its colour through the filter, there being no picture to bake.
fn flat_plate(value: &str) -> Option<Color> {
    let ground = match parse_background(value) {
        Background::Solid(None) => crate::pane_tiles::solid_gutter(),
        Background::Solid(Some(BackgroundScheme::Night)) => Theme::dark().mica_base,
        Background::Solid(Some(BackgroundScheme::Day)) => Theme::light().mica_base,
        _ => return None,
    };
    let [r, g, b] = filter_color([f32::from(ground.r), f32::from(ground.g), f32::from(ground.b)], GLASS_SATURATE);
    Some(Color { r: r.round() as u8, g: g.round() as u8, b: b.round() as u8, a: ground.a })
}

/// What lies under a glass surface this frame: the plate once it is baked, a colour over a solid ground, and nothing until then.
#[derive(Clone, Copy)]
struct Under {
    picture: Option<Plate>,
    focus: (f32, f32),
    flat: Option<Color>,
}

impl Under {
    fn now(app: &mut App) -> Self {
        let on = layers().glass;
        let flat = if on { flat_plate(&app.doc.global_settings.appearance.background) } else { None };
        let glass = app.local::<Glass>();
        Self { picture: glass.plate.filter(|_| on), focus: glass.focus, flat }
    }

    fn paint(self, ui: &mut Ui<'_>, window: Rect, r: Rect, radii: [f32; 4]) {
        if let Some(picture) = self.picture {
            ui.image_fixed(picture.image, r, radii, window, picture.size, self.focus);
        } else if let Some(flat) = self.flat {
            ui.fill_radii(r, radii, flat);
        }
    }
}

/// The layer between the window's background and the shell: the bar's strip, and the glass under this frame's surfaces. `bar` is the
/// top bar's box. A picture draws over a layer's boxes, so a surface's plate has to lie in a layer below the surface: all of them lie
/// in this one.
pub fn under_shell(app: &mut App, bar: Rect, sidebar_beside: bool) -> Option<E> {
    if !layers().layered() {
        return None;
    }
    let under = Under::now(app);
    Some(
        custom::<App>(move |_, ui, window| {
            paint_strip(ui, bar, sidebar_beside);
            SURFACES.with(|surfaces| {
                for &(r, radii) in surfaces.borrow().iter() {
                    under.paint(ui, window, r, radii);
                }
            });
        })
        .absolute()
        .inset(0.0)
        .pass_through(),
    )
}

/// The glass under the one surface that lies over the tiles, a sidebar floating in a narrow window: a layer of its own, written just
/// before it. It is still the same glass, so it hides what passes under it as upstream's does.
pub fn plate_over_tiles(app: &mut App, r: Rect, radii: [f32; 4]) -> Option<E> {
    if !layers().glass {
        return None;
    }
    let under = Under::now(app);
    Some(custom::<App>(move |_, ui, area| {
        let (window_w, window_h) = window_size();
        under.paint(ui, rect(0.0, 0.0, window_w, window_h), area, radii);
    }).absolute().left(r.x).top(r.y).size(r.w, r.h).pass_through())
}
