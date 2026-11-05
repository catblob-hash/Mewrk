//! What upstream's `applyAppearance` does through CSS, for a Fluent window.
//!
//! The look stays Fluent, so these only change what they can in it: the accent, the code theme, the zoom, the message size
//! and the window's backdrop. The shell picks them up from `root::theme` and the frame driver.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use fluent::theme::{CodePalette, Color, Theme, TypeStyle, Weight};
use fluent::{queue_image, release_image, ImageId};
use mewrk_client::appearance::{clamp_message_font_size, clamp_zoom, code_theme_for, normalize_hex_color, CodeTheme, CodeThemeScheme};
use mewrk_client::model::{AppearancePreferences, ThemePreference};
use mewrk_client::prelude::*;

use mewrk_motion::{Flipbook, Player, Tick, VideoInfo};

use crate::logic::background::{background_after_theme_change, parse_background, Background, BackgroundScheme};
use crate::logic::background_image::{decode, decode_data_url, is_opaque, tier_covers};
use crate::logic::store;

/// `#rrggbb` as a colour. The tables only hold valid ones, and anything else reads as black rather than failing a frame.
pub fn hex(color: &str) -> Color {
    let digits = color.strip_prefix('#').unwrap_or(color);
    Color::rgb(u32::from_str_radix(digits, 16).unwrap_or(0))
}

/// A code theme as what fluent's code surfaces read.
pub fn palette_of(theme: &CodeTheme) -> CodePalette {
    CodePalette {
        adaptive: theme.adaptive,
        background: hex(theme.background),
        foreground: hex(theme.foreground),
        line_number: hex(theme.line_number),
        comment: hex(theme.comment),
        comment_italic: theme.comment_italic,
        string: hex(theme.string),
        number: hex(theme.number),
        keyword: hex(theme.keyword),
        function: hex(theme.function),
        type_: hex(theme.type_),
        meta: hex(theme.meta),
        tag: hex(theme.tag),
        attribute: hex(theme.attribute),
        property: hex(theme.property),
        variable: hex(theme.variable),
        added: hex(theme.added),
        removed: hex(theme.removed),
    }
}

/// Black or white, whichever reads on this ground.
fn on_accent(c: Color) -> Color {
    let luma = 0.2126 * c.r as f32 + 0.7152 * c.g as f32 + 0.0722 * c.b as f32;
    if luma > 140.0 { Color::BLACK } else { Color::WHITE }
}

/// `root::theme`'s last step: Fluent's light or dark tokens with the user's accent and code theme on them.
pub fn themed(mut base: Theme, appearance: &AppearancePreferences) -> Theme {
    let scheme = if base.dark { CodeThemeScheme::Night } else { CodeThemeScheme::Day };
    let id = if base.dark { &appearance.code_theme_night } else { &appearance.code_theme_day };
    base.code = palette_of(code_theme_for(id, scheme));
    if let Some(color) = normalize_hex_color(&appearance.theme_color).filter(|c| !c.is_empty()) {
        let accent = hex(&color);
        base.accent = accent;
        base.accent_hover = accent.with_alpha(0.9);
        base.accent_pressed = accent.with_alpha(0.8);
        base.accent_text = if base.dark { accent.mix(Color::WHITE, 0.3) } else { accent.mix(Color::BLACK, 0.2) };
        base.attention = accent;
        base.selection = accent.with_alpha(if base.dark { 0.35 } else { 0.28 });
        base.text_on_accent = on_accent(accent);
    }
    base
}

/// What the frame driver multiplies its scale by: `zoom` scales layout, hit testing and scrollbars together.
pub fn zoom(app: &App) -> f32 {
    clamp_zoom(app.doc.global_settings.appearance.zoom) as f32
}

/// The base style of a message's text, for the markdown stream: size from Appearance, leading in proportion.
pub fn message_type(app: &App) -> TypeStyle {
    let size = clamp_message_font_size(f64::from(app.doc.global_settings.appearance.message_font_size)) as f32;
    TypeStyle { size, line: (size * 1.43).round(), weight: Weight::Regular, mono: false, italic: false, serif: false }
}

