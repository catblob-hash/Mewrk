//! Fluent 2 design tokens, with the values WinUI 3 ships.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

// SAFETY: four u8 fields under repr(C): no padding, and every bit pattern is valid.
unsafe impl bytemuck::Zeroable for Color {}
unsafe impl bytemuck::Pod for Color {}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
    pub const WHITE: Color = Color::rgb(0xFFFFFF);
    pub const BLACK: Color = Color::rgb(0x000000);

    /// From 0xRRGGBB.
    pub const fn rgb(v: u32) -> Self {
        Self { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a: 255 }
    }

    /// From 0xAARRGGBB, the notation the WinUI resource dictionaries use.
    pub const fn argb(v: u32) -> Self {
        Self { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a: (v >> 24) as u8 }
    }

    pub fn with_alpha(self, a: f32) -> Self {
        Self { a: (a.clamp(0.0, 1.0) * 255.0).round() as u8, ..self }
    }

    pub fn fade(self, f: f32) -> Self {
        Self { a: (self.a as f32 * f.clamp(0.0, 1.0)).round() as u8, ..self }
    }

    /// Linear interpolation in gamma space, premultiplied: a solid colour going to a faint one fades, where interpolating the
    /// channels on their own would pass through a bright ghost of the faint one's hue.
    pub fn mix(self, o: Color, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let m = |a: f32, b: f32| a + (b - a) * t;
        let (a0, a1) = (self.a as f32, o.a as f32);
        let a = m(a0, a1);
        if a <= 0.0 {
            return Self { a: 0, ..o };
        }
        let c = |x: u8, y: u8| (m(x as f32 * a0, y as f32 * a1) / a).round().clamp(0.0, 255.0) as u8;
        Self { r: c(self.r, o.r), g: c(self.g, o.g), b: c(self.b, o.b), a: a.round() as u8 }
    }

    /// `top` painted over this.
    pub fn over(self, top: Color) -> Self {
        let (below, above) = (self.a as f32 / 255.0, top.a as f32 / 255.0);
        let a = above + below * (1.0 - above);
        if a <= 0.0 {
            return Color::TRANSPARENT;
        }
        let c = |b: u8, t: u8| ((t as f32 * above + b as f32 * below * (1.0 - above)) / a).round().clamp(0.0, 255.0) as u8;
        Self { r: c(self.r, top.r), g: c(self.g, top.g), b: c(self.b, top.b), a: (a * 255.0).round() as u8 }
    }

    pub fn to_linear_f32(self) -> [f32; 4] {
        [self.r as f32 / 255.0, self.g as f32 / 255.0, self.b as f32 / 255.0, self.a as f32 / 255.0]
    }
}

/// The Fluent type ramp.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TypeStyle {
    pub size: f32,
    pub line: f32,
    pub weight: Weight,
    pub mono: bool,
    pub italic: bool,
    /// The chain [`crate::TextCtx::set_serif_family`] set, in front of the interface fonts. `mono` wins over it.
    pub serif: bool,
}

