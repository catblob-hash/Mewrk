//! `backdrop.css`'s two switches, `lib/glassPlate.ts` and `GlassPlate.tsx`. `data-backdrop="custom"`: the ground is not the theme's
//! own, so whatever paints the ground turns transparent over it. `data-glass="true"`: the same, and the sidebar and tiles turn to glass.
//!
//! Glass is the window's background blurred and saturated, with each surface's tint laid over it. Nothing blurs anything as it is
//! drawn: the filter runs once over the background, off the UI thread, and kept on disk that bake is all the glass ever costs again. A
//! still picture's bake is one frame and a video's or an animated picture's is one for each moment, in a file of small frames that playing
//! only copies into one small texture. That texture is what lies under every surface, fixed to the window and fitted as the background
//! is, so a surface shows the part beneath it wherever it goes. Only the background is baked in, so glass is one layer deep: what floats
//! over the tiles stays solid.
//!
//! A bake takes time, and a video's can take minutes. For whatever part of it isn't there yet, the renderer works the same filter out on
//! the GPU from what is on screen. That costs a little on every frame it runs, and it stops by itself as soon as the bake reaches the
//! moment that is showing: a bake is used for as far as it has got.

use crate::chrome::TILE_RADIUS;
use fluent::{blur_into, queue_target_image, release_image, write_image, ImageId, PlatePass, Theme};
use mewrk_client::appearance::{glass_tint_alpha, replaces_theme_ground};
use mewrk_client::prelude::*;
use mewrk_motion::player::Cancel;
use mewrk_motion::{FlipWriter, Flipbook, Tick};
use mewrk_settings_global::effects::{backdrop_media, backdrop_source, tick_backdrop, BackdropMedia};
use mewrk_settings_global::logic::background::{parse_background, Background, BackgroundScheme};
use mewrk_settings_global::logic::background_image::{area_resize, decode, decode_data_url, Pixels};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layers {
    pub custom: bool,
    pub glass: bool,
    /// `glassTransparency`.
    pub transparency: u32,
}

impl Layers {
    /// Whether the window shows `AppBackdrop` behind its gutter.
    pub fn layered(self) -> bool {
        self.custom || self.glass
    }
}

thread_local! {
    // The grounds are asked for all over the tree, and these change with a setting: one read a frame, not one argument each.
    static LAYERS: Cell<Layers> = const { Cell::new(Layers { custom: false, glass: false, transparency: 0 }) };
    /// The glass surfaces of this frame, as the view lays them out, so the plate is under them in the frame they move in.
    static SURFACES: RefCell<Vec<(Rect, [f32; 4])>> = const { RefCell::new(Vec::new()) };
    static MOTION_TICK: Cell<Tick> = const { Cell::new(Tick { changed: false, next: None, rebuild: false }) };
}

/// Reads the settings for this frame's grounds, and sees to the plate.
pub fn begin_frame(app: &mut App) {
    let appearance = &app.doc.global_settings.appearance;
    let layers = Layers { custom: replaces_theme_ground(&appearance.background), glass: appearance.liquid_glass, transparency: appearance.glass_transparency };
    LAYERS.with(|cell| cell.set(layers));
    SURFACES.with(|surfaces| surfaces.borrow_mut().clear());
    keep_plate(app);
    MOTION_TICK.with(|tick| tick.set(tick_motion(app, Instant::now())));
}

/// Moves the background on to what is due, and the glass with it. Between frames this is all a moving background needs: the scene already
/// on hand draws the new pixels, with no view to build. `changed` says whether it has to.
pub fn tick_motion(app: &mut App, now: Instant) -> Tick {
    let tick = tick_backdrop(app, now);
    show_plate(app);
    start_bake_when_settled(app);
    tick
}

/// What the frame's own tick found, for the frame loop to look again when it says.
pub fn frame_tick() -> Tick {
    MOTION_TICK.with(Cell::get)
}

