//! `AppearanceSettings/index.tsx`, `rows.tsx`, `ThemeSample.tsx` and `CodeThemePreview.tsx`.

mod background;
mod local_model;

use std::sync::Arc as StdArc;

use fluent::theme::Theme as FluentTheme;
use mewrk_client::appearance::*;
use mewrk_client::model::{AppLanguage, AppearancePreferences, ThemePreference};
use mewrk_client::prelude::*;
use mewrk_client::shortcuts::{bindings_conflict, shortcut_key_caps};

pub use background::{background_library_dialog, background_picture, background_preview, import_background_bytes, import_background_file, thumbnails_in, BackgroundState};
pub use local_model::{local_model_settings, local_model_status, LocalModelPage};

use crate::effects::{hex, palette_of, resolved_scheme, Picture};
use crate::logic::background::{parse_background, Background, BackgroundScheme};
use crate::logic::store;
use crate::widgets::*;

#[derive(Default)]
pub struct AppearancePage {
    pub color_draft: Option<String>,
    pub font_size_draft: Option<u32>,
    pub library_open: bool,
    pub palette_open: bool,
    pub ui_custom: Option<bool>,
    pub mono_custom: Option<bool>,
}

fn page_state(app: &mut App) -> &mut AppearancePage {
    app.local::<AppearancePage>()
}

pub fn unmount(app: &mut App) {
    *page_state(app) = AppearancePage::default();
    background::unmount(app);
    local_model::unmount(app);
}

fn set(app: &mut App, f: impl FnOnce(&mut AppearancePreferences)) {
    store::change(app, move |doc| f(&mut doc.global_settings.appearance));
}

/// `nextNonCollidingShortcut`.
pub fn next_non_colliding_shortcut(binding: &[String]) -> Vec<String> {
    composer_shortcut_choices().into_iter().find(|c| !bindings_conflict(c, binding)).unwrap_or_else(|| vec!["Enter".to_owned()])
}

pub fn set_send_shortcut(app: &mut App, send: Vec<String>) {
    set(app, move |a| {
        if bindings_conflict(&send, &a.newline_shortcut) {
            a.newline_shortcut = next_non_colliding_shortcut(&send);
        }
        a.send_shortcut = send;
    });
}

pub fn set_newline_shortcut(app: &mut App, candidate: Vec<String>) {
    set(app, move |a| {
        a.newline_shortcut = if bindings_conflict(&candidate, &a.send_shortcut) { next_non_colliding_shortcut(&a.send_shortcut) } else { candidate };
    });
}

pub fn set_theme(app: &mut App, theme: ThemePreference) {
    store::change(app, move |doc| doc.global_settings.theme = theme);
}

pub fn set_language(app: &mut App, language: AppLanguage) {
    store::change(app, move |doc| doc.global_settings.app_language = language);
    // The host cannot resolve `auto`, so what it resolves to rides along.
    let chinese = app.language_is_chinese();
    store::change(app, move |doc| doc.global_settings.resolved_app_language = if chinese { mewrk_client::model::ResolvedLanguage::ZhCn } else { mewrk_client::model::ResolvedLanguage::EnUs });
}

/// A box placed by fractions of its parent, as the sample's stylesheet places everything.
fn part(left: f32, top: f32, width: f32, height: f32, color: Color) -> E {
    div().absolute().left_pct(left).bottom_pct(1.0 - top - height).w_pct(width * 100.0).h_pct(height * 100.0).bg(color)
}

