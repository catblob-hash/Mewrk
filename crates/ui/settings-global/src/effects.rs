//! What upstream's `applyAppearance` does through CSS, for a Fluent window.
//!
//! The look stays Fluent, so these only change what they can in it: the accent, the code theme, the zoom, the message size
//! and the window's backdrop. The shell picks them up from `root::theme` and the frame driver.

use std::sync::atomic::{AtomicU64, Ordering};

use fluent::theme::{CodePalette, Color, Theme, TypeStyle, Weight};
use fluent::{queue_image, release_image, ImageId};
use mewrk_client::appearance::{clamp_message_font_size, clamp_zoom, code_theme_for, normalize_hex_color, CodeTheme, CodeThemeScheme};
use mewrk_client::model::{AppearancePreferences, ThemePreference};
use mewrk_client::prelude::*;

use crate::logic::background::{background_after_theme_change, parse_background, Background, BackgroundScheme};
use crate::logic::background_image::{decode, decode_data_url, tier_covers};
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

#[derive(Default)]
struct Backdrop {
    value: String,
    picture: Option<Picture>,
    /// What it was asked for at, so a window that grew past the tier asks for a larger one.
    asked: (f64, f64),
    largest: bool,
    token: u64,
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
                    let picture = upload(pixels.data, pixels.width, pixels.height);
                    task.post(move |app| adopt(app, token, picture, true));
                }
            });
        }
        Background::Imported(id) => {
            let args = json!({ "imageId": id, "width": width.round().clamp(1.0, 32768.0) as u32, "height": height.round().clamp(1.0, 32768.0) as u32 });
            app.spawn(move |task| {
                let Ok(tier) = task.invoke::<Value>("background_image_data", args) else { return };
                let largest = tier.get("largest").and_then(Value::as_bool).unwrap_or(true);
                let Some(url) = tier.get("dataUrl").and_then(Value::as_str) else { return };
                if let Ok(pixels) = decode_data_url(url) {
                    let picture = upload(pixels.data, pixels.width, pixels.height);
                    task.post(move |app| adopt(app, token, picture, largest));
                }
            });
        }
    }
}

fn adopt(app: &mut App, token: u64, picture: Picture, largest: bool) {
    let d = app.local::<Backdrop>();
    if d.token != token {
        release_image(picture.id);
        return;
    }
    if let Some(old) = d.picture.replace(picture) {
        release_image(old.id);
    }
    d.largest = largest;
    repaint();
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
        if let Some(old) = d.picture.take() {
            release_image(old.id);
        }
        load_backdrop(app, &value, w, h);
    } else {
        // A tier smaller than the window asks for the one that covers it, unless it's the largest there is.
        let d = app.local::<Backdrop>();
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
            let picture = app.local::<Backdrop>().picture?;
            Some(custom::<App>(move |_, ui, r| ui.image_cover_at(picture.id, r, picture.size, focus, 0.0, 1.0)).absolute().inset(0.0).pass_through())
        }
    }
}

/// What `window_backdrop` has loaded, for tests.
pub fn backdrop_size(app: &mut App) -> Option<(u32, u32)> {
    app.local::<Backdrop>().picture.map(|p| p.size)
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
    fn each_scheme_reads_its_own_code_theme() {
        let mut appearance = AppearancePreferences::default();
        appearance.code_theme_day = "github-light".into();
        appearance.code_theme_night = "nope".into();
        assert_eq!(themed(Theme::light(), &appearance).code.keyword, Color::rgb(0xCF222E));
        assert_eq!(themed(Theme::dark(), &appearance).code, CodePalette::mewrk(true));
        assert_eq!(themed(Theme::light(), &AppearancePreferences::default()).code, CodePalette::mewrk(false));
    }
}