/// Whether a frame shows anything that moves, and so whether the background need go on playing.
pub fn shows_motion(app: &mut App, scene: &fluent::Scene) -> bool {
    let value = app.doc.global_settings.appearance.background.clone();
    let background = backdrop_source(app, &value).map(|source| source.image);
    let plate = app.local::<Glass>().plate.map(|plate| plate.image);
    [background, plate].into_iter().flatten().any(|image| scene.draws_image(image))
}

/// Whether anything shows through a glass surface: all the way down, its tint covers whatever would lie under it.
fn see_through() -> bool {
    let layers = layers();
    layers.glass && layers.transparency > 0
}

/// Lays the glass under a surface, `r` being where it lies in the window: `<GlassPlate />` as a surface's first child.
pub fn surface(r: Rect, radii: [f32; 4]) {
    if see_through() {
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
    let (color, alpha) = if theme().dark { (0x212121, 0.84) } else { (0xF7F7F5, 0.78) };
    Color::rgb(color).with_alpha(glass_tint_alpha(alpha, layers().transparency))
}

/// `--glass-pane`: a side pane.
pub fn glass_pane() -> Color {
    let (color, alpha) = if theme().dark { (0x1B1B1B, 0.86) } else { (0xEFEFEB, 0.80) };
    Color::rgb(color).with_alpha(glass_tint_alpha(alpha, layers().transparency))
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
/// sidebar and the tiles cover what is not theirs to show. None of it lies over the tiles, where a wash would be one more blend over the page.
fn paint_strip(ui: &mut Ui<'_>, bar: Rect, sidebar_open: bool) {
    let tint = strip_tint();
    let solid = ((bar.h + STRIP_REACH) * 0.3).min(bar.h);
    ui.fill(rect(bar.x, bar.y, bar.w, solid), 0.0, tint);
    ui.fill_gradient(rect(bar.x, bar.y + solid, bar.w, bar.h - solid), [0.0; 4], tint, fade(tint, strip_share(bar.h, bar.h)));
    if sidebar_open {
        ui.fill(rect(bar.x - TILE_RADIUS, bar.y, TILE_RADIUS, TILE_RADIUS), 0.0, tint);
    }
}

// ---------------------------------------------------------------- the bake

/// `--glass-saturate`.
pub const GLASS_SATURATE: f32 = 1.3;
/// How many pixels of the baked copy the blur's standard deviation spans: enough that scaling it up shows nothing.
const PLATE_SIGMA: f32 = 3.0;
/// The longest side a baked copy may have, should a small blur over a large picture ask for more, and of the copy of the picture kept
/// to bake from.
const PLATE_MAX: u32 = 1024;
/// The blur is set in the window's pixels, so a window resized far enough wants another bake; a few percent either way is not to be seen.
const FIT_STEP: f32 = 1.05;
/// The same for what moves: under a picture that doesn't hold still a tenth either way can't be seen, and another bake costs a lot more.
const MOTION_FIT_STEP: f32 = 1.2;
/// How long the window's size and the blur setting have to stay put before that bake.
const FIT_SETTLES: Duration = Duration::from_millis(150);
/// The same for what moves, which has more to throw away if they move again.
const MOTION_FIT_SETTLES: Duration = Duration::from_millis(600);
/// What to ask of an imported picture's tiers: the smallest one this wide holds all that the finest blur leaves.
const IMPORTED_TIER: (u32, u32) = (PLATE_MAX, 1);
/// Bumped whenever the filter changes, so that nothing baked before is shown again.
const PLATE_VERSION: u32 = 1;
/// A heavy blur doesn't change quickly, so past this rate a bake would only spend disk.
const PLATE_FRAME_RATE: f64 = 30.0;
/// The most one background's glass may take on disk. A video long enough to need more is baked at a lower rate.
const PLATE_BYTES: u64 = 768 << 20;
/// The most all baked glass may take on disk. The longest unused goes first.
const PLATES_BYTES: u64 = 3 << 30;

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
/// per standard deviation across holds all that is left, and bilinear scaling draws it at the window's size. `blur` is in the window's pixels.
pub fn bake(picture: &Pixels, scale: f32, blur: u32) -> Result<Pixels, String> {
    let sigma = blur as f32 / scale.max(1e-3);
    let shrink = (PLATE_SIGMA / sigma.max(1e-3)).min(PLATE_MAX as f32 / picture.width.max(picture.height) as f32).min(1.0);
    let (width, height) = (((picture.width as f32 * shrink).round() as u32).max(1), ((picture.height as f32 * shrink).round() as u32).max(1));
    filtered(picture, width, height, sigma * (width as f32 / picture.width as f32))
}

/// The filter, step by step: this size, this blur in its pixels, and saturation. What the GPU's live pass does has to match.
pub fn filtered(picture: &Pixels, width: u32, height: u32, sigma: f32) -> Result<Pixels, String> {
    let mut plate = area_resize(picture, width, height)?;
    blur_pixels(&mut plate.data, width as usize, height as usize, sigma);
    filter_pixels(&mut plate.data, GLASS_SATURATE);
    Ok(plate)
}

/// The size a picture of this size is kept at to bake from.
fn reference_size(natural: (u32, u32)) -> (u32, u32) {
    let shrink = (PLATE_MAX as f32 / natural.0.max(natural.1).max(1) as f32).min(1.0);
    (((natural.0 as f32 * shrink).round() as u32).max(1), ((natural.1 as f32 * shrink).round() as u32).max(1))
}

/// The copy of a picture to keep for baking: a bake never wants more of it than this.
fn bake_source(picture: Pixels) -> Result<Pixels, String> {
    let (width, height) = reference_size((picture.width, picture.height));
    if (width, height) == (picture.width, picture.height) {
        return Ok(picture);
    }
    area_resize(&picture, width, height)
}

/// A plate as it has to be for a window and blur: everything that goes into one, and all a bake's name needs to say.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlateSpec {
    /// The blur in steps, as it falls on the picture.
    pub step: i32,
    pub size: (u32, u32),
    /// The blur in the plate's own pixels.
    pub sigma: f32,
}

/// `natural` is the size the background is stored at. Fits within a step share a spec, whichever window and blur got there, and the
/// blur baked is the step's own, so the same step always bakes to the same pixels and can be kept.
pub fn plate_spec(natural: (u32, u32), window: (f32, f32), blur: u32, moves: bool) -> PlateSpec {
    let reference = reference_size(natural);
    if blur == 0 {
        return PlateSpec { step: i32::MIN, size: reference, sigma: 0.0 };
    }
    let base = if moves { MOTION_FIT_STEP } else { FIT_STEP };
    let sigma = blur as f32 / cover_scale(window, reference).max(1e-3);
    let step = (sigma.max(1e-3).ln() / base.ln()).round() as i32;
    let sigma = base.powi(step);
    let shrink = (PLATE_SIGMA / sigma).min(1.0);
    let (width, height) = (((reference.0 as f32 * shrink).round() as u32).max(1), ((reference.1 as f32 * shrink).round() as u32).max(1));
    PlateSpec { step, size: (width, height), sigma: sigma * (width as f32 / reference.0 as f32) }
}

/// How many frames of glass to set aside room for, and how far apart: as many as there are, up to what the disk budget allows.
pub fn plate_timing(duration: f64, frame_rate: f64, frame_bytes: u64) -> (u32, f64) {
    let duration = duration.max(1e-3);
    let budget = (PLATE_BYTES / frame_bytes.max(1)).max(1) as f64;
    let rate = frame_rate.clamp(1.0, PLATE_FRAME_RATE).min(budget / duration);
    (((duration * rate).ceil() as u32 + 2).min(budget as u32).max(1), 1.0 / rate)
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
    /// A still picture's copy, kept to bake from when the fit moves.
    source: Option<Arc<Pixels>>,
    plate: Option<Plate>,
    /// `object-position` of the picture.
    focus: (f32, f32),
    /// What the plate is for.
    spec: Option<PlateSpec>,
    /// The bake for that, as far as it has got.
    book: Option<Arc<Flipbook>>,
    /// The frame of it in the plate.
    written: Option<u32>,
    /// The `serial` of what the GPU last worked the plate out from.
    live_of: Option<u64>,
    /// The bake under way, to stop if it stops being wanted.
    baking: Option<Cancel>,
    /// Since when the spec has been what it is, and whether it is the first since the background changed: that one has nothing better to show.
    moved: Option<Instant>,
    /// A bake that couldn't be made leaves the live pass to carry on, and isn't tried again until something changes.
    unbakeable: bool,
    /// The newest load or bake: anything older that arrives is dropped.
    token: u64,
    bakes: u32,
    frames_live: u64,
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

fn plates_folder(app: &App) -> Option<PathBuf> {
    app.app_data_dir().map(|dir| dir.join("glass-plates"))
}

/// Where a bake is kept. An image's plates begin with its id: the host removes them along with it.
fn plate_path(folder: &Path, value: &str, spec: PlateSpec) -> PathBuf {
    let (width, height) = spec.size;
    folder.join(format!("{}-v{PLATE_VERSION}-{}-{width}x{height}.flip", value.replace(':', "_"), spec.step))
}

/// A bake that has finished, whichever kind: from here on the file is all there is to it.
fn adopt_bake(app: &mut App, token: u64, book: Arc<Flipbook>, path: PathBuf, source: Option<Arc<Pixels>>) {
    let folder = plates_folder(app);
    let glass = app.local::<Glass>();
    if glass.token != token {
        return;
    }
    if source.is_some() {
        glass.source = source;
    }
    glass.book = Some(book);
    glass.written = None;
    glass.baking = None;
    glass.bakes += 1;
    repaint();
    if let Some(folder) = folder {
        std::thread::spawn(move || mewrk_motion::flipbook::prune(&folder, PLATES_BYTES, &path));
    }
}

fn bake_failed(app: &mut App, token: u64, reason: &str) {
    let glass = app.local::<Glass>();
    if glass.token == token {
        eprintln!("the glass could not be baked: {reason}");
        glass.baking = None;
        glass.unbakeable = true;
    }
}

/// Bakes a still picture into a book of one frame.
fn bake_still(task: &Task, value: &str, known: Option<Arc<Pixels>>, spec: PlateSpec, path: &Path) -> Result<(Arc<Flipbook>, Arc<Pixels>), String> {
    let source = match known {
        Some(source) => source,
        None => Arc::new(pixels_of(task, value).ok_or_else(|| "the picture can't be read".to_owned()).and_then(bake_source)?),
    };
    let plate = filtered(&source, spec.size.0.min(source.width), spec.size.1.min(source.height), spec.sigma)?;
    let mut writer = FlipWriter::create(path, plate.width, plate.height, 1)?;
    writer.push(&plate.data, 1);
    Ok((writer.finish()?, source))
}

/// Bakes every frame of a book of whole frames.
fn bake_animated(source: &Flipbook, mut writer: FlipWriter, spec: PlateSpec, step: f64, cancel: &Cancel) -> Result<Arc<Flipbook>, String> {
    let (width, height) = source.size();
    let mut next_due = 0u64;
    let mut pending: Option<(Pixels, u64)> = None;
    let count = source.count();
    for index in 0..count {
        if cancel.is_cancelled() {
            return Err("given up".to_owned());
        }
        let start = if index == 0 { 0 } else { source.end_us(index - 1).unwrap_or(0) };
        if start < next_due {
            continue;
        }
        next_due = start + (step * 1e6) as u64;
        let frame = source.frame(index).ok_or_else(|| "a frame went missing".to_owned())?;
        let picture = Pixels { width, height, data: frame.as_ref().to_vec() };
        let plate = filtered(&picture, spec.size.0, spec.size.1, spec.sigma)?;
        if let Some((previous, _)) = pending.replace((plate, start)) {
            writer.push(&previous.data, start);
        }
    }
    if let Some((last, _)) = pending {
        writer.push(&last.data, source.duration_us().max(1));
    }
    writer.finish()
}

/// Bakes a video from start to end, at a size the hardware decoder scales to on the way out: a few times the plate's, so the average
/// that makes each pixel of it still has something to average.
#[cfg(target_os = "macos")]
fn bake_video(video: &Path, info: mewrk_motion::VideoInfo, mut writer: FlipWriter, spec: PlateSpec, step: f64, cancel: &Cancel) -> Result<Arc<Flipbook>, String> {
    let even = |side: u32, most: u32| (side * 2).next_multiple_of(2).min(most);
    let decoded = (even(spec.size.0, info.width), even(spec.size.1, info.height));
    let mut reader = mewrk_motion::video::VideoReader::open(video, Some(decoded), 0.0)?;
    let mut next_due = 0.0;
    let mut pending: Option<Pixels> = None;
    let mut last_time = 0.0;
    while let Some(frame) = reader.next_frame() {
        if cancel.is_cancelled() {
            return Err("given up".to_owned());
        }
        if frame.time + 1e-4 < next_due {
            continue;
        }
        next_due = frame.time + step;
        last_time = frame.time;
        let picture = frame.to_pixels()?;
        drop(frame);
        let plate = filtered(&picture, spec.size.0.min(picture.width), spec.size.1.min(picture.height), spec.sigma)?;
        if let Some(previous) = pending.replace(plate) {
            writer.push(&previous.data, (last_time * 1e6) as u64);
        }
    }
    if !reader.ended_cleanly() {
        return Err("the video stopped being readable".to_owned());
    }
    if let Some(last) = pending {
        writer.push(&last.data, ((info.duration.max(last_time + 1e-3)) * 1e6) as u64);
    }
    writer.finish()
}

/// Takes the glass away, and whatever is on its way.
fn clear_plate(glass: &mut Glass) {
    if let Some(old) = glass.plate.take() {
        release_image(old.image);
    }
    if let Some(bake) = glass.baking.take() {
        bake.cancel();
    }
    glass.value = None;
    glass.source = None;
    glass.spec = None;
    glass.book = None;
    glass.written = None;
    glass.live_of = None;
    glass.moved = None;
    glass.unbakeable = false;
    glass.token += 1;
}

/// `bakeWindow`, once a frame: nothing happens unless the setting, the picture or the window's fit has moved.
fn keep_plate(app: &mut App) {
    let appearance = &app.doc.global_settings.appearance;
    let value = appearance.background.clone();
    let blur = appearance.glass_blur;
    let wanted = appearance.liquid_glass && appearance.glass_transparency > 0 && !matches!(parse_background(&value), Background::Solid(_));
    if !wanted {
        let glass = app.local::<Glass>();
        if glass.value.is_some() {
            clear_plate(glass);
        }
        return;
    }
    let window = window_size();
    let glass = app.local::<Glass>();
    if glass.value.as_deref() != Some(value.as_str()) {
        clear_plate(glass);
        glass.value = Some(value.clone());
        glass.focus = match parse_background(&value) {
            Background::Builtin(picture) => picture.focus,
            _ => (0.5, 0.5),
        };
    }
    // Nothing to fit until something has arrived, and until the host has said whether it moves there's no knowing what to bake.
    let Some(source) = backdrop_source(app, &value) else { return };
    let moves = matches!(backdrop_media(app), BackdropMedia::Video { .. } | BackdropMedia::Animated { .. });
    let spec = plate_spec(source.natural, window, blur, moves);
    let folder = plates_folder(app);
    let glass = app.local::<Glass>();
    if glass.spec != Some(spec) {
        let first = glass.spec.is_none();
        if let Some(bake) = glass.baking.take() {
            bake.cancel();
        }
        if glass.plate.is_none_or(|plate| plate.size != spec.size) {
            if let Some(old) = glass.plate.take() {
                release_image(old.image);
            }
            glass.plate = Some(Plate { image: queue_target_image(spec.size.0, spec.size.1), size: spec.size });
        }
        glass.token += 1;
        glass.spec = Some(spec);
        glass.book = None;
        glass.written = None;
        glass.live_of = None;
        glass.unbakeable = false;
        glass.moved = (!first).then(Instant::now);
        // Baked before, for this background, this fit and this blur.
        if let Some(path) = folder.map(|folder| plate_path(&folder, &value, spec))
            && let Ok(book) = Flipbook::open(&path)
            && book.size() == spec.size
        {
            mewrk_motion::flipbook::touch(&path);
            glass.book = Some(book);
            glass.bakes += 1;
        }
    }
}

/// Starts the bake this fit wants, once it has stopped moving and something knows what to bake.
fn start_bake_when_settled(app: &mut App) {
    let glass = app.local::<Glass>();
    let (Some(value), Some(spec)) = (glass.value.clone(), glass.spec) else { return };
    if glass.book.as_ref().is_some_and(|book| book.is_complete()) || glass.baking.is_some() || glass.unbakeable {
        return;
    }
    let media = backdrop_media(app);
    let settles = match media {
        BackdropMedia::Unknown => return,
        BackdropMedia::Still => FIT_SETTLES,
        BackdropMedia::Video { .. } | BackdropMedia::Animated { .. } => MOTION_FIT_SETTLES,
    };
    let Some(folder) = plates_folder(app) else { return };
    let path = plate_path(&folder, &value, spec);
    let glass = app.local::<Glass>();
    if glass.moved.is_some_and(|since| since.elapsed() < settles) {
        repaint();
        return;
    }
    glass.moved = None;
    let token = glass.token;
    let cancel = Cancel::default();
    let frame_bytes = u64::from(spec.size.0) * u64::from(spec.size.1) * 4;
    match media {
        BackdropMedia::Unknown => {}
        BackdropMedia::Still => {
            glass.baking = Some(cancel);
            let known = glass.source.clone();
            app.spawn(move |task| {
                let baked = bake_still(&task, &value, known, spec, &path);
                task.post(move |app| match baked {
                    Ok((book, source)) => adopt_bake(app, token, book, path, Some(source)),
                    Err(reason) => bake_failed(app, token, &reason),
                });
            });
        }
        BackdropMedia::Animated { book } => {
            let (capacity, step) = plate_timing(book.duration_us() as f64 / 1e6, f64::from(book.count()) / (book.duration_us().max(1) as f64 / 1e6), frame_bytes);
            match FlipWriter::create(&path, spec.size.0, spec.size.1, capacity) {
                Ok(writer) => {
                    glass.book = Some(writer.book());
                    glass.baking = Some(cancel.clone());
                    app.spawn(move |task| {
                        mewrk_motion::lower_this_threads_priority();
                        let baked = bake_animated(&book, writer, spec, step, &cancel);
                        task.post(move |app| match baked {
                            Ok(book) => adopt_bake(app, token, book, path, None),
                            Err(reason) => bake_failed(app, token, &reason),
                        });
                    });
                }
                Err(reason) => bake_failed(app, token, &reason),
            }
        }
        #[cfg(target_os = "macos")]
        BackdropMedia::Video { path: video, info } => {
            let (capacity, step) = plate_timing(info.duration, f64::from(info.frame_rate), frame_bytes);
            match FlipWriter::create(&path, spec.size.0, spec.size.1, capacity) {
                Ok(writer) => {
                    glass.book = Some(writer.book());
                    glass.baking = Some(cancel.clone());
                    app.spawn(move |task| {
                        mewrk_motion::lower_this_threads_priority();
                        let baked = bake_video(&video, info, writer, spec, step, &cancel);
                        task.post(move |app| match baked {
                            Ok(book) => adopt_bake(app, token, book, path, None),
                            Err(reason) => bake_failed(app, token, &reason),
                        });
                    });
                }
                Err(reason) => bake_failed(app, token, &reason),
            }
        }
        #[cfg(not(target_os = "macos"))]
        BackdropMedia::Video { .. } => {}
    }
}

/// Puts into the plate whatever it should hold at this moment: the bake's frame if it has got this far, and otherwise what the GPU works
/// out from what is on screen. Neither runs unless the moment has moved on.
fn show_plate(app: &mut App) {
    let value = app.doc.global_settings.appearance.background.clone();
    let Some(source) = backdrop_source(app, &value) else { return };
    let glass = app.local::<Glass>();
    let (Some(plate), Some(spec)) = (glass.plate, glass.spec) else { return };
    if let Some(book) = &glass.book
        && let Some(index) = book.index_at(source.time_us)
    {
        if glass.written != Some(index)
            && let Some(frame) = book.frame(index)
        {
            write_image(plate.image, 0, 0, plate.size.0, plate.size.1, frame);
            glass.written = Some(index);
            glass.live_of = None;
            book.prefetch(index + 1);
        }
        return;
    }
    glass.written = None;
    if glass.live_of != Some(source.serial) {
        blur_into(PlatePass { source: source.image, target: plate.image, sigma: spec.sigma, saturate: GLASS_SATURATE });
        glass.live_of = Some(source.serial);
        glass.frames_live += 1;
    }
}

/// The plate on screen, for tests.
pub fn plate(app: &mut App) -> Option<Plate> {
    app.local::<Glass>().plate
}

/// How many times the glass has been baked, or found already baked, for tests.
pub fn bakes(app: &mut App) -> u32 {
    app.local::<Glass>().bakes
}

/// How many frames of glass the GPU has had to work out because no bake had got there, for tests.
pub fn frames_live(app: &mut App) -> u64 {
    app.local::<Glass>().frames_live
}

/// The bake under way or done, for tests.
pub fn baked_book(app: &mut App) -> Option<Arc<Flipbook>> {
    app.local::<Glass>().book.clone()
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

/// What lies under a glass surface this frame: the plate once there is something to make it from, a colour over a solid ground, and nothing until then.
#[derive(Clone, Copy)]
struct Under {
    picture: Option<Plate>,
    focus: (f32, f32),
    flat: Option<Color>,
    /// The plate is as solid as what it is made from.
    opaque: bool,
}

impl Under {
    fn now(app: &mut App) -> Self {
        let on = layers().glass;
        let value = app.doc.global_settings.appearance.background.clone();
        let flat = if on { flat_plate(&value) } else { None };
        let opaque = backdrop_source(app, &value).is_some_and(|source| source.opaque);
        let glass = app.local::<Glass>();
        Self { picture: glass.plate.filter(|_| on), focus: glass.focus, flat, opaque }
    }

    fn paint(self, ui: &mut Ui<'_>, window: Rect, r: Rect, radii: [f32; 4]) {
        if let Some(picture) = self.picture {
            ui.image_fixed(picture.image, r, radii, window, picture.size, self.focus);
            if self.opaque {
                ui.image_is_opaque();
            }
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
    if !see_through() {
        return None;
    }
    let under = Under::now(app);
    Some(custom::<App>(move |_, ui, area| {
        let (window_w, window_h) = window_size();
        under.paint(ui, rect(0.0, 0.0, window_w, window_h), area, radii);
    }).absolute().left(r.x).top(r.y).size(r.w, r.h).pass_through())
}