/// A miniature of the window, in one scheme's colours: sidebar, top bar, the conversation tile with its composer, and a side pane. `picture`
/// lies behind it where its ground shows, in place of the scheme's solid one; with `glass`, the sidebar and tiles let it through.
fn theme_sample(tokens: FluentTheme, picture: Option<Picture>, glass: Option<u32>) -> E {
    let t = tokens;
    let thin = |c: Color, alpha: f32| glass.map_or(c, |transparency| c.with_alpha(glass_tint_alpha(alpha, transparency)));
    let (tile_alpha, pane_alpha) = if t.dark { (0.84, 0.86) } else { (0.78, 0.8) };
    let bar = |left: f32, top: f32, width: f32, color: Color| part(left, top, width, 0.04, color).radius(1.5);
    let sidebar = part(0.0, 0.0, 0.24, 1.0, thin(t.layer, tile_alpha))
        .border_right(t.card_stroke)
        .child(bar(0.12, 0.15, 0.46, t.text_primary.with_alpha(0.72)))
        .child(part(0.06, 0.365, 0.88, 0.08, t.subtle_hover).radius(2.0))
        .child(bar(0.12, 0.28, 0.62, t.text_tertiary))
        .child(bar(0.12, 0.385, 0.54, t.text_tertiary))
        .child(bar(0.12, 0.49, 0.68, t.text_tertiary))
        .child(bar(0.12, 0.595, 0.44, t.text_tertiary));
    let topbar = part(0.24, 0.0, 0.76, 0.12, Color::TRANSPARENT).child(part(0.05, 0.33, 0.2, 0.34, t.text_primary.with_alpha(0.72)).radius(1.5)).child(part(0.86, 0.33, 0.1, 0.34, t.text_tertiary.with_alpha(0.6)).radius(1.5));
    let composer = part(0.07, 0.74, 0.86, 0.19, thin(t.control_fill, 0.46)).radius(4.0).border(t.control_stroke).child(div().absolute().right(3.0).top(2.0).size(6.0, 6.0).radius(3.0).bg(t.accent));
    let chat = div()
        .flex_grow(1.85)
        .basis(0.0)
        .min_w(0.0)
        .radius(3.0)
        .bg(thin(t.mica_base, tile_alpha))
        .child(part(0.55, 0.09, 0.38, 0.11, t.subtle_hover).radius(3.0))
        .child(bar(0.07, 0.29, 0.74, t.text_tertiary))
        .child(bar(0.07, 0.38, 0.82, t.text_tertiary))
        .child(bar(0.07, 0.47, 0.48, t.text_tertiary))
        .child(composer);
    let task = |top: f32| part(0.1, top, 0.8, 0.11, t.subtle_hover).radius(2.0);
    let pane = div().flex_grow(1.0).basis(0.0).min_w(0.0).radius(3.0).bg(thin(t.layer, pane_alpha)).border(t.card_stroke).child(bar(0.1, 0.08, 0.42, t.text_secondary.with_alpha(0.8))).child(task(0.21).child(div().absolute().left(0.0).top(0.0).h_full().w(2.0).bg(t.accent))).child(task(0.36)).child(task(0.51));
    let tiles = row().absolute().left_pct(0.24).bottom_pct(0.0).w_pct(76.0).h_pct(88.0).p(2.0).gap(3.0).when(picture.is_none() && glass.is_none(), |e| e.bg(t.card_secondary)).child(chat).child(pane);
    div()
        .w_full()
        .h_full()
        .bg(t.mica_base)
        .clip()
        .pass_through()
        .child(picture.map(|p| custom::<App>(move |_, ui, r| ui.image_cover(p.id, r, p.size, 0.0, 1.0)).absolute().inset(0.0).pass_through()))
        .child(sidebar)
        .child(topbar)
        .child(tiles)
}

fn theme_card(app: &mut App, value: ThemePreference, label: &str, picture: Option<Picture>, glass: Option<u32>, selected: bool, stacked: bool) -> E {
    let t_ = theme();
    let accent = normalize_hex_color(&app.doc.global_settings.appearance.theme_color).filter(|c| !c.is_empty());
    let tokens = |dark: bool| {
        let mut base = if dark { FluentTheme::dark() } else { FluentTheme::light() };
        if let Some(c) = &accent {
            base.accent = hex(c);
        }
        base
    };
    let frame = div().w_full().aspect(16.0 / 9.0).radius(6.0).clip().border(t_.card_stroke).pass_through();
    let surface = match value {
        ThemePreference::Day => frame.child(theme_sample(tokens(false), picture, glass)),
        ThemePreference::Night => frame.child(theme_sample(tokens(true), picture, glass)),
        // Follow system: one window, the scheme on screen on its left half and the other one on its right.
        ThemePreference::System => {
            let dark_first = resolved_scheme(app) == BackgroundScheme::Night;
            let left = div().absolute().left(0.0).top(0.0).h_full().w_pct(50.0).clip().pass_through().child(theme_sample(tokens(dark_first), picture, glass).w_pct(200.0));
            let right = div().absolute().left_pct(0.5).top(0.0).h_full().w_pct(50.0).clip().pass_through().child(div().absolute().left_pct(-1.0).top(0.0).h_full().w_pct(200.0).pass_through().child(theme_sample(tokens(!dark_first), picture, glass)));
            frame.child(left).child(right).child(div().absolute().left_pct(0.5).top(0.0).h_full().w(1.0).bg(t_.control_stroke))
        }
    };
    col()
        .key(("theme-card", value as u8))
        .when(!stacked, |e| e.flex1())
        .min_w(0.0)
        .pt(6.0)
        .px(6.0)
        .pb(7.0)
        .gap(6.0)
        .radius(OVERLAY_RADIUS)
        .border(if selected { t_.accent } else { t_.card_stroke })
        .hover_bg(t_.subtle_hover)
        .aria_pressed(selected)
        .on_click(move |app: &mut App| set_theme(app, value))
        .child(surface)
        .child(text(label.to_owned()).ty(if selected { CAPTION_STRONG } else { CAPTION }).text_center())
}

