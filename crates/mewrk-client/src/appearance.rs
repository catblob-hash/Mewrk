//! Appearance data (lib/appearance.ts): defaults, normalisation, clamps, and what upstream's `applyAppearance` worked
//! out for the DOM, as [`DerivedAppearance`]. Also the parts of lib/background.ts and lib/codeThemes.ts that takes.

use std::collections::BTreeMap;

use crate::model::AppearancePreferences;
use crate::shortcuts::{concrete_binding, PRIMARY_MODIFIER};
use crate::App;

/// Theme color presets.
pub const THEME_COLOR_PRESETS: [&str; 5] = ["#356AE6", "#00B96B", "#EF4444", "#F59E0B", "#8B5CF6"];

/// UI font candidates. The empty first item represents the default.
pub const UI_FONT_PRESETS: [&str; 8] = ["", "Inter", "Segoe UI", "Microsoft YaHei", "Noto Sans SC", "Source Han Sans SC", "PingFang SC", "Arial"];

/// Monospace font candidates. The empty first item represents the default.
pub const MONO_FONT_PRESETS: [&str; 7] = ["", "Cascadia Code", "Cascadia Mono", "Consolas", "JetBrains Mono", "Fira Code", "Sarasa Mono SC"];

pub const MIN_MESSAGE_FONT_SIZE: u32 = 12;
pub const MAX_MESSAGE_FONT_SIZE: u32 = 22;
pub const MIN_ZOOM: f64 = 0.5;
pub const MAX_ZOOM: f64 = 2.0;
pub const ZOOM_STEP: f64 = 0.1;

/// Factory defaults for appearance preferences; the host's `AppearancePreferences::default` is upstream's seed.
pub fn default_appearance_preferences() -> AppearancePreferences {
    AppearancePreferences::default()
}

/// Available composer send and newline shortcuts, in display order: the command modifier with Enter is ⌘↩ on a Mac and
/// Ctrl+Enter elsewhere.
pub fn composer_shortcut_choices() -> [Vec<String>; 4] {
    [vec!["Enter".to_owned()], vec!["Shift".to_owned(), "Enter".to_owned()], concrete_binding(&[PRIMARY_MODIFIER, "Enter"], None), vec!["Alt".to_owned(), "Enter".to_owned()]]
}

/// Normalizes a user-entered hexadecimal color by Cherry Studio's rules: trim whitespace, add `#`, expand three digits,
/// reject anything but six digits, uppercase. Empty stays empty; `None` preserves the draft instead of persisting it.
pub fn normalize_hex_color(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Some(String::new());
    }
    let body = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let expanded = if body.len() == 3 { body.chars().flat_map(|c| [c, c]).collect() } else { body.to_owned() };
    if expanded.len() != 6 {
        return None;
    }
    Some(format!("#{}", expanded.to_ascii_uppercase()))
}

/// The zoom to apply, in range and rounded to one decimal place so floating-point accumulation preserves 100%.
pub fn clamp_zoom(value: f64) -> f64 {
    if !value.is_finite() {
        return 1.0;
    }
    (value.max(MIN_ZOOM).min(MAX_ZOOM) * 10.0).round() / 10.0
}

/// The message font size to apply, in range and whole.
pub fn clamp_message_font_size(value: f64) -> u32 {
    if !value.is_finite() {
        return 14;
    }
    (value.round().max(f64::from(MIN_MESSAGE_FONT_SIZE)).min(f64::from(MAX_MESSAGE_FONT_SIZE))) as u32
}

/// Converts a font family to a CSS `font-family`; an empty family uses the fallback stack.
fn font_family_value(family: &str, fallback: &str) -> String {
    let trimmed = family.trim();
    if trimmed.is_empty() {
        return fallback.to_owned();
    }
    // Preserve quoted names and font stacks; quote only a single bare family name.
    if trimmed.contains(',') || trimmed.contains('"') {
        return trimmed.to_owned();
    }
    format!("\"{trimmed}\", {fallback}")
}

const DEFAULT_UI_FONT_STACK: &str = "Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, \"Segoe UI\", \"Noto Sans SC\", sans-serif";
const DEFAULT_MONO_FONT_STACK: &str = "\"Cascadia Code\", \"Cascadia Mono\", \"SFMono-Regular\", Consolas, monospace";
pub const SERIF_FONT_STACK: &str = "Georgia, \"Times New Roman\", \"Noto Serif SC\", \"Source Han Serif SC\", serif";

/// What upstream's `applyAppearance` set on the document root, as values. Nothing here is applied: fonts and colours
/// stay Fluent's unless a view chooses otherwise. `custom_css` is left out on purpose: nothing reads it, and it stays in the document
/// so that one written by upstream keeps its value.
#[derive(Clone, Debug, PartialEq)]
pub struct DerivedAppearance {
    /// `--accent`, `--focus` and `--accent-custom`: the chosen color, or `None` for the theme's own.
    pub accent: Option<String>,
    /// `--accent-soft`: the chosen color at about 12% alpha, for its soft background.
    pub accent_soft: Option<String>,
    /// `--app-font-family`.
    pub app_font_family: String,
    /// `--mono`.
    pub mono_font_family: String,
    /// `--message-font-family`: the serif stack, or the same as `app_font_family`.
    pub message_font_family: String,
    /// `--message-font-size`, in pixels.
    pub message_font_size: u32,
    /// `zoom`. Upstream used it rather than `transform: scale()` so layout, hit testing and scrollbars scale too.
    pub zoom: f64,
    /// `--code-day-*`, keyed without the prefix.
    pub code_day: BTreeMap<&'static str, String>,
    /// `--code-night-*`, keyed without the prefix.
    pub code_night: BTreeMap<&'static str, String>,
    /// `data-wide-messages`.
    pub wide_messages: bool,
    /// `data-code-wrap`.
    pub code_wrap: bool,
    /// `data-code-collapse`.
    pub code_collapse: bool,
    /// `data-backdrop`: whether the ground is something other than the theme's own, in which case the surfaces that
    /// paint the ground let it through. Follows the choice, not a picture's arrival, so layout doesn't restyle twice.
    pub backdrop_custom: bool,
    /// `data-glass`: panes of glass over the background.
    pub glass: bool,
}

/// Everything `applyAppearance` derived from the preferences.
pub fn derive_appearance(appearance: &AppearancePreferences) -> DerivedAppearance {
    // An empty color and an unusable one both leave the theme's own accent.
    let accent = normalize_hex_color(&appearance.theme_color).filter(|color| !color.is_empty());
    let app_font_family = font_family_value(&appearance.ui_font_family, DEFAULT_UI_FONT_STACK);
    DerivedAppearance {
        // Eight-digit hexadecimal alpha, as upstream's soft background and focus ring derive from the accent.
        accent_soft: accent.as_ref().map(|color| format!("{color}1F")),
        accent,
        mono_font_family: font_family_value(&appearance.mono_font_family, DEFAULT_MONO_FONT_STACK),
        message_font_family: if appearance.serif_messages { SERIF_FONT_STACK.to_owned() } else { app_font_family.clone() },
        app_font_family,
        message_font_size: clamp_message_font_size(f64::from(appearance.message_font_size)),
        zoom: clamp_zoom(appearance.zoom),
        code_day: code_theme_variables(code_theme_for(&appearance.code_theme_day, CodeThemeScheme::Day), None),
        code_night: code_theme_variables(code_theme_for(&appearance.code_theme_night, CodeThemeScheme::Night), None),
        wide_messages: appearance.wide_messages,
        code_wrap: appearance.code_block_wrappable,
        code_collapse: appearance.code_block_collapsible,
        backdrop_custom: replaces_theme_ground(&appearance.background),
        glass: appearance.liquid_glass,
    }
}

