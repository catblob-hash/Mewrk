//! Terminal colours, tuned to sit on the Fluent surface in both themes.

use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color as Ansi, NamedColor, Rgb};
use fluent::{Color, Theme};

/// Base colours borrow Fluent's status tokens where they exist, so `ls` and an InfoBar agree.
const DARK: [u32; 16] = [
    0x3C3C3C, 0xF1707B, 0x6CCB5F, 0xE3C341, 0x6CB6FF, 0xD68FE8, 0x4FC9D9, 0xCFCFCF, //
    0x8B8B8B, 0xFF99A4, 0x9BE58A, 0xFCE100, 0x99CCFF, 0xEFB5FF, 0x8BE8F0, 0xFFFFFF,
];

/// "White" is dark here: programs print it as ordinary text, assuming a dark ground, and it must stay readable.
const LIGHT: [u32; 16] = [
    0x1F1F1F, 0xC42B1C, 0x0F7B0F, 0x9D5D00, 0x005FB8, 0x9A1F9A, 0x007C8A, 0x5C5C5C, //
    0x6E6E6E, 0xD13438, 0x107C10, 0x986F0B, 0x0078D4, 0xB82FB0, 0x038387, 0x3A3A3A,
];

/// What Mewrk's xterm asks for with `minimumContrastRatio`: 256-colour and truecolour programs pick their own
/// colours, knowing nothing of this ground.
const MIN_CONTRAST: f32 = 3.0;
/// White as a background on the light theme, where the status bar `ESC[30;47m` asks for must stay light.
const LIGHT_PAPER: [u32; 2] = [0xD6D6D6, 0xFFFFFF];
const DIM: f32 = 0.66;
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Palette {
    ansi: [Color; 16],
    fg: Color,
    /// What the default background stands for when a colour needs one. It is never painted: the surface shows.
    pub ground: Color,
    cursor: Color,
    pub cursor_text: Color,
    dark: bool,
}

/// A cell's colours and face, resolved.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Style {
    pub fg: Color,
    /// `None` lets the surface show through.
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub hidden: bool,
}

fn from_rgb(c: Rgb) -> Color {
    Color { r: c.r, g: c.g, b: c.b, a: 255 }
}

fn opaque(c: Color, over: Color) -> Color {
    over.mix(Color { a: 255, ..c }, c.a as f32 / 255.0)
}

fn luminance(c: Color) -> f32 {
    let channel = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
}

fn contrast(a: Color, b: Color) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// `fg`, moved towards black or white only as far as it takes to read on `bg`, which keeps its hue where it can.
fn readable(fg: Color, bg: Color) -> Color {
    if contrast(fg, bg) >= MIN_CONTRAST {
        return fg;
    }
    let target = if contrast(Color::BLACK, bg) > contrast(Color::WHITE, bg) { Color::BLACK } else { Color::WHITE };
    (1..10).map(|i| fg.mix(target, i as f32 / 10.0)).find(|c| contrast(*c, bg) >= MIN_CONTRAST).unwrap_or(target)
}

impl Palette {
    pub fn from_theme(t: &Theme) -> Self {
        Self {
            ansi: (if t.dark { DARK } else { LIGHT }).map(Color::rgb),
            fg: t.text_primary,
            ground: t.mica_base,
            cursor: fluent::mark::BRAND_ORANGE,
            cursor_text: Color::rgb(0x14110F),
            dark: t.dark,
        }
    }

    fn base(&self, i: usize) -> Color {
        match i {
            0..16 => self.ansi[i],
            16..232 => {
                let i = i - 16;
                Color { r: CUBE[i / 36], g: CUBE[i / 6 % 6], b: CUBE[i % 6], a: 255 }
            }
            232..256 => {
                let v = (8 + (i - 232) * 10) as u8;
                Color { r: v, g: v, b: v, a: 255 }
            }
            256 | 267 => self.fg,
            257 => self.ground,
            258 => self.cursor,
            259..267 => self.ansi[i - 259].fade(DIM),
            _ => self.fg.fade(DIM),
        }
    }

    /// A slot of the 269 that `Colors` indexes: the program's OSC override, else the theme's.
    pub fn slot(&self, i: usize, over: &Colors) -> Color {
        over[i].map_or_else(|| self.base(i), from_rgb)
    }

    /// The answer to a program asking for a colour with OSC 4, 10, 11 or 12.
    pub fn query(&self, i: usize) -> Rgb {
        let c = opaque(self.base(i.min(268)), self.ground);
        Rgb { r: c.r, g: c.g, b: c.b }
    }

    pub fn foreground(&self, c: Ansi, flags: Flags, over: &Colors) -> Color {
        match c {
            Ansi::Spec(rgb) => from_rgb(rgb),
            Ansi::Indexed(i) => self.slot(i as usize, over),
            // Bold brightens the eight base colours, as xterm.js does by default.
            Ansi::Named(n) if (n as usize) < 8 && flags.contains(Flags::BOLD) => self.slot(n as usize + 8, over),
            Ansi::Named(n) => self.slot(n as usize, over),
        }
    }

    /// `None` for the default, which is the surface itself.
    fn background(&self, c: Ansi, over: &Colors) -> Option<Color> {
        let white = match c {
            Ansi::Named(NamedColor::Background) => return None,
            Ansi::Named(NamedColor::White) | Ansi::Indexed(7) => Some((7, LIGHT_PAPER[0])),
            Ansi::Named(NamedColor::BrightWhite) | Ansi::Indexed(15) => Some((15, LIGHT_PAPER[1])),
            _ => None,
        };
        Some(match white {
            Some((slot, paper)) if !self.dark && over[slot].is_none() => Color::rgb(paper),
            _ => self.foreground(c, Flags::empty(), over),
        })
    }