/// A native colour input has no equivalent here, so "any colour" is a grid of hues over the same hex field upstream has.
fn colour_palette() -> Vec<String> {
    let mut out = Vec::new();
    for lightness in [0.35f32, 0.5, 0.65] {
        for hue in (0..12).map(|i| i as f32 * 30.0) {
            let c = (1.0 - (2.0 * lightness - 1.0).abs()) * 0.75;
            let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
            let m = lightness - c / 2.0;
            let (r, g, b) = match (hue / 60.0) as u32 {
                0 => (c, x, 0.0),
                1 => (x, c, 0.0),
                2 => (0.0, c, x),
                3 => (0.0, x, c),
                4 => (x, 0.0, c),
                _ => (c, 0.0, x),
            };
            let byte = |v: f32| ((v + m) * 255.0).round() as u8;
            out.push(format!("#{:02X}{:02X}{:02X}", byte(r), byte(g), byte(b)));
        }
    }
    out
}

fn accent_row(app: &mut App) -> E {
    let t_ = theme();
    let stored = app.doc.global_settings.appearance.theme_color.clone();
    let normalized = normalize_hex_color(&stored).unwrap_or_default();
    let p = page_state(app);
    let draft = p.color_draft.clone().unwrap_or_else(|| stored.clone());
    let palette_open = p.palette_open;
    let choose = |app: &mut App, value: String| {
        page_state(app).color_draft = None;
        set(app, move |a| a.theme_color = value);
    };
    let mut controls = row().gap(8.0).items_center().wrap().child(row().key("accent-default").center().size(30.0, 30.0).radius(15.0).bg(t_.subtle_hover).border(if stored.is_empty() { t_.accent } else { t_.control_stroke }).tooltip(t("默认", "Default")).aria_label(t("默认主色", "Default accent color")).aria_pressed(stored.is_empty()).hover_bg(t_.subtle_hover).on_click(move |app: &mut App| choose(app, String::new())).child(icon("RotateCcw", 13.0).color(t_.text_secondary)));
    for &color in THEME_COLOR_PRESETS.iter() {
        let selected = normalized == color;
        controls = controls.child(row().key(("accent", color)).size(30.0, 30.0).radius(15.0).bg(hex(color)).border(if selected { t_.text_primary } else { t_.control_stroke }).tooltip(color).when(a11y_active(), |e| e.aria_label(tf("主色 {color}", "Accent color {color}", &[("color", &color)]))).aria_pressed(selected).on_click(move |app: &mut App| choose(app, color.to_owned())));
    }
    let toggle_palette = row().key("accent-picker").center().size(30.0, 30.0).radius(15.0).bg(t_.subtle_hover).border(t_.control_stroke).hover_bg(t_.subtle_hover).tooltip(t("选择主色", "Choose accent color")).aria_haspopup(HasPopup::Dialog).aria_expanded(palette_open).on_click(|app: &mut App| {
        let p = page_state(app);
        p.palette_open = !p.palette_open;
    }).child(icon("Palette", 13.0).color(t_.text_secondary));
    let toggle_palette = if palette_open {
        let grid = colour_palette();
        let mut rows = col().p(8.0).gap(4.0);
        for chunk in grid.chunks(12) {
            let mut line = row().gap(4.0);
            for color in chunk {
                let c = color.clone();
                line = line.child(div().key(("accent-swatch", color.as_str())).size(18.0, 18.0).radius(3.0).bg(hex(color)).tooltip(color.clone()).when(a11y_active(), |e| e.aria_label(tf("主色 {color}", "Accent color {color}", &[("color", color)]))).aria_pressed(normalized == *color).on_click(move |app: &mut App| {
                    page_state(app).palette_open = false;
                    choose(app, c.clone());
                }));
            }
            rows = rows.child(line);
        }
        toggle_palette.child(flyout_panel::<App>().child(rows).key("accent-palette").role(Role::Group).aria_label(t("选择主色", "Choose accent color")).popover("accent-picker", Side::Below, Align::Start, 4.0, |app: &mut App| page_state(app).palette_open = false))
    } else {
        toggle_palette
    };
    controls = controls.child(toggle_palette);
    let hex_input = field::<App>("accent-hex", draft.to_uppercase())
        .ty(MONO_BODY)
        .placeholder(t("默认", "Default"))
        .on_change(|app, v| page_state(app).color_draft = Some(v))
        .on_blur(|app, _| {
            let draft = page_state(app).color_draft.take();
            let Some(draft) = draft else { return };
            // What doesn't parse goes back to what's stored instead of being saved.
            if let Some(next) = normalize_hex_color(&draft)
                && next != app.doc.global_settings.appearance.theme_color {
                    set(app, move |a| a.theme_color = next);
                }
        })
        .build()
        .w(106.0)
        .aria_label(t("十六进制主色", "Hex accent color"));
    controls.child(hex_input)
}