impl App {
    /// `getAppearanceSnapshot`: the preferences last published, for rendering that doesn't want them threaded through.
    pub fn appearance_snapshot(&mut self) -> &AppearancePreferences {
        self.local::<AppearancePreferences>()
    }

    /// What's left of `applyAppearance` without a DOM: makes these what [`App::appearance_snapshot`] reads.
    pub fn publish_appearance(&mut self, appearance: AppearancePreferences) {
        *self.local::<AppearancePreferences>() = appearance;
    }
}

/// `BUILTIN_PICTURES`'s saved values (lib/background.ts). The pictures themselves are the background view's.
const BUILTIN_PICTURE_IDS: [&str; 4] = ["builtin:desk", "builtin:chair", "builtin:shelf", "builtin:curtain"];

/// Whether the window's ground is something other than the theme's own: a picture, or the other theme's solid. Anything
/// unknown reads as the theme's own (lib/background.ts).
pub fn replaces_theme_ground(value: &str) -> bool {
    let imported = value.len() == 64 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    value == "solid:day" || value == "solid:night" || BUILTIN_PICTURE_IDS.contains(&value) || imported
}

/// Which of the app's two themes a code theme belongs to (lib/codeThemes.ts).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodeThemeScheme {
    Day,
    Night,
}

/// The colours code and diffs are drawn in. One is chosen for each of the app's themes, from the themes of that scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeTheme {
    pub id: &'static str,
    /// A proper name, shown as written in every language.
    pub name: &'static str,
    pub scheme: CodeThemeScheme,
    /// The Mewrk themes: code keeps the app's own surface and text colour, so it sits in the window's well. The three
    /// surface colours below are then what that surface is in the plain window, for the settings preview.
    pub adaptive: bool,
    pub background: &'static str,
    pub foreground: &'static str,
    pub line_number: &'static str,
    pub comment: &'static str,
    pub comment_italic: bool,
    pub string: &'static str,
    pub number: &'static str,
    pub keyword: &'static str,
    pub function: &'static str,
    pub type_: &'static str,
    /// Decorators, preprocessor directives, a diff's file headers.
    pub meta: &'static str,
    pub tag: &'static str,
    pub attribute: &'static str,
    /// A key in data or configuration.
    pub property: &'static str,
    /// A sigilled variable: `$HOME`, `@name`.
    pub variable: &'static str,
    /// The hues an added and a removed line are marked and tinted with.
    pub added: &'static str,
    pub removed: &'static str,
}

/// Where a themed block's ground and text colour come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeThemeSurface {
    /// The theme's own.
    Theme,
    /// Left unset, so the app's surfaces show: what an adaptive theme does in the window.
    App,
    /// An adaptive theme's well as liquid glass draws it, for a preview that can't borrow the window's.
    Glass,
}

/// The theme a scheme falls back to.
pub fn default_code_theme(scheme: CodeThemeScheme) -> &'static str {
    match scheme {
        CodeThemeScheme::Day => "mewrk-light",
        CodeThemeScheme::Night => "mewrk-dark",
    }
}

/// The themes of one scheme, the app's own first.
pub fn code_themes_for(scheme: CodeThemeScheme) -> Vec<&'static CodeTheme> {
    CODE_THEMES.iter().filter(|theme| theme.scheme == scheme).collect()
}

/// The theme saved as `id` for `scheme`; an unknown id, or one of the other scheme, is the default.
pub fn code_theme_for(id: &str, scheme: CodeThemeScheme) -> &'static CodeTheme {
    let default = default_code_theme(scheme);
    CODE_THEMES
        .iter()
        .find(|candidate| candidate.id == id && candidate.scheme == scheme)
        .or_else(|| CODE_THEMES.iter().find(|candidate| candidate.id == default))
        .expect("every scheme has its default theme")
}

/// The id `id` is saved as: itself when it names a theme of `scheme`, otherwise the default.
pub fn normalize_code_theme(id: &str, scheme: CodeThemeScheme) -> &'static str {
    code_theme_for(id, scheme).id
}

/// How much of its hue a changed line is tinted with, and how much the characters that changed within it. A dark ground
/// swallows a tint a light one shows.
fn diff_tint(scheme: CodeThemeScheme) -> (f64, f64) {
    match scheme {
        CodeThemeScheme::Day => (0.13, 0.28),
        CodeThemeScheme::Night => (0.17, 0.36),
    }
}

/// A themed block's edge: its own text colour, thinned.
const BORDER_ALPHA: f64 = 0.14;
/// A code well under liquid glass is the text colour at this alpha (`--subtle`).
const GLASS_WELL_ALPHA: f64 = 0.04;

fn glass_text(scheme: CodeThemeScheme) -> &'static str {
    match scheme {
        CodeThemeScheme::Day => "#1b1b1a",
        CodeThemeScheme::Night => "#e5e5e4",
    }
}

/// `#rrggbb` at `alpha`, as eight-digit hex.
fn with_alpha(color: &str, alpha: f64) -> String {
    format!("{color}{:02x}", (alpha * 255.0).round() as u8)
}

/// Every variable a theme may set, without the `--code-` prefix, in a fixed order.
pub const CODE_THEME_VARIABLES: [&str; 22] = [
    "bg", "fg", "border", "line-number", "comment-style", "comment", "string", "number", "keyword", "function", "type", "meta", "tag", "attribute", "property", "variable",
    "added", "added-line", "added-word", "removed", "removed-line", "removed-word",
];

/// A theme as `--code-*` values, keyed without the prefix. `None` is the theme's natural surface: the app's for an
/// adaptive theme, its own otherwise.
pub fn code_theme_variables(theme: &CodeTheme, surface: Option<CodeThemeSurface>) -> BTreeMap<&'static str, String> {
    let surface = surface.unwrap_or(if theme.adaptive { CodeThemeSurface::App } else { CodeThemeSurface::Theme });
    let (line, word) = diff_tint(theme.scheme);
    let mut variables = BTreeMap::from([
        ("comment-style", if theme.comment_italic { "italic" } else { "normal" }.to_owned()),
        ("added", theme.added.to_owned()),
        ("added-line", with_alpha(theme.added, line)),
        ("added-word", with_alpha(theme.added, word)),
        ("removed", theme.removed.to_owned()),
        ("removed-line", with_alpha(theme.removed, line)),
        ("removed-word", with_alpha(theme.removed, word)),
        ("comment", theme.comment.to_owned()),
        ("string", theme.string.to_owned()),
        ("number", theme.number.to_owned()),
        ("keyword", theme.keyword.to_owned()),
        ("function", theme.function.to_owned()),
        ("type", theme.type_.to_owned()),
        ("meta", theme.meta.to_owned()),
        ("tag", theme.tag.to_owned()),
        ("attribute", theme.attribute.to_owned()),
        ("property", theme.property.to_owned()),
        ("variable", theme.variable.to_owned()),
    ]);
    if surface != CodeThemeSurface::App {
        let glass = surface == CodeThemeSurface::Glass && theme.adaptive;
        let text = glass_text(theme.scheme);
        variables.insert("bg", if glass { with_alpha(text, GLASS_WELL_ALPHA) } else { theme.background.to_owned() });
        variables.insert("fg", theme.foreground.to_owned());
        variables.insert("border", if glass { with_alpha(text, 0.11) } else { with_alpha(theme.foreground, BORDER_ALPHA) });
        variables.insert("line-number", theme.line_number.to_owned());
    }
    variables
}