/// A solid background follows the change of theme, whichever control changed it. Call it with what's on screen each
/// frame, or when it changes. `App.tsx` does this in an effect on the resolved theme.
pub fn on_scheme_changed(app: &mut App, dark: bool) {
    #[derive(Default)]
    struct Followed(Option<bool>);
    let previous = app.local::<Followed>().0.replace(dark);
    if previous.is_none() || previous == Some(dark) {
        return;
    }
    let next = background_after_theme_change(&app.doc.global_settings.appearance.background);
    if next != app.doc.global_settings.appearance.background {
        store::change(app, move |doc| doc.global_settings.appearance.background = next);
    }
}

/// The window came back into focus: the panels that read something a terminal may have changed. Call it on the first
/// focus after a blur.
pub fn window_refocused(app: &mut App) {
    if crate::shell::current_view(app) == Some(crate::shell::SettingsView::Providers) {
        crate::providers::window_refocused(app);
    }
    crate::permissions_page::window_refocused(app);
}

pub fn resolved_scheme(app: &App) -> BackgroundScheme {
    let dark = match app.doc.global_settings.theme {
        ThemePreference::Day => false,
        ThemePreference::Night => true,
        ThemePreference::System => app.system_dark,
    };
    if dark { BackgroundScheme::Night } else { BackgroundScheme::Day }
}

// ---------------------------------------------------------------- pictures

/// A decoded, uploaded picture.
#[derive(Clone, Copy)]
pub struct Picture {
    pub id: ImageId,
    pub size: (u32, u32),
}

/// What the chosen background is, once the host has said: only a file beside its first frame tells it moves.
#[derive(Clone, Default)]
pub enum BackdropMedia {
    /// Still asking, or decoding its frames. Its first frame is on screen by then.
    #[default]
    Unknown,
    Still,
    Video { path: PathBuf, info: VideoInfo },
    Animated { book: Arc<Flipbook> },
}

/// What the window's background is showing right now, for whatever is worked out from it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackdropSource {
    pub image: ImageId,
    /// The size of the picture as stored, not of this texture: it doesn't change as tiers and frames come and go.
    pub natural: (u32, u32),
    /// The time of what is showing, within the loop. A still picture's stays at zero.
    pub time_us: u64,
    /// Moves on each time the pixels in `image` change, or `image` does.
    pub serial: u64,
    /// Every pixel of it is solid, so what lies under it can go, and under anything made from it.
    pub opaque: bool,
}

#[derive(Default)]
struct Backdrop {
    value: String,
    picture: Option<Picture>,
    /// What it was asked for at, so a window that grew past the tier asks for a larger one.
    asked: (f64, f64),
    largest: bool,
    token: u64,
    media_token: u64,
    /// Whether the picture has no holes.
    opaque: bool,
    natural: (u32, u32),
    media: BackdropMedia,
    player: Option<Player>,
    /// The size a video is being decoded at.
    output: (u32, u32),
    serial: u64,
    /// Nothing it shows can be seen: the window is out of sight, or something solid covers all of it.
    window_hidden: bool,
    covered: bool,
}

static NEXT_LOAD: AtomicU64 = AtomicU64::new(1);

/// Uploads a decoded picture from a background thread, and asks for the frame that shows it.
fn upload(rgba: Vec<u8>, width: u32, height: u32) -> Picture {
    Picture { id: queue_image(rgba, width, height), size: (width, height) }
}

