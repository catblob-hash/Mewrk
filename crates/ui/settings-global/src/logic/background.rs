//! `lib/background.ts`: the window's background as saved in `AppearancePreferences.background`.
//!
//! Each theme has a solid ground of its own, and `solid` follows every change of theme with no write. Picking the other
//! theme's ground saves `solid:<scheme>`, kept until the theme next changes. Pictures stay whatever the theme does.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackgroundScheme {
    Day,
    Night,
}

#[derive(Clone, Copy)]
pub struct BuiltinPicture {
    pub id: &'static str,
    pub name: &'static str,
    /// 3840×2400, for the window.
    pub picture: &'static [u8],
    /// 480×300, for the picker and the previews.
    pub thumbnail: &'static [u8],
    /// Where the picture's cat is, as fractions of its size: cropping trims around it.
    pub focus: (f32, f32),
}

pub const BUILTIN_PICTURES: &[BuiltinPicture] = &[
    BuiltinPicture { id: "builtin:desk", name: "desk", picture: include_bytes!("../../assets/backgrounds/desk.jpg"), thumbnail: include_bytes!("../../assets/backgrounds/desk-thumb.jpg"), focus: (0.25, 0.81) },
    BuiltinPicture { id: "builtin:chair", name: "chair", picture: include_bytes!("../../assets/backgrounds/chair.jpg"), thumbnail: include_bytes!("../../assets/backgrounds/chair-thumb.jpg"), focus: (0.90, 0.83) },
    BuiltinPicture { id: "builtin:shelf", name: "shelf", picture: include_bytes!("../../assets/backgrounds/shelf.jpg"), thumbnail: include_bytes!("../../assets/backgrounds/shelf-thumb.jpg"), focus: (0.70, 0.49) },
    BuiltinPicture { id: "builtin:curtain", name: "curtain", picture: include_bytes!("../../assets/backgrounds/curtain.jpg"), thumbnail: include_bytes!("../../assets/backgrounds/curtain-thumb.jpg"), focus: (0.95, 0.70) },
];

pub const THEME_SOLID: &str = "solid";

#[derive(Clone, Copy)]
pub enum Background<'a> {
    /// A theme's ground; `None` for the one the theme on screen owns.
    Solid(Option<BackgroundScheme>),
    Builtin(&'static BuiltinPicture),
    Imported(&'a str),
}

fn is_imported_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn parse_background(value: &str) -> Background<'_> {
    match value {
        "solid:day" => return Background::Solid(Some(BackgroundScheme::Day)),
        "solid:night" => return Background::Solid(Some(BackgroundScheme::Night)),
        _ => {}
    }
    if let Some(b) = BUILTIN_PICTURES.iter().find(|p| p.id == value) {
        return Background::Builtin(b);
    }
    if is_imported_id(value) {
        return Background::Imported(value);
    }
    Background::Solid(None)
}

/// What picking one theme's ground saves while `theme` is on screen.
pub fn solid_background(scheme: BackgroundScheme, theme: BackgroundScheme) -> String {
    if scheme == theme {
        THEME_SOLID.to_owned()
    } else {
        format!("solid:{}", if scheme == BackgroundScheme::Day { "day" } else { "night" })
    }
}

/// The background after the theme on screen changes: a solid ground goes to the new theme's.
pub fn background_after_theme_change(value: &str) -> String {
    if matches!(parse_background(value), Background::Solid(_)) { THEME_SOLID.to_owned() } else { value.to_owned() }
}

/// Whether the window's ground is something other than the theme's own.
pub fn replaces_theme_ground(value: &str) -> bool {
    !matches!(parse_background(value), Background::Solid(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use BackgroundScheme::*;

    #[test]
    fn unknown_values_read_as_the_themes_own_ground() {
        assert!(matches!(parse_background("solid"), Background::Solid(None)));
        assert!(matches!(parse_background("nonsense"), Background::Solid(None)));
        assert!(matches!(parse_background("solid:night"), Background::Solid(Some(Night))));
        assert!(matches!(parse_background("builtin:desk"), Background::Builtin(_)));
        assert!(matches!(parse_background(&"a".repeat(64)), Background::Imported(_)));
        assert!(matches!(parse_background(&"A".repeat(64)), Background::Solid(None)));
    }

    #[test]
    fn a_solid_ground_follows_the_theme_and_a_picture_stays() {
        assert_eq!(solid_background(Day, Day), "solid");
        assert_eq!(solid_background(Night, Day), "solid:night");
        assert_eq!(background_after_theme_change("solid:night"), "solid");
        assert_eq!(background_after_theme_change("builtin:chair"), "builtin:chair");
        assert!(replaces_theme_ground("solid:day"));
        assert!(replaces_theme_ground("builtin:chair"));
        assert!(!replaces_theme_ground("solid"));
    }
}