/// Every theme, the app's own first, then the published ones as their authors named them.
///
/// The app's own hues are the palette's syntax colours tuned per scheme by hand rather than mirrored: a violet at mid
/// lightness is its own mirror image, and reads well on the day well and poorly on the night one. A published theme is
/// its author's palette, reduced to the colours this has a place for (lib/codeThemePalettes.ts).
pub const CODE_THEMES: &[CodeTheme] = &[
    CodeTheme {
        id: "mewrk-light", name: "Mewrk Light", scheme: CodeThemeScheme::Day, adaptive: true,
        background: "#f1f1ee", foreground: "#343431", line_number: "#989892",
        comment: "#73736d", comment_italic: true,
        string: "#246b4e", number: "#a2531d", keyword: "#6254aa", function: "#795d2e", type_: "#1f7389",
        meta: "#aa4f6a", tag: "#8e3630", attribute: "#96580f", property: "#315eae", variable: "#315eae",
        added: "#287a38", removed: "#a23737",
    },
    CodeTheme {
        id: "mewrk-dark", name: "Mewrk Dark", scheme: CodeThemeScheme::Night, adaptive: true,
        background: "#11110e", foreground: "#cececb", line_number: "#6d6d67",
        comment: "#8a8a83", comment_italic: true,
        string: "#8fd0ae", number: "#f0a070", keyword: "#a99cf0", function: "#dcc48e", type_: "#6cc4d8",
        meta: "#e58fb0", tag: "#ee8a82", attribute: "#e8b05c", property: "#86aef0", variable: "#86aef0",
        added: "#6fcf82", removed: "#f08a8a",
    },
    CodeTheme {
        id: "github-light", name: "GitHub Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#ffffff", foreground: "#1f2328", line_number: "#8c959f",
        comment: "#6e7781", comment_italic: false,
        string: "#0a3069", number: "#0550ae", keyword: "#cf222e", function: "#8250df", type_: "#953800",
        meta: "#0550ae", tag: "#116329", attribute: "#0550ae", property: "#116329", variable: "#1f2328",
        added: "#1a7f37", removed: "#cf222e",
    },
    CodeTheme {
        id: "light-plus", name: "Light+", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#ffffff", foreground: "#000000", line_number: "#237893",
        comment: "#008000", comment_italic: false,
        string: "#a31515", number: "#098658", keyword: "#0000ff", function: "#795e26", type_: "#267f99",
        meta: "#af00db", tag: "#800000", attribute: "#e50000", property: "#0451a5", variable: "#001080",
        added: "#098658", removed: "#a31515",
    },
    CodeTheme {
        id: "one-light", name: "One Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#fafafa", foreground: "#383a42", line_number: "#9d9d9f",
        comment: "#a0a1a7", comment_italic: true,
        string: "#50a14f", number: "#986801", keyword: "#a626a4", function: "#4078f2", type_: "#c18401",
        meta: "#986801", tag: "#e45649", attribute: "#986801", property: "#e45649", variable: "#e45649",
        added: "#50a14f", removed: "#e45649",
    },
    CodeTheme {
        id: "solarized-light", name: "Solarized Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#fdf6e3", foreground: "#657b83", line_number: "#93a1a1",
        comment: "#93a1a1", comment_italic: true,
        string: "#2aa198", number: "#d33682", keyword: "#859900", function: "#268bd2", type_: "#cb4b16",
        meta: "#b58900", tag: "#268bd2", attribute: "#93a1a1", property: "#859900", variable: "#268bd2",
        added: "#859900", removed: "#dc322f",
    },
    CodeTheme {
        id: "catppuccin-latte", name: "Catppuccin Latte", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#eff1f5", foreground: "#4c4f69", line_number: "#8c8fa1",
        comment: "#7c7f93", comment_italic: true,
        string: "#40a02b", number: "#fe640b", keyword: "#8839ef", function: "#1e66f5", type_: "#df8e1d",
        meta: "#fe640b", tag: "#1e66f5", attribute: "#df8e1d", property: "#1e66f5", variable: "#4c4f69",
        added: "#40a02b", removed: "#d20f39",
    },
    CodeTheme {
        id: "gruvbox-light", name: "Gruvbox Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#fbf1c7", foreground: "#3c3836", line_number: "#bdae93",
        comment: "#928374", comment_italic: true,
        string: "#79740e", number: "#8f3f71", keyword: "#9d0006", function: "#b57614", type_: "#b57614",
        meta: "#af3a03", tag: "#427b58", attribute: "#b57614", property: "#689d6a", variable: "#076678",
        added: "#79740e", removed: "#cc241d",
    },
    CodeTheme {
        id: "ayu-light", name: "Ayu Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#fcfcfc", foreground: "#5c6166", line_number: "#cbd0d7",
        comment: "#adaeb1", comment_italic: true,
        string: "#86b300", number: "#a37acc", keyword: "#fa8532", function: "#eba400", type_: "#22a4e6",
        meta: "#e59645", tag: "#55b4d4", attribute: "#eba400", property: "#55b4d4", variable: "#5c6166",
        added: "#6cbf43", removed: "#ff7383",
    },
    CodeTheme {
        id: "tokyo-night-light", name: "Tokyo Night Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#e6e7ed", foreground: "#343b59", line_number: "#9da0ab",
        comment: "#888b94", comment_italic: false,
        string: "#385f0d", number: "#965027", keyword: "#65359d", function: "#2959aa", type_: "#343b58",
        meta: "#33635c", tag: "#8c4351", attribute: "#65359d", property: "#006c86", variable: "#343b58",
        added: "#33635c", removed: "#8c4351",
    },
    CodeTheme {
        id: "rose-pine-dawn", name: "Rosé Pine Dawn", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#faf4ed", foreground: "#575279", line_number: "#797593",
        comment: "#9893a5", comment_italic: true,
        string: "#ea9d34", number: "#d7827e", keyword: "#286983", function: "#d7827e", type_: "#56949f",
        meta: "#d7827e", tag: "#56949f", attribute: "#907aa9", property: "#56949f", variable: "#575279",
        added: "#56949f", removed: "#b4637a",
    },
    CodeTheme {
        id: "everforest-light", name: "Everforest Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#fdf6e3", foreground: "#5c6a72", line_number: "#c5c8b8",
        comment: "#939f91", comment_italic: true,
        string: "#dfa000", number: "#df69ba", keyword: "#f85552", function: "#8da101", type_: "#3a94c5",
        meta: "#35a77c", tag: "#f57d26", attribute: "#dfa000", property: "#f57d26", variable: "#5c6a72",
        added: "#8da101", removed: "#f85552",
    },
    CodeTheme {
        id: "night-owl-light", name: "Night Owl Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#fbfbfb", foreground: "#403f53", line_number: "#90a7b2",
        comment: "#939dbb", comment_italic: false,
        string: "#c96765", number: "#aa0982", keyword: "#994cc3", function: "#4876d6", type_: "#111111",
        meta: "#4876d6", tag: "#994cc3", attribute: "#4876d6", property: "#0c969b", variable: "#4876d6",
        added: "#08916a", removed: "#de3d3b",
    },
    CodeTheme {
        id: "vitesse-light", name: "Vitesse Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#ffffff", foreground: "#393a34", line_number: "#c1c1bf",
        comment: "#a0ada0", comment_italic: false,
        string: "#b56959", number: "#2f798a", keyword: "#1e754f", function: "#59873a", type_: "#2e8f82",
        meta: "#998418", tag: "#1e754f", attribute: "#b07d48", property: "#998418", variable: "#b07d48",
        added: "#22863a", removed: "#b31d28",
    },
    CodeTheme {
        id: "kanagawa-lotus", name: "Kanagawa Lotus", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#f2ecbc", foreground: "#545464", line_number: "#766b90",
        comment: "#716e61", comment_italic: false,
        string: "#6f894e", number: "#b35b79", keyword: "#624c83", function: "#4d699b", type_: "#597b75",
        meta: "#cc6d00", tag: "#77713f", attribute: "#624c83", property: "#597b75", variable: "#545464",
        added: "#6e915f", removed: "#d7474b",
    },
    CodeTheme {
        id: "material-lighter", name: "Material Lighter", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#fafafa", foreground: "#90a4ae", line_number: "#cfd8dc",
        comment: "#90a4ae", comment_italic: true,
        string: "#91b859", number: "#f76d47", keyword: "#9c3eda", function: "#6182b8", type_: "#e2931d",
        meta: "#39adb5", tag: "#e53935", attribute: "#9c3eda", property: "#e53935", variable: "#90a4ae",
        added: "#91b859", removed: "#e53935",
    },
    CodeTheme {
        id: "min-light", name: "Min Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#ffffff", foreground: "#212121", line_number: "#cccccc",
        comment: "#c2c3c5", comment_italic: false,
        string: "#22863a", number: "#1976d2", keyword: "#d32f2f", function: "#6f42c1", type_: "#6f42c1",
        meta: "#1976d2", tag: "#22863a", attribute: "#6f42c1", property: "#d32f2f", variable: "#212121",
        added: "#77cc00", removed: "#d32f2f",
    },
    CodeTheme {
        id: "snazzy-light", name: "Snazzy Light", scheme: CodeThemeScheme::Day, adaptive: false,
        background: "#fafbfc", foreground: "#565869", line_number: "#b4b6c0",
        comment: "#adb1c2", comment_italic: false,
        string: "#cf9c00", number: "#ff5c57", keyword: "#f767bb", function: "#09a1ed", type_: "#13bbb7",
        meta: "#11658f", tag: "#13bbb7", attribute: "#13bbb7", property: "#11658f", variable: "#565869",
        added: "#2dae58", removed: "#ff5c57",
    },
    CodeTheme {
        id: "github-dark", name: "GitHub Dark", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#0d1117", foreground: "#e6edf3", line_number: "#6e7681",
        comment: "#8b949e", comment_italic: false,
        string: "#a5d6ff", number: "#79c0ff", keyword: "#ff7b72", function: "#d2a8ff", type_: "#ffa657",
        meta: "#79c0ff", tag: "#7ee787", attribute: "#79c0ff", property: "#7ee787", variable: "#e6edf3",
        added: "#3fb950", removed: "#f85149",
    },
    CodeTheme {
        id: "github-dark-dimmed", name: "GitHub Dark Dimmed", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#22272e", foreground: "#adbac7", line_number: "#636e7b",
        comment: "#768390", comment_italic: false,
        string: "#96d0ff", number: "#6cb6ff", keyword: "#f47067", function: "#dcbdfb", type_: "#f69d50",
        meta: "#6cb6ff", tag: "#8ddb8c", attribute: "#6cb6ff", property: "#8ddb8c", variable: "#adbac7",
        added: "#57ab5a", removed: "#e5534b",
    },
    CodeTheme {
        id: "dark-plus", name: "Dark+", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#1e1e1e", foreground: "#d4d4d4", line_number: "#858585",
        comment: "#6a9955", comment_italic: false,
        string: "#ce9178", number: "#b5cea8", keyword: "#569cd6", function: "#dcdcaa", type_: "#4ec9b0",
        meta: "#c586c0", tag: "#569cd6", attribute: "#9cdcfe", property: "#9cdcfe", variable: "#9cdcfe",
        added: "#b5cea8", removed: "#ce9178",
    },
    CodeTheme {
        id: "one-dark-pro", name: "One Dark Pro", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#282c34", foreground: "#abb2bf", line_number: "#495162",
        comment: "#7f848e", comment_italic: true,
        string: "#98c379", number: "#d19a66", keyword: "#c678dd", function: "#61afef", type_: "#e5c07b",
        meta: "#d19a66", tag: "#e06c75", attribute: "#d19a66", property: "#e06c75", variable: "#e06c75",
        added: "#98c379", removed: "#e06c75",
    },
    CodeTheme {
        id: "dracula", name: "Dracula", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#282a36", foreground: "#f8f8f2", line_number: "#6272a4",
        comment: "#6272a4", comment_italic: false,
        string: "#f1fa8c", number: "#bd93f9", keyword: "#ff79c6", function: "#50fa7b", type_: "#8be9fd",
        meta: "#bd93f9", tag: "#ff79c6", attribute: "#50fa7b", property: "#8be9fd", variable: "#f8f8f2",
        added: "#50fa7b", removed: "#ff5555",
    },
    CodeTheme {
        id: "monokai", name: "Monokai", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#272822", foreground: "#f8f8f2", line_number: "#90908a",
        comment: "#88846f", comment_italic: false,
        string: "#e6db74", number: "#ae81ff", keyword: "#f92672", function: "#a6e22e", type_: "#a6e22e",
        meta: "#ae81ff", tag: "#f92672", attribute: "#a6e22e", property: "#66d9ef", variable: "#f8f8f2",
        added: "#a6e22e", removed: "#f92672",
    },
    CodeTheme {
        id: "solarized-dark", name: "Solarized Dark", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#002b36", foreground: "#839496", line_number: "#586e75",
        comment: "#586e75", comment_italic: true,
        string: "#2aa198", number: "#d33682", keyword: "#859900", function: "#268bd2", type_: "#cb4b16",
        meta: "#b58900", tag: "#268bd2", attribute: "#93a1a1", property: "#859900", variable: "#268bd2",
        added: "#859900", removed: "#dc322f",
    },
    CodeTheme {
        id: "nord", name: "Nord", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#2e3440", foreground: "#d8dee9", line_number: "#4c566a",
        comment: "#616e88", comment_italic: false,
        string: "#a3be8c", number: "#b48ead", keyword: "#81a1c1", function: "#88c0d0", type_: "#8fbcbb",
        meta: "#5e81ac", tag: "#81a1c1", attribute: "#8fbcbb", property: "#8fbcbb", variable: "#d8dee9",
        added: "#a3be8c", removed: "#bf616a",
    },
    CodeTheme {
        id: "catppuccin-mocha", name: "Catppuccin Mocha", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#1e1e2e", foreground: "#cdd6f4", line_number: "#7f849c",
        comment: "#9399b2", comment_italic: true,
        string: "#a6e3a1", number: "#fab387", keyword: "#cba6f7", function: "#89b4fa", type_: "#f9e2af",
        meta: "#fab387", tag: "#89b4fa", attribute: "#f9e2af", property: "#89b4fa", variable: "#cdd6f4",
        added: "#a6e3a1", removed: "#f38ba8",
    },
    CodeTheme {
        id: "catppuccin-macchiato", name: "Catppuccin Macchiato", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#24273a", foreground: "#cad3f5", line_number: "#8087a2",
        comment: "#939ab7", comment_italic: true,
        string: "#a6da95", number: "#f5a97f", keyword: "#c6a0f6", function: "#8aadf4", type_: "#eed49f",
        meta: "#f5a97f", tag: "#8aadf4", attribute: "#eed49f", property: "#8aadf4", variable: "#cad3f5",
        added: "#a6da95", removed: "#ed8796",
    },
    CodeTheme {
        id: "catppuccin-frappe", name: "Catppuccin Frappé", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#303446", foreground: "#c6d0f5", line_number: "#838ba7",
        comment: "#949cbb", comment_italic: true,
        string: "#a6d189", number: "#ef9f76", keyword: "#ca9ee6", function: "#8caaee", type_: "#e5c890",
        meta: "#ef9f76", tag: "#8caaee", attribute: "#e5c890", property: "#8caaee", variable: "#c6d0f5",
        added: "#a6d189", removed: "#e78284",
    },
    CodeTheme {
        id: "gruvbox-dark", name: "Gruvbox Dark", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#282828", foreground: "#ebdbb2", line_number: "#665c54",
        comment: "#928374", comment_italic: true,
        string: "#b8bb26", number: "#d3869b", keyword: "#fb4934", function: "#fabd2f", type_: "#fabd2f",
        meta: "#fe8019", tag: "#8ec07c", attribute: "#fabd2f", property: "#689d6a", variable: "#83a598",
        added: "#b8bb26", removed: "#fb4934",
    },
    CodeTheme {
        id: "ayu-dark", name: "Ayu Dark", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#10141c", foreground: "#bfbdb6", line_number: "#404758",
        comment: "#5a6673", comment_italic: true,
        string: "#aad94c", number: "#d2a6ff", keyword: "#ff8f40", function: "#ffb454", type_: "#59c2ff",
        meta: "#e6c08a", tag: "#39bae6", attribute: "#ffb454", property: "#39bae6", variable: "#bfbdb6",
        added: "#70bf56", removed: "#f26d78",
    },
    CodeTheme {
        id: "ayu-mirage", name: "Ayu Mirage", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#242936", foreground: "#cccac2", line_number: "#4a5261",
        comment: "#6e7c8f", comment_italic: true,
        string: "#d5ff80", number: "#dfbfff", keyword: "#ffa659", function: "#ffcd66", type_: "#73d0ff",
        meta: "#d9be98", tag: "#5ccfe6", attribute: "#ffcd66", property: "#5ccfe6", variable: "#cccac2",
        added: "#87d96c", removed: "#f27983",
    },
    CodeTheme {
        id: "tokyo-night", name: "Tokyo Night", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#1a1b26", foreground: "#a9b1d6", line_number: "#363b54",
        comment: "#51597d", comment_italic: false,
        string: "#9ece6a", number: "#ff9e64", keyword: "#bb9af7", function: "#7aa2f7", type_: "#c0caf5",
        meta: "#73daca", tag: "#f7768e", attribute: "#bb9af7", property: "#0db9d7", variable: "#c0caf5",
        added: "#449dab", removed: "#914c54",
    },
    CodeTheme {
        id: "tokyo-night-storm", name: "Tokyo Night Storm", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#24283b", foreground: "#a9b1d6", line_number: "#3b4261",
        comment: "#5f6996", comment_italic: false,
        string: "#9ece6a", number: "#ff9e64", keyword: "#bb9af7", function: "#7aa2f7", type_: "#c0caf5",
        meta: "#73daca", tag: "#f7768e", attribute: "#bb9af7", property: "#2ac3de", variable: "#c0caf5",
        added: "#449dab", removed: "#914c54",
    },
    CodeTheme {
        id: "rose-pine", name: "Rosé Pine", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#191724", foreground: "#e0def4", line_number: "#908caa",
        comment: "#6e6a86", comment_italic: true,
        string: "#f6c177", number: "#ebbcba", keyword: "#31748f", function: "#ebbcba", type_: "#9ccfd8",
        meta: "#ebbcba", tag: "#9ccfd8", attribute: "#c4a7e7", property: "#9ccfd8", variable: "#e0def4",
        added: "#9ccfd8", removed: "#eb6f92",
    },
    CodeTheme {
        id: "rose-pine-moon", name: "Rosé Pine Moon", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#232136", foreground: "#e0def4", line_number: "#908caa",
        comment: "#6e6a86", comment_italic: true,
        string: "#f6c177", number: "#ea9a97", keyword: "#3e8fb0", function: "#ea9a97", type_: "#9ccfd8",
        meta: "#ea9a97", tag: "#9ccfd8", attribute: "#c4a7e7", property: "#9ccfd8", variable: "#e0def4",
        added: "#9ccfd8", removed: "#eb6f92",
    },
    CodeTheme {
        id: "everforest-dark", name: "Everforest Dark", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#2d353b", foreground: "#d3c6aa", line_number: "#606a64",
        comment: "#859289", comment_italic: true,
        string: "#dbbc7f", number: "#d699b6", keyword: "#e67e80", function: "#a7c080", type_: "#7fbbb3",
        meta: "#83c092", tag: "#e69875", attribute: "#dbbc7f", property: "#e69875", variable: "#d3c6aa",
        added: "#a7c080", removed: "#e67e80",
    },
    CodeTheme {
        id: "night-owl", name: "Night Owl", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#011627", foreground: "#d6deeb", line_number: "#4b6479",
        comment: "#637777", comment_italic: false,
        string: "#ecc48d", number: "#f78c6c", keyword: "#c792ea", function: "#82aaff", type_: "#ffcb8b",
        meta: "#82aaff", tag: "#caece6", attribute: "#c5e478", property: "#7fdbca", variable: "#c5e478",
        added: "#22da6e", removed: "#ef5350",
    },
    CodeTheme {
        id: "material-palenight", name: "Material Palenight", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#292d3e", foreground: "#babed8", line_number: "#3a3f58",
        comment: "#676e95", comment_italic: true,
        string: "#c3e88d", number: "#f78c6c", keyword: "#c792ea", function: "#82aaff", type_: "#ffcb6b",
        meta: "#89ddff", tag: "#f07178", attribute: "#c792ea", property: "#f07178", variable: "#babed8",
        added: "#c3e88d", removed: "#f07178",
    },
    CodeTheme {
        id: "material-ocean", name: "Material Ocean", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#0f111a", foreground: "#babed8", line_number: "#252836",
        comment: "#464b5d", comment_italic: true,
        string: "#c3e88d", number: "#f78c6c", keyword: "#c792ea", function: "#82aaff", type_: "#ffcb6b",
        meta: "#89ddff", tag: "#f07178", attribute: "#c792ea", property: "#f07178", variable: "#babed8",
        added: "#c3e88d", removed: "#f07178",
    },
    CodeTheme {
        id: "material-darker", name: "Material Darker", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#212121", foreground: "#eeffff", line_number: "#424242",
        comment: "#545454", comment_italic: true,
        string: "#c3e88d", number: "#f78c6c", keyword: "#c792ea", function: "#82aaff", type_: "#ffcb6b",
        meta: "#89ddff", tag: "#f07178", attribute: "#c792ea", property: "#f07178", variable: "#eeffff",
        added: "#c3e88d", removed: "#f07178",
    },
    CodeTheme {
        id: "kanagawa-wave", name: "Kanagawa Wave", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#1f1f28", foreground: "#dcd7ba", line_number: "#54546d",
        comment: "#727169", comment_italic: false,
        string: "#98bb6c", number: "#d27e99", keyword: "#957fb8", function: "#7e9cd8", type_: "#7aa89f",
        meta: "#ffa066", tag: "#e6c384", attribute: "#957fb8", property: "#7aa89f", variable: "#dcd7ba",
        added: "#76946a", removed: "#c34043",
    },
    CodeTheme {
        id: "kanagawa-dragon", name: "Kanagawa Dragon", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#181616", foreground: "#c5c9c5", line_number: "#625e5a",
        comment: "#737c73", comment_italic: false,
        string: "#8a9a7b", number: "#a292a3", keyword: "#8992a7", function: "#8ba4b0", type_: "#8ea4a2",
        meta: "#b6927b", tag: "#c4b28a", attribute: "#8992a7", property: "#8ea4a2", variable: "#c5c9c5",
        added: "#76946a", removed: "#c34043",
    },
    CodeTheme {
        id: "synthwave-84", name: "Synthwave '84", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#262335", foreground: "#bbbbbb", line_number: "#888690",
        comment: "#848bbd", comment_italic: true,
        string: "#ff8b39", number: "#f97e72", keyword: "#fede5d", function: "#36f9f6", type_: "#fe4450",
        meta: "#f97e72", tag: "#72f1b8", attribute: "#fede5d", property: "#72f1b8", variable: "#ff7edb",
        added: "#63c89e", removed: "#fe4450",
    },
    CodeTheme {
        id: "vitesse-dark", name: "Vitesse Dark", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#121212", foreground: "#cecabe", line_number: "#52514f",
        comment: "#687668", comment_italic: false,
        string: "#c98a7d", number: "#4c9a91", keyword: "#4d9375", function: "#80a665", type_: "#5da994",
        meta: "#b8a965", tag: "#4d9375", attribute: "#bd976a", property: "#b8a965", variable: "#bd976a",
        added: "#4d9375", removed: "#cb7676",
    },
    CodeTheme {
        id: "vesper", name: "Vesper", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#101010", foreground: "#ffffff", line_number: "#505050",
        comment: "#575757", comment_italic: false,
        string: "#99ffe4", number: "#ffc799", keyword: "#a0a0a0", function: "#ffc799", type_: "#ffc799",
        meta: "#a0a0a0", tag: "#ffc799", attribute: "#a0a0a0", property: "#ffc799", variable: "#ffffff",
        added: "#99ffe4", removed: "#ff8080",
    },
    CodeTheme {
        id: "houston", name: "Houston", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#17191e", foreground: "#eef0f9", line_number: "#545864",
        comment: "#909299", comment_italic: true,
        string: "#ffd493", number: "#ffd493", keyword: "#54b9ff", function: "#00daef", type_: "#acafff",
        meta: "#ffd493", tag: "#54b9ff", attribute: "#4bf3c8", property: "#4bf3c8", variable: "#4bf3c8",
        added: "#4bf3c8", removed: "#f4587e",
    },
    CodeTheme {
        id: "andromeeda", name: "Andromeeda", scheme: CodeThemeScheme::Night, adaptive: false,
        background: "#23262e", foreground: "#d5ced9", line_number: "#746f77",
        comment: "#87888f", comment_italic: false,
        string: "#96e072", number: "#f39c12", keyword: "#c74ded", function: "#ffe66d", type_: "#ffe66d",
        meta: "#ee5d43", tag: "#f92672", attribute: "#ffe66d", property: "#c74ded", variable: "#00e8c6",
        added: "#96e072", removed: "#ee5d43",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const SCHEMES: [CodeThemeScheme; 2] = [CodeThemeScheme::Day, CodeThemeScheme::Night];

    fn is_hex_digits(text: &str) -> bool {
        text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    /// `^#[0-9a-f]{n}$`.
    fn is_hex_color(text: &str, digits: usize) -> bool {
        text.len() == digits + 1 && text.starts_with('#') && is_hex_digits(&text[1..])
    }

    // lib/appearance.ts has no test file upstream; these cover its logic.

    #[test]
    fn keeps_the_factory_defaults_field_for_field() {
        let value = serde_json::to_value(default_appearance_preferences()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "themeColor": "", "zoom": 1.0, "uiFontFamily": "", "monoFontFamily": "", "messageFontSize": 14,
                "serifMessages": false, "wideMessages": false, "sendShortcut": ["Enter"], "newlineShortcut": ["Shift", "Enter"],
                "spellCheck": false, "renderUserMarkdown": false, "confirmMessageDelete": false, "collapseReasoning": true,
                "codeBlockCollapsible": false, "codeBlockWrappable": false, "singleDollarMath": true, "customCss": "",
                "liquidGlass": false, "background": "solid", "codeThemeDay": "mewrk-light", "codeThemeNight": "mewrk-dark",
                "localModel": {
                    "titles": false, "shellExplanations": false, "errorExplanations": false, "subagents": false,
                    "titlePrompt": "", "shellPrompt": "", "errorPrompt": "",
                },
            })
        );
    }

    #[test]
    fn a_stylesheet_written_by_upstream_comes_back_as_it_went_in() {
        let mut document = serde_json::to_value(default_appearance_preferences()).unwrap();
        document["customCss"] = serde_json::json!(".composer { color: red; }");
        let read: AppearancePreferences = serde_json::from_value(document.clone()).unwrap();
        assert_eq!(serde_json::to_value(&read).unwrap(), document);
    }

    #[test]
    fn offers_the_four_composer_chords_in_display_order() {
        let primary = if cfg!(target_os = "macos") { "Meta" } else { "Control" };
        assert_eq!(composer_shortcut_choices(), [vec!["Enter"], vec!["Shift", "Enter"], vec![primary, "Enter"], vec!["Alt", "Enter"]]);
    }

    #[test]
    fn normalizes_a_hex_color_or_keeps_the_draft() {
        assert_eq!(normalize_hex_color("").as_deref(), Some(""));
        assert_eq!(normalize_hex_color("   ").as_deref(), Some(""));
        assert_eq!(normalize_hex_color("#356ae6").as_deref(), Some("#356AE6"));
        assert_eq!(normalize_hex_color(" 356ae6 ").as_deref(), Some("#356AE6"));
        assert_eq!(normalize_hex_color("abc").as_deref(), Some("#AABBCC"));
        assert_eq!(normalize_hex_color("#AbC").as_deref(), Some("#AABBCC"));
        for draft in ["#", "##abc", "12", "1234", "12345", "1234567", "xyz", "#12345g", "红绿蓝"] {
            assert_eq!(normalize_hex_color(draft), None, "{draft}");
        }
    }

    #[test]
    fn clamps_zoom_and_keeps_accumulated_steps_on_the_grid() {
        assert_eq!(clamp_zoom(1.0), 1.0);
        assert_eq!(clamp_zoom(0.1), MIN_ZOOM);
        assert_eq!(clamp_zoom(9.0), MAX_ZOOM);
        assert_eq!(clamp_zoom(f64::NAN), 1.0);
        assert_eq!(clamp_zoom(f64::INFINITY), 1.0);
        assert_eq!(clamp_zoom(f64::NEG_INFINITY), 1.0);
        let mut zoom = 1.0;
        for _ in 0..3 {
            zoom = clamp_zoom(zoom + ZOOM_STEP);
        }
        assert_eq!(zoom, 1.3);
        for _ in 0..3 {
            zoom = clamp_zoom(zoom - ZOOM_STEP);
        }
        assert_eq!(zoom, 1.0);
    }

    #[test]
    fn clamps_the_message_font_size_to_whole_pixels() {
        assert_eq!(clamp_message_font_size(14.0), 14);
        assert_eq!(clamp_message_font_size(14.5), 15);
        assert_eq!(clamp_message_font_size(1.0), MIN_MESSAGE_FONT_SIZE);
        assert_eq!(clamp_message_font_size(100.0), MAX_MESSAGE_FONT_SIZE);
        assert_eq!(clamp_message_font_size(f64::NAN), 14);
        assert_eq!(clamp_message_font_size(f64::INFINITY), 14);
    }

    #[test]
    fn quotes_only_a_single_bare_font_family() {
        assert_eq!(font_family_value("  ", "serif"), "serif");
        assert_eq!(font_family_value(" Inter ", "serif"), "\"Inter\", serif");
        assert_eq!(font_family_value("Inter, Arial", "serif"), "Inter, Arial");
        assert_eq!(font_family_value("\"My Font\"", "serif"), "\"My Font\"");
    }

    #[test]
    fn derives_everything_apply_appearance_set() {
        let plain = derive_appearance(&default_appearance_preferences());
        assert_eq!(plain.accent, None);
        assert_eq!(plain.accent_soft, None);
        assert_eq!(plain.app_font_family, DEFAULT_UI_FONT_STACK);
        assert_eq!(plain.mono_font_family, DEFAULT_MONO_FONT_STACK);
        assert_eq!(plain.message_font_family, DEFAULT_UI_FONT_STACK);
        assert_eq!(plain.message_font_size, 14);
        assert_eq!(plain.zoom, 1.0);
        assert!(!plain.backdrop_custom);
        assert!(!plain.glass);
        assert_eq!(plain.code_day, code_theme_variables(code_theme_for("mewrk-light", CodeThemeScheme::Day), None));
        assert_eq!(plain.code_night, code_theme_variables(code_theme_for("mewrk-dark", CodeThemeScheme::Night), None));

        let chosen = derive_appearance(&AppearancePreferences {
            theme_color: "#abc".to_owned(),
            zoom: 5.0,
            ui_font_family: "Inter".to_owned(),
            mono_font_family: "Fira Code".to_owned(),
            message_font_size: 99,
            serif_messages: true,
            wide_messages: true,
            code_block_wrappable: true,
            code_block_collapsible: true,
            background: "builtin:desk".to_owned(),
            liquid_glass: true,
            code_theme_day: "github-light".to_owned(),
            code_theme_night: "github-light".to_owned(),
            ..default_appearance_preferences()
        });
        assert_eq!(chosen.accent.as_deref(), Some("#AABBCC"));
        assert_eq!(chosen.accent_soft.as_deref(), Some("#AABBCC1F"));
        assert_eq!(chosen.app_font_family, format!("\"Inter\", {DEFAULT_UI_FONT_STACK}"));
        assert_eq!(chosen.mono_font_family, format!("\"Fira Code\", {DEFAULT_MONO_FONT_STACK}"));
        assert_eq!(chosen.message_font_family, SERIF_FONT_STACK);
        assert_eq!(chosen.message_font_size, MAX_MESSAGE_FONT_SIZE);
        assert_eq!(chosen.zoom, MAX_ZOOM);
        assert!(chosen.wide_messages && chosen.code_wrap && chosen.code_collapse && chosen.backdrop_custom && chosen.glass);
        assert_eq!(chosen.code_day, code_theme_variables(code_theme_for("github-light", CodeThemeScheme::Day), None));
        // A theme of the other scheme reads as the default.
        assert_eq!(chosen.code_night, plain.code_night);

        // A color still being typed leaves the theme's own accent.
        assert_eq!(derive_appearance(&AppearancePreferences { theme_color: "#12".to_owned(), ..default_appearance_preferences() }).accent, None);
    }

    // lib/background.test.ts, what this file ported of it.

    #[test]
    fn lets_the_ground_through_only_for_something_other_than_the_themes_own() {
        let imported = "0123456789abcdef".repeat(4);
        assert!(!replaces_theme_ground("solid"));
        assert!(replaces_theme_ground("solid:day"));
        assert!(replaces_theme_ground("solid:night"));
        assert!(replaces_theme_ground("builtin:desk"));
        assert!(replaces_theme_ground("builtin:curtain"));
        assert!(replaces_theme_ground(&imported));
        // Anything unknown is the theme's own ground.
        assert!(!replaces_theme_ground("builtin:gone"));
        assert!(!replaces_theme_ground(&imported.to_uppercase()));
        assert!(!replaces_theme_ground(&imported[1..]));
    }

    // lib/codeThemes.test.ts.

    #[test]
    fn gives_every_theme_its_own_id() {
        let ids: HashSet<&str> = CODE_THEMES.iter().map(|theme| theme.id).collect();
        assert_eq!(ids.len(), CODE_THEMES.len());
    }

    #[test]
    fn gives_every_theme_a_day_or_night_scheme_and_six_digit_lowercase_colours() {
        // The scheme is an enum here; the colours are what upstream's generated data could get wrong.
        for theme in CODE_THEMES {
            for colour in [
                theme.background, theme.foreground, theme.line_number, theme.comment, theme.string, theme.number, theme.keyword, theme.function,
                theme.type_, theme.meta, theme.tag, theme.attribute, theme.property, theme.variable, theme.added, theme.removed,
            ] {
                assert!(is_hex_color(colour, 6), "{} {colour}", theme.id);
            }
        }
        assert_eq!(CODE_THEMES.len(), 48);
    }

    #[test]
    fn makes_the_apps_own_adaptive_theme_the_default_of_each_scheme() {
        assert_eq!(default_code_theme(CodeThemeScheme::Day), "mewrk-light");
        assert_eq!(default_code_theme(CodeThemeScheme::Night), "mewrk-dark");
        for scheme in SCHEMES {
            let theme = CODE_THEMES.iter().find(|candidate| candidate.id == default_code_theme(scheme)).unwrap();
            assert_eq!(theme.scheme, scheme);
            assert!(theme.adaptive);
        }
    }

    #[test]
    fn lists_the_day_themes_for_day_and_the_night_themes_for_night_the_apps_own_first() {
        let day = code_themes_for(CodeThemeScheme::Day);
        let night = code_themes_for(CodeThemeScheme::Night);
        assert!(day.iter().all(|theme| theme.scheme == CodeThemeScheme::Day));
        assert_eq!(day[0].id, "mewrk-light");
        assert!(night.iter().all(|theme| theme.scheme == CodeThemeScheme::Night));
        assert_eq!(night[0].id, "mewrk-dark");
        assert_eq!(day.len() + night.len(), CODE_THEMES.len());
    }

    #[test]
    fn resolves_an_unknown_or_other_scheme_id_to_the_schemes_default() {
        assert_eq!(code_theme_for("no-such-theme", CodeThemeScheme::Day).id, "mewrk-light");
        assert_eq!(code_theme_for("dracula", CodeThemeScheme::Day).id, "mewrk-light");
        assert_eq!(code_theme_for("github-light", CodeThemeScheme::Night).id, "mewrk-dark");
        assert_eq!(normalize_code_theme("dracula", CodeThemeScheme::Day), "mewrk-light");
        assert_eq!(normalize_code_theme("github-light", CodeThemeScheme::Night), "mewrk-dark");
    }

    #[test]
    fn keeps_a_valid_id_of_the_scheme_as_itself() {
        assert_eq!(normalize_code_theme("github-light", CodeThemeScheme::Day), "github-light");
        assert_eq!(normalize_code_theme("dracula", CodeThemeScheme::Night), "dracula");
        assert_eq!(normalize_code_theme("mewrk-dark", CodeThemeScheme::Night), "mewrk-dark");
        assert_eq!(code_theme_for("dracula", CodeThemeScheme::Night).id, "dracula");
    }

    #[test]
    fn leaves_an_adaptive_themes_surface_to_the_app_by_default_with_every_token_and_tint() {
        let theme = code_theme_for("mewrk-light", CodeThemeScheme::Day);
        let variables = code_theme_variables(theme, None);
        assert_eq!(
            variables.keys().copied().collect::<Vec<_>>(),
            [
                "added", "added-line", "added-word", "attribute", "comment", "comment-style", "function", "keyword", "meta", "number", "property", "removed",
                "removed-line", "removed-word", "string", "tag", "type", "variable",
            ]
        );
        assert_eq!(variables["comment-style"], "italic");
        for (kind, colour) in [
            ("comment", theme.comment), ("string", theme.string), ("number", theme.number), ("keyword", theme.keyword), ("function", theme.function),
            ("type", theme.type_), ("meta", theme.meta), ("tag", theme.tag), ("attribute", theme.attribute), ("property", theme.property), ("variable", theme.variable),
        ] {
            assert_eq!(variables[kind], colour, "{kind}");
        }
        assert_eq!(variables["added"], theme.added);
        assert_eq!(variables["removed"], theme.removed);
    }

    #[test]
    fn sets_the_themes_own_ground_text_and_line_numbers_when_asked_for_the_themes_surface() {
        let theme = code_theme_for("mewrk-dark", CodeThemeScheme::Night);
        let variables = code_theme_variables(theme, Some(CodeThemeSurface::Theme));
        assert_eq!(variables["bg"], theme.background);
        assert_eq!(variables["fg"], theme.foreground);
        assert_eq!(variables["line-number"], theme.line_number);
    }

    #[test]
    fn draws_an_adaptive_themes_glass_as_its_text_colour_at_low_alpha_not_as_its_ground() {
        for theme in [code_theme_for("mewrk-light", CodeThemeScheme::Day), code_theme_for("mewrk-dark", CodeThemeScheme::Night)] {
            let variables = code_theme_variables(theme, Some(CodeThemeSurface::Glass));
            assert!(is_hex_color(&variables["bg"], 8), "{}", theme.id);
            assert_ne!(variables["bg"], theme.background, "{}", theme.id);
        }
    }

    #[test]
    fn keeps_a_non_adaptive_themes_own_opaque_ground_even_when_glass_is_asked_for() {
        let dracula = code_theme_for("dracula", CodeThemeScheme::Night);
        let variables = code_theme_variables(dracula, None);
        assert_eq!(variables["bg"], "#282a36");
        assert_eq!(variables["fg"], "#f8f8f2");
        assert_eq!(variables["line-number"], "#6272a4");
        assert_eq!(code_theme_variables(dracula, Some(CodeThemeSurface::Glass))["bg"], "#282a36");
    }

    #[test]
    fn tints_each_changed_line_and_its_changed_words_in_the_themes_hue_the_words_more_strongly() {
        for theme in CODE_THEMES {
            let variables = code_theme_variables(theme, None);
            for (side, line, word, hue) in [("added", "added-line", "added-word", theme.added), ("removed", "removed-line", "removed-word", theme.removed)] {
                let (line, word) = (&variables[line], &variables[word]);
                assert!(line.starts_with(hue) && line.len() == 9 && is_hex_digits(&line[7..]), "{} {side}-line", theme.id);
                assert!(word.starts_with(hue) && word.len() == 9 && is_hex_digits(&word[7..]), "{} {side}-word", theme.id);
                let alpha = |text: &str| u8::from_str_radix(&text[7..], 16).unwrap();
                assert!(alpha(word) > alpha(line), "{}", theme.id);
            }
        }
    }

    #[test]
    fn names_in_code_theme_variables_every_variable_any_theme_can_set() {
        for theme in CODE_THEMES {
            for surface in [None, Some(CodeThemeSurface::App), Some(CodeThemeSurface::Theme), Some(CodeThemeSurface::Glass)] {
                for name in code_theme_variables(theme, surface).keys() {
                    assert!(CODE_THEME_VARIABLES.contains(name), "{}: {name}", theme.id);
                }
            }
        }
    }
}