fn load_backdrop(app: &mut App, value: &str, width: f64, height: f64) {
    let token = NEXT_LOAD.fetch_add(1, Ordering::Relaxed);
    let d = app.local::<Backdrop>();
    d.token = token;
    d.asked = (width, height);
    match parse_background(value) {
        Background::Solid(_) => {}
        Background::Builtin(b) => {
            let bytes = b.picture;
            app.spawn(move |task| {
                if let Ok(pixels) = decode(bytes) {
                    let natural = (pixels.width, pixels.height);
                    let opaque = is_opaque(&pixels);
                    let picture = upload(pixels.data, pixels.width, pixels.height);
                    task.post(move |app| adopt(app, token, picture, true, natural, opaque));
                }
            });
        }
        Background::Imported(id) => {
            let args = json!({ "imageId": id, "width": width.round().clamp(1.0, 32768.0) as u32, "height": height.round().clamp(1.0, 32768.0) as u32 });
            app.spawn(move |task| {
                let Ok(tier) = task.invoke::<Value>("background_image_data", args) else { return };
                let largest = tier.get("largest").and_then(Value::as_bool).unwrap_or(true);
                let full = |key: &str| tier.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
                let natural = (full("fullWidth"), full("fullHeight"));
                let Some(url) = tier.get("dataUrl").and_then(Value::as_str) else { return };
                if let Ok(pixels) = decode_data_url(url) {
                    let opaque = is_opaque(&pixels);
                    let picture = upload(pixels.data, pixels.width, pixels.height);
                    task.post(move |app| adopt(app, token, picture, largest, natural, opaque));
                }
            });
        }
    }
}