/// The review pane's sample in miniature: a few lines with one rewritten, so syntax colours, the bands for a removed and an
/// added line and the mark for what changed are all in view at once. Every sample is the same five lines tall, and a line too
/// long for the block is cut short.
fn code_theme_preview(theme_data: &CodeTheme) -> E {
    let p = palette_of(theme_data);
    let (line, word) = match theme_data.scheme {
        CodeThemeScheme::Day => (0.13, 0.28),
        CodeThemeScheme::Night => (0.17, 0.36),
    };
    let fg = p.foreground;
    type Run = (&'static str, Color, bool);
    let lines: [(&str, Option<bool>, Vec<Run>); 5] = [
        ("1", None, vec![("// Greets a visitor by name.", p.comment, false)]),
        ("2", None, vec![("function ", p.keyword, false), ("greet", p.function, false), ("(name: ", fg, false), ("string", p.type_, false), (", times = ", fg, false), ("1", p.number, false), (") {", fg, false)]),
        ("3", Some(false), vec![("  return ", p.keyword, false), ("\"", p.string, true), ("Hello, ", p.string, false), ("\"", p.string, true), (" + ", fg, true), ("name;", fg, false)]),
        ("3", Some(true), vec![("  return ", p.keyword, false), ("`", p.string, true), ("Hello, ", p.string, false), ("${", p.string, true), ("name", p.string, false), ("}!`", p.string, true), (".repeat(times)", fg, true), (";", fg, false)]),
        ("4", None, vec![("}", fg, false)]),
    ];
    let mut block = col().py(8.0).radius(8.0).border(fg.with_alpha(0.14)).bg(p.background).clip().pass_through().aria_hidden(true);
    for (number, added, runs) in lines {
        let hue = added.map(|a| if a { p.added } else { p.removed });
        let sign = added.map_or(" ", |a| if a { "+" } else { "-" });
        // Layout trims the spaces a line ends in, and a run's are part of the sample: a word joiner after them keeps them inside the line.
        let body = row().flex1().min_w(0.0).clip().child(text(sign).ty(MONO_SMALL).color(hue.unwrap_or(fg)).w(14.0).nowrap()).children(runs.into_iter().map(|(run, color, marked)| text(format!("{run}\u{2060}")).ty(MONO_SMALL).color(color).nowrap().shrink0().when(marked, |e| e.radius(3.0).bg(hue.unwrap_or(fg).with_alpha(word)))));
        block = block.child(row().h(19.0).items_center().map(|e| match hue {
            Some(h) => e.bg(h.with_alpha(line)),
            None => e,
        }).child(row().pl(14.0).pr(10.0).shrink0().child(text(number).ty(MONO_SMALL).color(hue.unwrap_or(p.line_number)).nowrap())).child(body));
    }
    block
}

fn code_theme_column(scheme: CodeThemeScheme, stored: &str, label: &str) -> E {
    let themes = code_themes_for(scheme);
    let selected = code_theme_for(stored, scheme);
    let index = themes.iter().position(|t| t.id == selected.id).unwrap_or(0);
    let names: Vec<Arc<str>> = themes.iter().map(|t| Arc::from(t.name)).collect();
    let ids: Vec<&'static str> = themes.iter().map(|t| t.id).collect();
    col().flex1().min_w(280.0).gap(8.0).child(select::<App>(("code-theme", scheme as u8), names, index, move |app, i| {
        let id = ids[i].to_owned();
        set(app, move |a| match scheme {
            CodeThemeScheme::Day => a.code_theme_day = id,
            CodeThemeScheme::Night => a.code_theme_night = id,
        });
    }).aria_label(label)).child(code_theme_preview(selected))
}

fn font_combo(key: &'static str, stored: &str, presets: &'static [&'static str], custom_mode: Option<bool>, is_custom: fn(&mut AppearancePage, Option<bool>), write: fn(&mut AppearancePreferences, String), label: &str) -> E {
    let known = presets.contains(&stored);
    // Choosing Custom only opens the input; the default font is not saved until it changes.
    let in_custom = custom_mode.unwrap_or(!known);
    let mut options: Vec<Arc<str>> = presets.iter().map(|f| Arc::from(if f.is_empty() { t("默认", "Default") } else { *f })).collect();
    options.push(Arc::from(t("自定义…", "Custom…")));
    let custom_index = presets.len();
    let selected = if in_custom { custom_index } else { presets.iter().position(|f| *f == stored).unwrap_or(0) };
    // Each option in its own family, `<option style={{ fontFamily }}>`.
    let fonts = presets.iter().map(|f| (!f.is_empty()).then(|| Arc::from(*f))).collect();
    let combo = select_in_fonts::<App>(("font", key), options, fonts, selected, move |app, i| {
        if i == custom_index {
            is_custom(page_state(app), Some(true));
            return;
        }
        is_custom(page_state(app), None);
        let value = presets[i].to_owned();
        set(app, move |a| write(a, value));
    })
    .aria_label(label);
    let custom_input = in_custom.then(|| {
        input(("font-custom", key), if known { String::new() } else { stored.to_owned() }, t("输入字体族", "Enter a font family"), move |app, v| {
            // What was typed decides from here on: a value that is a preset, an empty one included, goes back to the list.
            is_custom(page_state(app), None);
            set(app, move |a| write(a, v));
        })
        .min_w(150.0)
        .aria_label(tf("自定义{name}", "Custom {name}", &[("name", &label)]))
        .tooltip(tf("自定义{name}", "Custom {name}", &[("name", &label)]))
    });
    let combo = col().min_w(140.0).child(combo);
    // `max-width: 760px` stacks the two; otherwise the select alone keeps the second column, under where the input would be.
    if window_size().0 <= 760.0 {
        return col().w_full().gap(7.0).child(combo).child(custom_input);
    }
    match custom_input {
        Some(custom) => row().w(390.0).gap(7.0).child(combo.flex1()).child(col().flex1().child(custom)),
        None => row().w(390.0).gap(7.0).child(div().flex1().min_w(140.0)).child(combo.flex1()),
    }
}

fn shortcut_select(key: &'static str, label: &str, value: &[String], choices: Vec<Vec<String>>, on_change: impl Fn(&mut App, Vec<String>) + 'static) -> E {
    let names: Vec<Arc<str>> = choices.iter().map(|c| Arc::from(shortcut_key_caps(c, None).join(" + "))).collect();
    let selected = choices.iter().position(|c| c.join("+") == value.join("+")).unwrap_or(0);
    let on_change = StdArc::new(on_change);
    row().gap(9.0).items_center().justify_end().child(select::<App>(("composer-shortcut", key), names, selected, move |app, i| on_change(app, choices[i].clone())).w(142.0).aria_label(label)).child(row().gap(4.0).aria_hidden(true).children(shortcut_key_caps(value, None).into_iter().map(|cap| keycap(cap).min_w(24.0).h(22.0).px(6.0))))
}

/// A horizontal slider. A range input commits on release, as persisting every pixel of a drag would be wasteful.
/// How wide a glass slider's track is. Its number stands beside it, so that the row reads the same at every value.
const GLASS_SLIDER_WIDTH: f32 = 180.0;

/// A slider over whole numbers from zero. It applies as it moves: what it changes is the window around the dialog.
fn glass_slider(key: &'static str, label: &'static str, value: u32, max: u32, unit: &'static str, disabled: Option<&'static str>, write: fn(&mut AppearancePreferences, u32)) -> E {
    let t_ = theme();
    let value = value.min(max);
    let fraction = value as f32 / max as f32;
    let from_x = move |e: &PointerEvent| (((e.x - e.rect.x) / e.rect.w.max(1.0)).clamp(0.0, 1.0) * max as f32).round() as u32;
    let track = row()
        .key(key)
        .h(22.0)
        .w(GLASS_SLIDER_WIDTH)
        .items_center()
        .cursor(CursorKind::Hand)
        .aria_label(label)
        .aria_value(f64::from(value), 0.0, f64::from(max))
        .child(div().h(4.0).flex1().radius(2.0).bg(t_.subtle_pressed).child(div().h_full().w_pct(fraction * 100.0).radius(2.0).bg(t_.accent)))
        .child(div().absolute().left(0.0).right(0.0).h(22.0).top(0.0).pass_through().child(div().absolute().left_pct(fraction).size(14.0, 14.0).top(4.0).ml(-7.0).radius(7.0).bg(t_.accent)));
    let number = text(format!("{value}{unit}")).ty(MONO_SMALL).color(t_.text_secondary).w(48.0).text_center().nowrap();
    let slider = row().gap(10.0).items_center();
    if disabled.is_some() {
        return slider.child(track).child(number).opacity(0.5).disabled(true).maybe_disabled_reason(disabled);
    }
    let track = track
        .on_pointer(move |app: &mut App, e: PointerEvent| {
            let next = from_x(&e);
            if next != value {
                set(app, move |a| write(a, next));
            }
        })
        .on_key(move |app: &mut App, k: &KeyPress| {
            let delta: i64 = match k.key {
                Key::Left | Key::Down => -1,
                Key::Right | Key::Up => 1,
                _ => return false,
            };
            let next = (i64::from(value) + delta).clamp(0, i64::from(max)) as u32;
            set(app, move |a| write(a, next));
            true
        });
    slider.child(track).child(number)
}

fn font_size_slider(value: u32, committed: u32) -> E {
    let t_ = theme();
    let span = (MAX_MESSAGE_FONT_SIZE - MIN_MESSAGE_FONT_SIZE) as f32;
    let fraction = (value - MIN_MESSAGE_FONT_SIZE) as f32 / span;
    let from_x = move |e: &PointerEvent| clamp_message_font_size((MIN_MESSAGE_FONT_SIZE as f32 + ((e.x - e.rect.x) / e.rect.w.max(1.0)).clamp(0.0, 1.0) * span) as f64);
    let track = row()
        .key("font-size-slider")
        .h(22.0)
        .w_full()
        .items_center()
        .cursor(CursorKind::Hand)
        .aria_label(t("消息字号", "Message font size"))
        .aria_value(f64::from(value), f64::from(MIN_MESSAGE_FONT_SIZE), f64::from(MAX_MESSAGE_FONT_SIZE))
        .on_pointer(move |app: &mut App, e: PointerEvent| {
            let next = from_x(&e);
            page_state(app).font_size_draft = Some(next);
            repaint();
            if e.phase == Phase::Up {
                page_state(app).font_size_draft = None;
                if next != committed {
                    set(app, move |a| a.message_font_size = next);
                }
            }
        })
        .on_key(move |app: &mut App, k: &KeyPress| {
            let delta: i64 = match k.key {
                Key::Left | Key::Down => -1,
                Key::Right | Key::Up => 1,
                _ => return false,
            };
            let next = clamp_message_font_size((committed as i64 + delta) as f64);
            set(app, move |a| a.message_font_size = next);
            true
        })
        .child(div().h(4.0).flex1().radius(2.0).bg(t_.subtle_pressed).child(div().h_full().w_pct(fraction * 100.0).radius(2.0).bg(t_.accent)))
        .child(div().absolute().left(0.0).right(0.0).h(22.0).top(0.0).pass_through().child(div().absolute().left_pct(fraction).size(14.0, 14.0).top(4.0).ml(-7.0).radius(7.0).bg(t_.accent)));
    let ticks = row().justify_between().aria_hidden(true).child(text("A").ty(FINE).color(t_.text_tertiary).nowrap()).child(text(t("默认", "Default")).ty(FINE).color(t_.text_tertiary).nowrap()).child(text("A").ty(BODY_SMALL).color(t_.text_tertiary).nowrap());
    col().gap(3.0).w_full().child(track).child(ticks)
}

/// The page.
pub fn appearance_settings(app: &mut App) -> E {
    let appearance = app.doc.global_settings.appearance.clone();
    let theme_pref = app.doc.global_settings.theme;
    let language = app.doc.global_settings.app_language;
    // The theme cards draw the window as it would look: a solid ground is each theme's own.
    let picture = if matches!(parse_background(&appearance.background), Background::Solid(_)) { None } else { background_picture(app, &appearance.background) };
    let glass = appearance.liquid_glass.then_some(appearance.glass_transparency);
    let has_picture = !matches!(parse_background(&appearance.background), Background::Solid(_));
    let p = page_state(app);
    let library_open = p.library_open;
    let (ui_custom, mono_custom, font_draft) = (p.ui_custom, p.mono_custom, p.font_size_draft);

    let toggle = |key: &'static str, label: &'static str, on: bool, write: fn(&mut AppearancePreferences, bool)| labeled_switch(("appearance", key), on, false, move |app| set(app, move |a| write(a, !on))).aria_label(label);

    // Three across, or one above another at `max-width: 760px`.
    let stacked = window_size().0 <= 760.0;
    let themes = if stacked { col() } else { row() }
        .gap(9.0)
        .w_full()
        .child(theme_card(app, ThemePreference::Day, t("浅色", "Light"), picture, glass, theme_pref == ThemePreference::Day, stacked))
        .child(theme_card(app, ThemePreference::Night, t("深色", "Dark"), picture, glass, theme_pref == ThemePreference::Night, stacked))
        .child(theme_card(app, ThemePreference::System, t("跟随系统", "Follow system"), picture, glass, theme_pref == ThemePreference::System, stacked));
    let preview = background_preview(app, &appearance.background);
    let background_row = row().gap(10.0).items_center().child(small_button(t("选择背景…", "Choose background…")).key("background-open").aria_haspopup(HasPopup::Dialog).on_click(|app: &mut App| page_state(app).library_open = true)).child(preview);
    let accent = accent_row(app);
    let theme_card_el = card(None)
        .role(Role::Region)
        .aria_label(t("主题", "Theme"))
        .child(first_row(setting_row(t("主题", "Theme"), None, themes, true)))
        .child(setting_row(t("液态玻璃", "Liquid glass"), Some(help(t("侧栏和面板变成半透明玻璃。", "Translucent glass sidebar and panes."))), toggle("glass", t("液态玻璃", "Liquid glass"), appearance.liquid_glass, |a, v| a.liquid_glass = v), false))
        .child(appearance.liquid_glass.then(|| setting_row(t("透明度", "Transparency"), None, glass_slider("glass-transparency", t("透明度", "Transparency"), appearance.glass_transparency, MAX_GLASS_TRANSPARENCY, "%", None, |a, v| a.glass_transparency = v), false)))
        .child(appearance.liquid_glass.then(|| setting_row(t("模糊度", "Blur"), None, glass_slider("glass-blur", t("模糊度", "Blur"), appearance.glass_blur, MAX_GLASS_BLUR, " px", (!has_picture).then(|| t("纯色背景没有可模糊的图片", "A solid background has no picture to blur")), |a, v| a.glass_blur = v), false)))
        .child(setting_row(t("自定义背景", "Custom background"), Some(help(t("纯色随主题切换，图片不变。", "A solid color follows the theme; a picture stays."))), background_row, false))
        .child(setting_row(t("主色", "Accent color"), None, accent, true));

    let code_row = row().gap(12.0).wrap().w_full().child(code_theme_column(CodeThemeScheme::Day, &appearance.code_theme_day, t("浅色主题下的代码外观", "Code appearance in the light theme"))).child(code_theme_column(CodeThemeScheme::Night, &appearance.code_theme_night, t("深色主题下的代码外观", "Code appearance in the dark theme")));
    let code_card = card(None).role(Role::Region).aria_label(t("代码外观", "Code appearance")).child(first_row(setting_row(t("代码外观", "Code appearance"), Some(help(t("代码与差异的配色：左浅色，右深色。", "Code and diff colors: light on the left, dark on the right."))), code_row, true)));

    let automatic = if mewrk_client::i18n::system_prefers_chinese() { t("简体中文", "Simplified Chinese") } else { t("英语", "English") };
    let languages = [AppLanguage::Auto, AppLanguage::ZhCn, AppLanguage::EnUs];
    let language_select = select::<App>("app-language", vec![Arc::from(tf("自动（{resolved}）", "Automatic ({resolved})", &[("resolved", &automatic)])), Arc::from(t("简体中文", "Simplified Chinese")), Arc::from(t("英语", "English"))], languages.iter().position(|l| *l == language).unwrap_or(0), move |app, i| set_language(app, languages[i])).w(200.0).aria_label(t("应用语言", "Application language"));
    let zoom = appearance.zoom;
    let zoom_controls = row()
        .gap(3.0)
        .items_center()
        .child((zoom != 1.0).then(|| icon_button::<App>("RotateCcw", t("重置页面缩放", "Reset page zoom")).key("zoom-reset").on_click(|app: &mut App| set(app, |a| a.zoom = 1.0))))
        .child(icon_button::<App>("Minus", t("缩小", "Zoom out")).key("zoom-out").disabled(zoom <= MIN_ZOOM).on_click(move |app: &mut App| set(app, move |a| a.zoom = clamp_zoom(zoom - ZOOM_STEP))))
        .child(text(format!("{}%", (zoom * 100.0).round() as i64)).key("zoom-value").ty(MONO_SMALL).color(theme().text_secondary).w(52.0).text_center().nowrap())
        .child(icon_button::<App>("Plus", t("放大", "Zoom in")).key("zoom-in").disabled(zoom >= MAX_ZOOM).on_click(move |app: &mut App| set(app, move |a| a.zoom = clamp_zoom(zoom + ZOOM_STEP))));
    let display_card = card(Some(t("显示与语言", "Display and language")))
        .child(first_row(setting_row(t("应用语言", "Application language"), None, language_select, false)))
        .child(setting_row(t("页面缩放", "Page zoom"), None, zoom_controls, false))
        .child(setting_row(t("宽屏消息布局", "Wide message layout"), None, toggle("wide", t("宽屏消息布局", "Wide message layout"), appearance.wide_messages, |a, v| a.wide_messages = v), false));

    let fonts_card = card(Some(t("字体", "Fonts")))
        .child(first_row(setting_row(t("界面字体", "Interface font"), None, font_combo("ui", &appearance.ui_font_family, &UI_FONT_PRESETS, ui_custom, |p, v| p.ui_custom = v, |a, v| a.ui_font_family = v, t("界面字体", "Interface font")), false)))
        .child(setting_row(t("等宽字体", "Monospace font"), None, font_combo("mono", &appearance.mono_font_family, &MONO_FONT_PRESETS, mono_custom, |p, v| p.mono_custom = v, |a, v| a.mono_font_family = v, t("等宽字体", "Monospace font")), false))
        .child(setting_row(t("消息字号", "Message font size"), None, font_size_slider(font_draft.unwrap_or(clamp_message_font_size(f64::from(appearance.message_font_size))), appearance.message_font_size), true))
        .child(setting_row(t("使用衬线字体", "Use serif font"), None, toggle("serif", t("使用衬线字体", "Use serif font"), appearance.serif_messages, |a, v| a.serif_messages = v), false));

    let newline_choices: Vec<Vec<String>> = composer_shortcut_choices().into_iter().filter(|c| !bindings_conflict(c, &appearance.send_shortcut)).collect();
    let input_card = card(Some(t("输入", "Input")))
        .child(first_row(setting_row(t("发送快捷键", "Send shortcut"), None, shortcut_select("send", t("发送快捷键", "Send shortcut"), &appearance.send_shortcut, composer_shortcut_choices().to_vec(), set_send_shortcut), false)))
        .child(setting_row(t("换行快捷键", "Newline shortcut"), None, shortcut_select("newline", t("换行快捷键", "Newline shortcut"), &appearance.newline_shortcut, newline_choices, set_newline_shortcut), false))
        .child(setting_row(t("拼写检查", "Spell check"), None, toggle("spell", t("拼写检查", "Spell check"), appearance.spell_check, |a, v| a.spell_check = v), false))
        .child(setting_row(t("用 Markdown 渲染用户消息", "Render user messages with Markdown"), None, toggle("user-md", t("用 Markdown 渲染用户消息", "Render user messages with Markdown"), appearance.render_user_markdown, |a, v| a.render_user_markdown = v), false))
        .child(setting_row(t("删除消息前确认", "Confirm before deleting messages"), None, toggle("confirm-delete", t("删除消息前确认", "Confirm before deleting messages"), appearance.confirm_message_delete, |a, v| a.confirm_message_delete = v), false));

    let messages_card = card(Some(t("消息", "Messages")))
        .child(first_row(setting_row(t("折叠思维链", "Collapse reasoning"), None, toggle("collapse-reasoning", t("折叠思维链", "Collapse reasoning"), appearance.collapse_reasoning, |a, v| a.collapse_reasoning = v), false)))
        .child(setting_row(t("代码块可折叠", "Collapsible code blocks"), None, toggle("code-collapse", t("代码块可折叠", "Collapsible code blocks"), appearance.code_block_collapsible, |a, v| a.code_block_collapsible = v), false))
        .child(setting_row(t("代码块自动换行", "Wrap code blocks"), None, toggle("code-wrap", t("代码块自动换行", "Wrap code blocks"), appearance.code_block_wrappable, |a, v| a.code_block_wrappable = v), false));

    let local_model_card = local_model_settings(app);
    let math_card = card(Some(t("数学", "Math"))).child(first_row(setting_row(t("启用 $...$ 行内公式", "Enable $...$ inline math"), Some(help(t("关闭后只渲染 $$…$$ 公式。", "When off, only $$…$$ formulas are rendered."))), toggle("math", t("启用 $...$ 行内公式", "Enable $...$ inline math"), appearance.single_dollar_math, |a, v| a.single_dollar_math = v), false)));
    let library = library_open.then(|| background_library_dialog(app, &appearance.background, |app| page_state(app).library_open = false));
    page()
        .child(page_heading(Some(page_title(t("外观", "Appearance"))), page_description(t("主题、字体与消息渲染。", "Theme, fonts, and message rendering.")), None))
        .child(theme_card_el)
        .child(library)
        .child(code_card)
        .child(display_card)
        .child(fonts_card)
        .child(input_card)
        .child(messages_card)
        .child(local_model_card)
        .child(math_card)
}

#[cfg(test)]
mod tests {
    use super::*;

    // AppearanceSettings.test.tsx
    #[test]
    fn a_colliding_newline_moves_to_the_next_free_choice() {
        let enter = vec!["Enter".to_owned()];
        assert_eq!(next_non_colliding_shortcut(&enter), vec!["Shift".to_owned(), "Enter".to_owned()]);
        let shift_enter = vec!["Shift".to_owned(), "Enter".to_owned()];
        assert_eq!(next_non_colliding_shortcut(&shift_enter), enter);
    }

    #[test]
    fn the_palette_is_valid_hex_that_normalizes_to_itself() {
        let grid = colour_palette();
        assert_eq!(grid.len(), 36);
        for c in grid {
            assert_eq!(normalize_hex_color(&c).as_deref(), Some(c.as_str()));
        }
    }
}