    pub fn style(&self, cell: &Cell, over: &Colors) -> Style {
        let flags = cell.flags;
        let mut fg = self.foreground(cell.fg, flags, over);
        let mut bg = self.background(cell.bg, over);
        let ground = self.slot(NamedColor::Background as usize, over);
        let inverse = flags.contains(Flags::INVERSE);
        if inverse {
            (fg, bg) = (bg.unwrap_or(ground), Some(opaque(fg, ground)));
        }
        // The theme's own text on the theme's own ground needs no checking, and that is nearly every cell.
        if inverse || bg.is_some() || cell.fg != Ansi::Named(NamedColor::Foreground) {
            fg = readable(fg, bg.unwrap_or(ground));
        }
        if flags.contains(Flags::DIM) {
            fg = fg.fade(DIM);
        }
        Style { fg, bg, bold: flags.contains(Flags::BOLD), italic: flags.contains(Flags::ITALIC), hidden: flags.contains(Flags::HIDDEN) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(fg: Ansi, bg: Ansi, flags: Flags) -> Cell {
        Cell { fg, bg, flags, ..Cell::default() }
    }

    const FG: Ansi = Ansi::Named(NamedColor::Foreground);
    const BG: Ansi = Ansi::Named(NamedColor::Background);

    #[test]
    fn every_colour_reads_on_its_theme() {
        for theme in [Theme::dark(), Theme::light()] {
            let p = Palette::from_theme(&theme);
            // Black on a dark ground is the one pair that cannot read and is not meant to: it is for backgrounds.
            for i in (if theme.dark { 1 } else { 0 })..16 {
                let ratio = contrast(p.ansi[i], p.ground);
                assert!(ratio >= MIN_CONTRAST, "colour {i} has contrast {ratio} (dark: {})", theme.dark);
            }
        }
    }

    #[test]
    fn all_three_colour_depths_resolve() {
        let p = Palette::from_theme(&Theme::dark());
        let over = Colors::default();
        let none = Flags::empty();
        assert_eq!(p.foreground(Ansi::Named(NamedColor::Red), none, &over), Color::rgb(DARK[1]));
        assert_eq!(p.foreground(Ansi::Indexed(1), none, &over), Color::rgb(DARK[1]));
        assert_eq!(p.foreground(Ansi::Indexed(16), none, &over), Color::rgb(0x000000));
        assert_eq!(p.foreground(Ansi::Indexed(196), none, &over), Color::rgb(0xFF0000));
        assert_eq!(p.foreground(Ansi::Indexed(231), none, &over), Color::rgb(0xFFFFFF));
        assert_eq!(p.foreground(Ansi::Indexed(232), none, &over), Color::rgb(0x080808));
        assert_eq!(p.foreground(Ansi::Indexed(255), none, &over), Color::rgb(0xEEEEEE));
        assert_eq!(p.foreground(Ansi::Spec(Rgb { r: 1, g: 2, b: 3 }), none, &over), Color::rgb(0x010203));
        assert_eq!(p.foreground(Ansi::Named(NamedColor::Red), Flags::BOLD, &over), Color::rgb(DARK[9]));
    }

    #[test]
    fn default_colours_follow_the_theme() {
        for theme in [Theme::dark(), Theme::light()] {
            let s = Palette::from_theme(&theme).style(&Cell::default(), &Colors::default());
            assert_eq!(s.fg, theme.text_primary);
            assert_eq!(s.bg, None, "the default background is the surface itself");
        }
    }

    #[test]
    fn inverse_dim_and_contrast() {
        let t = Theme::dark();
        let p = Palette::from_theme(&t);
        let over = Colors::default();
        let s = p.style(&cell(FG, BG, Flags::INVERSE), &over);
        assert_eq!(s.bg, Some(Color::WHITE));
        assert_eq!(s.fg, t.mica_base);
        let dim = p.style(&cell(FG, BG, Flags::DIM), &over);
        assert!(dim.fg.a < t.text_primary.a);
        // A program's near-ground truecolour is lifted until it reads.
        let faint = p.style(&cell(Ansi::Spec(Rgb { r: 0x28, g: 0x28, b: 0x28 }), BG, Flags::empty()), &over);
        assert!(contrast(faint.fg, t.mica_base) >= MIN_CONTRAST);
    }

    #[test]
    fn white_stays_readable_as_text_and_light_as_paper() {
        let p = Palette::from_theme(&Theme::light());
        let over = Colors::default();
        let status = p.style(&cell(Ansi::Named(NamedColor::Black), Ansi::Named(NamedColor::White), Flags::empty()), &over);
        assert_eq!(status.bg, Some(Color::rgb(LIGHT_PAPER[0])));
        assert_eq!(status.fg, Color::rgb(LIGHT[0]));
        let text = p.style(&cell(Ansi::Named(NamedColor::White), BG, Flags::empty()), &over);
        assert_eq!(text.fg, Color::rgb(LIGHT[7]));
    }

    #[test]
    fn program_overrides_win() {
        let p = Palette::from_theme(&Theme::dark());
        let mut over = Colors::default();
        over[1] = Some(Rgb { r: 9, g: 9, b: 9 });
        assert_eq!(p.slot(1, &over), Color::rgb(0x090909));
        assert_eq!(p.query(257), Rgb { r: 0x20, g: 0x20, b: 0x20 });
        assert_eq!(p.query(256), Rgb { r: 255, g: 255, b: 255 });
    }
}