fn adopt(app: &mut App, token: u64, picture: Picture, largest: bool, natural: (u32, u32), opaque: bool) {
    let d = app.local::<Backdrop>();
    if d.token != token {
        release_image(picture.id);
        return;
    }
    if let Some(old) = d.picture.replace(picture) {
        release_image(old.id);
    }
    d.largest = largest;
    d.opaque = opaque;
    d.natural = if natural.0 > 0 && natural.1 > 0 { natural } else { picture.size };
    d.serial += 1;
    repaint();
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MotionFile {
    kind: String,
    source: PathBuf,
    frames: PathBuf,
}

/// The smallest even size that covers the window, or the video's own if that is smaller: nothing is gained from decoding more pixels
/// than the window has, and without mip levels, drawing many more would only shimmer.
pub fn video_output_size(video: (u32, u32), window: (f64, f64)) -> (u32, u32) {
    let scale = (window.0 / f64::from(video.0.max(1))).max(window.1 / f64::from(video.1.max(1))).min(1.0);
    let even = |side: u32| ((f64::from(side) * scale).ceil() as u32).next_multiple_of(2).min(side);
    (even(video.0), even(video.1))
}

/// Finds out whether a background moves and gets it ready to. Until then the first frame stands in. With reduced motion asked for, it stays that way.
fn load_media(app: &mut App, value: &str) {
    let token = NEXT_LOAD.fetch_add(1, Ordering::Relaxed);
    app.local::<Backdrop>().media_token = token;
    let Background::Imported(id) = parse_background(value) else {
        app.local::<Backdrop>().media = BackdropMedia::Still;
        return;
    };
    if fluent::reduced_motion() {
        app.local::<Backdrop>().media = BackdropMedia::Still;
        return;
    }
    let args = json!({ "imageId": id });
    app.spawn(move |task| {
        let found = task.invoke::<Option<MotionFile>>("background_image_motion", args).ok().flatten();
        let media = match found {
            Some(file) if file.kind == "video" => mewrk_motion::video::probe(&file.source).map_or(BackdropMedia::Still, |info| BackdropMedia::Video { path: file.source, info }),
            Some(file) => {
                let book = Flipbook::open(&file.frames).or_else(|_| mewrk_motion::animated::decode_to_flipbook(&file.source, &file.frames));
                book.map_or(BackdropMedia::Still, |book| BackdropMedia::Animated { book })
            }
            None => BackdropMedia::Still,
        };
        task.post(move |app| adopt_media(app, token, media));
    });
}

/// The window size a video has to cover. Under glass all but the thin gutters show the baked plate, where the sharp frame is out of sight, so half the pixels do.
fn video_covers(app: &App, window: (f64, f64)) -> (f64, f64) {
    let appearance = &app.doc.global_settings.appearance;
    if appearance.liquid_glass && appearance.glass_transparency > 0 { (window.0 * 0.5, window.1 * 0.5) } else { window }
}

fn adopt_media(app: &mut App, token: u64, media: BackdropMedia) {
    let (w, h) = window_size();
    let covers = video_covers(app, (f64::from(w) * 2.0, f64::from(h) * 2.0));
    let d = app.local::<Backdrop>();
    if d.media_token != token {
        return;
    }
    d.player = match &media {
        BackdropMedia::Animated { book } => Some(Player::flipbook(book.clone())),
        #[cfg(target_os = "macos")]
        BackdropMedia::Video { path, info } => {
            d.output = video_output_size((info.width, info.height), covers);
            Some(Player::video(path.clone(), *info, d.output))
        }
        _ => None,
    };
    d.media = media;
    repaint();
}

/// Moves a background that moves on to what is due, sending any new frame to the GPU. Nothing else needs to run for that frame to show.
pub fn tick_backdrop(app: &mut App, now: Instant) -> Tick {
    let d = app.local::<Backdrop>();
    let paused = d.window_hidden || d.covered || fluent::reduced_motion();
    let Some(player) = d.player.as_mut() else { return Tick::default() };
    player.set_paused(paused, now);
    let mut tick = player.tick(now);
    if tick.changed {
        d.serial += 1;
        // The first real frame replaces the first frame's picture for good, and nothing drawn so far knows its texture.
        if player.image().is_some()
            && let Some(picture) = d.picture.take()
        {
            release_image(picture.id);
            tick.rebuild = true;
        }
    }
    tick
}

/// The window can or can't be seen.
pub fn set_window_visible(app: &mut App, visible: bool) {
    app.local::<Backdrop>().window_hidden = !visible;
}

/// Whether anything of the background got through to the last frame: nothing covers everything often, but a dialog over a maximized page
/// does. Answers whether that just stopped being so, and it has something to catch up on.
pub fn set_backdrop_covered(app: &mut App, covered: bool) -> bool {
    let d = app.local::<Backdrop>();
    let uncovered = d.covered && !covered && d.player.is_some();
    d.covered = covered;
    uncovered
}

/// What is showing, if the chosen background is `value` and anything of it has arrived.
pub fn backdrop_source(app: &mut App, value: &str) -> Option<BackdropSource> {
    let d = app.local::<Backdrop>();
    if d.value != value {
        return None;
    }
    let image = d.player.as_ref().and_then(Player::image).map(|(image, _)| image).or(d.picture.map(|picture| picture.id))?;
    Some(BackdropSource { image, natural: d.natural, time_us: d.player.as_ref().map_or(0, Player::position_us), serial: d.serial, opaque: shown_opaque(d) })
}

/// A video's frames are as solid as its first. A book of frames knows for itself, and it may have holes where its first frame has none.
fn shown_opaque(d: &Backdrop) -> bool {
    d.player.as_ref().and_then(Player::is_flipbook_opaque).unwrap_or(d.opaque)
}

/// What the chosen background turned out to be.
pub fn backdrop_media(app: &mut App) -> BackdropMedia {
    app.local::<Backdrop>().media.clone()
}

/// The window's backdrop, as the first child of the root: a picture, or the other theme's ground. `None` for the theme's
/// own, which is Fluent's mica and needs nothing. It paints behind everything and takes no input.
pub fn window_backdrop(app: &mut App) -> Option<E> {
    let value = app.doc.global_settings.appearance.background.clone();
    let (w, h) = window_size();
    let (w, h) = (w as f64 * 2.0, h as f64 * 2.0);
    let changed = app.local::<Backdrop>().value != value;
    if changed {
        let d = app.local::<Backdrop>();
        d.value = value.clone();
        d.largest = false;
        d.media = BackdropMedia::Unknown;
        d.player = None;
        d.serial += 1;
        if let Some(old) = d.picture.take() {
            release_image(old.id);
        }
        load_backdrop(app, &value, w, h);
        load_media(app, &value);
    } else {
        // A tier smaller than the window asks for the one that covers it, unless it's the largest there is.
        let covers = video_covers(app, (w, h));
        let d = app.local::<Backdrop>();
        // A window that outgrew the size a video is decoded at gets more of it. A smaller one keeps what it has: going back and forth costs more.
        if let (BackdropMedia::Video { info, .. }, Some(player)) = (&d.media, &mut d.player) {
            let wanted = video_output_size((info.width, info.height), covers);
            if f64::from(wanted.0) > f64::from(d.output.0) * 1.1 {
                d.output = wanted;
                player.resize(wanted);
            }
        }
        if let Some(p) = d.picture
            && !d.largest && !tier_covers(p.size, w, h) && (w > d.asked.0 || h > d.asked.1) {
                load_backdrop(app, &value, w, h);
            }
    }
    match parse_background(&value) {
        Background::Solid(None) => None,
        Background::Solid(Some(scheme)) => {
            let ground = if scheme == BackgroundScheme::Night { Theme::dark().mica_base } else { Theme::light().mica_base };
            Some(div().absolute().inset(0.0).bg(ground).pass_through())
        }
        Background::Builtin(_) | Background::Imported(_) => {
            let focus = match parse_background(&value) {
                Background::Builtin(b) => b.focus,
                _ => (0.5, 0.5),
            };
            let d = app.local::<Backdrop>();
            let shown = d.player.as_ref().and_then(Player::image).or(d.picture.map(|picture| (picture.id, picture.size)))?;
            let opaque = shown_opaque(d);
            Some(
                custom::<App>(move |_, ui, r| {
                    ui.image_cover_at(shown.0, r, shown.1, focus, 0.0, 1.0);
                    if opaque {
                        ui.image_is_opaque();
                    }
                })
                .absolute()
                .inset(0.0)
                .pass_through(),
            )
        }
    }
}

/// What `window_backdrop` has loaded, for tests.
pub fn backdrop_size(app: &mut App) -> Option<(u32, u32)> {
    let d = app.local::<Backdrop>();
    d.player.as_ref().and_then(Player::image).map(|(_, size)| size).or(d.picture.map(|p| p.size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chosen_accent_reaches_the_tokens_derived_from_it() {
        let mut appearance = AppearancePreferences::default();
        let plain = themed(Theme::dark(), &appearance);
        assert_eq!(plain.accent, Theme::dark().accent);
        appearance.theme_color = "#356AE6".into();
        let t = themed(Theme::dark(), &appearance);
        assert_eq!(t.accent, Color::rgb(0x356AE6));
        assert_eq!(t.selection.a, Color::rgb(0x356AE6).with_alpha(0.35).a);
        assert_eq!(t.text_on_accent, Color::WHITE);
        appearance.theme_color = "#F59E0B".into();
        assert_eq!(themed(Theme::light(), &appearance).text_on_accent, Color::BLACK);
    }

    #[test]
    fn a_video_is_decoded_at_what_covers_the_window_and_never_above_its_own_size() {
        assert_eq!(video_output_size((3840, 2160), (2880.0, 1800.0)), (3200, 1800));
        assert_eq!(video_output_size((1920, 1080), (5120.0, 2880.0)), (1920, 1080));
        assert_eq!(video_output_size((1920, 1080), (1920.0, 1080.0)), (1920, 1080));
        let (width, height) = video_output_size((1921, 1081), (1000.0, 500.0));
        assert!(width % 2 == 0 && height % 2 == 0, "{width}x{height}");
        assert!(width >= 1000 && height >= 500, "{width}x{height}");
    }

    #[test]
    fn each_scheme_reads_its_own_code_theme() {
        let mut appearance = AppearancePreferences::default();
        appearance.code_theme_day = "github-light".into();
        appearance.code_theme_night = "nope".into();
        assert_eq!(themed(Theme::light(), &appearance).code.keyword, Color::rgb(0xCF222E));
        assert_eq!(themed(Theme::dark(), &appearance).code, CodePalette::mewrk(true));
        assert_eq!(themed(Theme::light(), &AppearancePreferences::default()).code, CodePalette::mewrk(false));
    }
}