/// By the bits of its sizes: what tells two styles apart for a cache.
impl std::hash::Hash for TypeStyle {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.size.to_bits().hash(h);
        self.line.to_bits().hash(h);
        self.weight.hash(h);
        self.mono.hash(h);
        self.italic.hash(h);
        self.serif.hash(h);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Weight {
    Regular,
    Semibold,
}

const fn ty(size: f32, line: f32, weight: Weight) -> TypeStyle {
    TypeStyle { size, line, weight, mono: false, italic: false, serif: false }
}

pub const CAPTION: TypeStyle = ty(12.0, 16.0, Weight::Regular);
pub const CAPTION_STRONG: TypeStyle = ty(12.0, 16.0, Weight::Semibold);
pub const FINE: TypeStyle = ty(11.0, 14.0, Weight::Regular);
pub const FINE_STRONG: TypeStyle = ty(11.0, 14.0, Weight::Semibold);
pub const BODY_SMALL: TypeStyle = ty(13.0, 18.0, Weight::Regular);
pub const BODY_SMALL_STRONG: TypeStyle = ty(13.0, 18.0, Weight::Semibold);
pub const BODY: TypeStyle = ty(14.0, 20.0, Weight::Regular);
pub const BODY_STRONG: TypeStyle = ty(14.0, 20.0, Weight::Semibold);
pub const BODY_LARGE: TypeStyle = ty(18.0, 24.0, Weight::Regular);
pub const SUBTITLE: TypeStyle = ty(20.0, 28.0, Weight::Semibold);
pub const TITLE: TypeStyle = ty(28.0, 36.0, Weight::Semibold);
pub const MONO: TypeStyle = TypeStyle { size: 13.0, line: 20.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
pub const MONO_SMALL: TypeStyle = TypeStyle { size: 11.0, line: 15.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
pub const MONO_BODY: TypeStyle = TypeStyle { size: 12.0, line: 17.0, weight: Weight::Regular, mono: true, italic: false, serif: false };

pub const CONTROL_RADIUS: f32 = 4.0;
pub const OVERLAY_RADIUS: f32 = 8.0;

/// Motion: durations from WinUI, with the curves of the Fluent 2 motion guidance.
pub mod motion {
    pub const FASTER: f32 = 0.083;
    pub const FAST: f32 = 0.167;
    pub const NORMAL: f32 = 0.250;
    pub const SLOW: f32 = 0.358;

    fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        let curve = |a: f32, b: f32, t: f32| {
            let u = 1.0 - t;
            3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
        };
        // Newton on x(t), which converges in a few steps for the monotonic curves used here.
        let mut t = x;
        for _ in 0..6 {
            let e = curve(x1, x2, t) - x;
            let u = 1.0 - t;
            let d = 3.0 * u * u * x1 + 6.0 * u * t * (x2 - x1) + 3.0 * t * t * (1.0 - x2);
            if d.abs() < 1e-5 {
                break;
            }
            t = (t - e / d).clamp(0.0, 1.0);
        }
        curve(y1, y2, t)
    }

    /// Things arriving: fast start, long settle.
    pub fn decelerate(x: f32) -> f32 {
        bezier(0.1, 0.9, 0.2, 1.0, x)
    }

    /// Things leaving.
    pub fn accelerate(x: f32) -> f32 {
        bezier(0.7, 0.0, 1.0, 0.5, x)
    }

    pub fn ease(x: f32) -> f32 {
        bezier(0.33, 0.0, 0.67, 1.0, x)
    }
}

/// What code and diffs are drawn in. Each token kind has its colour; `adaptive` keeps the app's own ground and text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CodePalette {
    pub adaptive: bool,
    pub background: Color,
    pub foreground: Color,
    pub line_number: Color,
    pub comment: Color,
    pub comment_italic: bool,
    pub string: Color,
    pub number: Color,
    pub keyword: Color,
    pub function: Color,
    pub type_: Color,
    /// Decorators, preprocessor directives, a diff's file headers.
    pub meta: Color,
    pub tag: Color,
    pub attribute: Color,
    /// A key in data or configuration.
    pub property: Color,
    /// A sigilled variable: `$HOME`, `@name`.
    pub variable: Color,
    /// The hues an added and a removed line are marked and tinted with.
    pub added: Color,
    pub removed: Color,
}

impl CodePalette {
    /// Mewrk's own: code keeps the window's surface.
    pub const fn mewrk(dark: bool) -> Self {
        const fn c(dark: bool, night: u32, day: u32) -> Color {
            Color::rgb(if dark { night } else { day })
        }
        Self {
            adaptive: true,
            background: c(dark, 0x11110E, 0xF1F1EE),
            foreground: c(dark, 0xCECECB, 0x343431),
            line_number: c(dark, 0x6D6D67, 0x989892),
            comment: c(dark, 0x8A8A83, 0x73736D),
            comment_italic: true,
            string: c(dark, 0x8FD0AE, 0x246B4E),
            number: c(dark, 0xF0A070, 0xA2531D),
            keyword: c(dark, 0xA99CF0, 0x6254AA),
            function: c(dark, 0xDCC48E, 0x795D2E),
            type_: c(dark, 0x6CC4D8, 0x1F7389),
            meta: c(dark, 0xE58FB0, 0xAA4F6A),
            tag: c(dark, 0xEE8A82, 0x8E3630),
            attribute: c(dark, 0xE8B05C, 0x96580F),
            property: c(dark, 0x86AEF0, 0x315EAE),
            variable: c(dark, 0x86AEF0, 0x315EAE),
            added: c(dark, 0x6FCF82, 0x287A38),
            removed: c(dark, 0xF08A8A, 0xA23737),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub dark: bool,

    pub mica_base: Color,
    pub mica_tint_a: Color,
    pub mica_tint_b: Color,
    pub noise_alpha: f32,
    pub text_gamma: f32,

    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_tertiary: Color,
    pub text_disabled: Color,
    pub text_on_accent: Color,

    pub accent: Color,
    pub accent_hover: Color,
    pub accent_pressed: Color,
    pub accent_text: Color,

    pub control_fill: Color,
    pub control_fill_hover: Color,
    pub control_fill_pressed: Color,
    pub control_fill_disabled: Color,
    pub control_stroke: Color,
    pub control_stroke_bottom: Color,
    pub control_strong_stroke: Color,

    pub subtle_hover: Color,
    pub subtle_pressed: Color,

    pub layer: Color,
    pub card: Color,
    pub card_secondary: Color,
    pub card_stroke: Color,
    pub divider: Color,

    pub flyout: Color,
    pub flyout_stroke: Color,

    pub success: Color,
    pub caution: Color,
    pub critical: Color,
    pub attention: Color,
    pub success_bg: Color,
    pub caution_bg: Color,
    pub critical_bg: Color,
    pub attention_bg: Color,
    /// A control whose change would throw away a warm prompt cache: its main colour, on or off.
    pub cache_warm: Color,
    /// The same, for a control that is a filled row.
    pub cache_warm_bg: Color,

    pub selection: Color,
    pub code_bg: Color,
    pub code: CodePalette,
}

impl Theme {
    pub fn dark() -> Self {
        let accent = Color::rgb(0x60CDFF);
        Self {
            dark: true,
            mica_base: Color::rgb(0x202020),
            mica_tint_a: Color::rgb(0x2A3A4E).with_alpha(0.75),
            mica_tint_b: Color::rgb(0x33283E).with_alpha(0.55),
            noise_alpha: 0.012,
            text_gamma: 1.0,

            text_primary: Color::rgb(0xFFFFFF),
            text_secondary: Color::argb(0xC8FFFFFF),
            text_tertiary: Color::argb(0x8BFFFFFF),
            text_disabled: Color::argb(0x5DFFFFFF),
            text_on_accent: Color::rgb(0x000000),

            accent,
            accent_hover: accent.with_alpha(0.9),
            accent_pressed: accent.with_alpha(0.8),
            accent_text: Color::rgb(0x99EBFF),

            control_fill: Color::argb(0x0FFFFFFF),
            control_fill_hover: Color::argb(0x15FFFFFF),
            control_fill_pressed: Color::argb(0x08FFFFFF),
            control_fill_disabled: Color::argb(0x0BFFFFFF),
            control_stroke: Color::argb(0x12FFFFFF),
            control_stroke_bottom: Color::argb(0x12FFFFFF),
            control_strong_stroke: Color::argb(0x8BFFFFFF),

            subtle_hover: Color::argb(0x0FFFFFFF),
            subtle_pressed: Color::argb(0x0AFFFFFF),

            layer: Color::argb(0x4C3A3A3A),
            card: Color::argb(0x0DFFFFFF),
            card_secondary: Color::argb(0x08FFFFFF),
            card_stroke: Color::argb(0x1A000000),
            divider: Color::argb(0x15FFFFFF),

            flyout: Color::rgb(0x2C2C2C),
            flyout_stroke: Color::rgb(0x484848),

            success: Color::rgb(0x6CCB5F),
            caution: Color::rgb(0xFCE100),
            critical: Color::rgb(0xFF99A4),
            attention: Color::rgb(0x60CDFF),
            success_bg: Color::rgb(0x393D1B),
            caution_bg: Color::rgb(0x433519),
            critical_bg: Color::rgb(0x442726),
            attention_bg: Color::argb(0x08FFFFFF),
            cache_warm: Color::rgb(0xFF8C00),
            cache_warm_bg: Color::rgb(0x4A2E12),

            selection: accent.with_alpha(0.35),
            code_bg: Color::argb(0x4D000000),
            code: CodePalette::mewrk(true),
        }
    }

    pub fn light() -> Self {
        let accent = Color::rgb(0x005FB8);
        Self {
            dark: false,
            mica_base: Color::rgb(0xF3F3F3),
            mica_tint_a: Color::rgb(0xD8E6F6).with_alpha(0.8),
            mica_tint_b: Color::rgb(0xEDE3F3).with_alpha(0.7),
            noise_alpha: 0.008,
            text_gamma: 0.82,

            text_primary: Color::argb(0xE4000000),
            text_secondary: Color::argb(0x9E000000),
            text_tertiary: Color::argb(0x72000000),
            text_disabled: Color::argb(0x5C000000),
            text_on_accent: Color::rgb(0xFFFFFF),

            accent,
            accent_hover: accent.with_alpha(0.9),
            accent_pressed: accent.with_alpha(0.8),
            accent_text: Color::rgb(0x003E92),

            control_fill: Color::argb(0xB3FFFFFF),
            control_fill_hover: Color::argb(0x80F9F9F9),
            control_fill_pressed: Color::argb(0x4DF9F9F9),
            control_fill_disabled: Color::argb(0x4DF9F9F9),
            control_stroke: Color::argb(0x0F000000),
            control_stroke_bottom: Color::argb(0x29000000),
            control_strong_stroke: Color::argb(0x72000000),

            subtle_hover: Color::argb(0x09000000),
            subtle_pressed: Color::argb(0x06000000),

            layer: Color::argb(0x80FFFFFF),
            card: Color::argb(0xB3FFFFFF),
            card_secondary: Color::argb(0x80F6F6F6),
            card_stroke: Color::argb(0x0F000000),
            divider: Color::argb(0x14000000),

            flyout: Color::rgb(0xFCFCFC),
            flyout_stroke: Color::rgb(0xCFCFCF),

            success: Color::rgb(0x0F7B0F),
            caution: Color::rgb(0x9D5D00),
            critical: Color::rgb(0xC42B1C),
            attention: Color::rgb(0x005FB8),
            success_bg: Color::rgb(0xDFF6DD),
            caution_bg: Color::rgb(0xFFF4CE),
            critical_bg: Color::rgb(0xFDE7E9),
            attention_bg: Color::argb(0x80F6F6F6),
            cache_warm: Color::rgb(0xCA5010),
            cache_warm_bg: Color::rgb(0xFFE3CC),

            selection: accent.with_alpha(0.28),
            code_bg: Color::argb(0x0C000000),
            code: CodePalette::mewrk(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_the_way_winui_writes_them() {
        assert_eq!(Color::argb(0x0FFFFFFF), Color { r: 255, g: 255, b: 255, a: 15 });
        assert_eq!(Color::rgb(0x60CDFF), Color { r: 0x60, g: 0xCD, b: 0xFF, a: 255 });
    }

    #[test]
    fn motion_curves_start_and_end_where_they_should() {
        for f in [motion::decelerate, motion::accelerate, motion::ease] {
            assert!(f(0.0).abs() < 1e-3);
            assert!((f(1.0) - 1.0).abs() < 1e-3);
            let mut prev = -1.0;
            for i in 0..=100 {
                let y = f(i as f32 / 100.0);
                assert!(y >= prev - 1e-4, "not monotonic at {i}");
                prev = y;
            }
        }
        assert!(motion::decelerate(0.3) > 0.7, "decelerate should cover most of the distance early");
    }
}
