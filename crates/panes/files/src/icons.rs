//! Which icon stands for a file (upstream's `lib/fileIcons.ts` and `FileIcon.tsx`). Icons and the rules that choose
//! between them are Material Icon Theme's; `scripts/gen_file_icons.py` distils its manifest into `icon_table.rs`.

use crate::icon_table::{EXTENSIONS, FALLBACK, ICONS, LIGHT, NAMES, NAME_DEPTH};
use fluent::el::{custom, theme};
use mewrk_client::prelude::E;

/// Types the theme has no rule for, drawn with the icon it gives their nearest kin. Only consulted once the theme's own
/// tables have missed.
pub const KINDRED_ICONS: &[(&str, &[&str])] = &[
    ("video", &["3gp", "3g2", "f4v", "rmvb", "mxf"]),
    ("audio", &["midi", "amr", "ape", "ac3", "dsf", "caf"]),
    ("image", &["hdr", "jxl", "jp2", "pcx", "arw", "orf", "rw2"]),
    ("3d", &["usda", "usdc", "x3d", "max", "ma", "mb", "c4d", "splat", "ifc", "vrm", "lwo"]),
    ("zip", &["war", "ear", "asar", "lz", "lz4", "lzma", "cab", "cpio"]),
    ("database", &["avro", "mdb", "accdb", "duckdb", "feather", "arrow"]),
    ("table", &["numbers", "xlsm", "xlsb"]),
    ("word", &["pages", "wps"]),
    ("epub", &["mobi", "azw", "azw3", "fb2"]),
    ("key", &["p12", "pfx", "jks", "keystore"]),
    ("dll", &["dylib"]),
];

/// Extension of a file name, lowercased. A dotfile with no second dot is all name and no extension.
pub fn file_extension(name: &str) -> String {
    match name.rfind('.') {
        Some(at) if at > 0 => name[at + 1..].to_lowercase(),
        _ => String::new(),
    }
}

fn lookup(table: &'static [(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table.binary_search_by(|(k, _)| k.as_bytes().cmp(key.as_bytes())).ok().map(|i| table[i].1)
}

/// Name of the icon for a path, by the theme's own precedence: a whole file name outranks any extension and may be
/// qualified by the directories above it; among extensions the longest wins.
pub fn file_icon_name(path: &str) -> &'static str {
    let lower = path.to_lowercase();
    let segments: Vec<&str> = lower.split(['\\', '/']).filter(|s| !s.is_empty()).collect();
    for depth in (1..=NAME_DEPTH.min(segments.len())).rev() {
        if let Some(icon) = lookup(NAMES, &segments[segments.len() - depth..].join("/")) {
            return icon;
        }
    }
    let name = segments.last().copied().unwrap_or("");
    for (dot, _) in name.match_indices('.') {
        if let Some(icon) = lookup(EXTENSIONS, &name[dot + 1..]) {
            return icon;
        }
    }
    let extension = file_extension(name);
    KINDRED_ICONS.iter().find(|(_, list)| list.contains(&extension.as_str())).map_or(FALLBACK, |(icon, _)| icon)
}

/// The same icon redrawn for a light background, for the few whose usual drawing is too pale to read on one.
pub fn light_icon_name(icon: &str) -> Option<String> {
    LIGHT.binary_search(&icon).is_ok().then(|| format!("{icon}_light"))
}

/// The drawing to use on the current ground.
pub fn icon_svg(icon: &str, dark: bool) -> Option<(String, &'static str)> {
    let name = if dark { icon.to_owned() } else { light_icon_name(icon).unwrap_or_else(|| icon.to_owned()) };
    let i = ICONS.binary_search_by(|(k, _)| k.as_bytes().cmp(name.as_bytes())).ok()?;
    Some((name, ICONS[i].1))
}

/// The icon of a file's type, chosen from its name. Decoration: it says nothing the name beside it does not.
pub fn file_icon(path: &str, size: f32) -> E {
    let icon = file_icon_name(path);
    let dark = theme().dark;
    custom(move |_app, ui, r| {
        if let Some((name, svg)) = icon_svg(icon, dark) {
            ui.svg_icon(&name, svg, r.x + r.w * 0.5, r.y + r.h * 0.5, size);
        }
    })
    .size(size, size)
    .shrink0()
    .pass_through()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_the_icon_of_the_files_type() {
        assert_eq!(file_icon_name("src-tauri/src/lib.rs"), "rust");
        assert_eq!(file_icon_name("notes.md"), "markdown");
        assert_eq!(file_icon_name("no-such-type.qqq"), FALLBACK);
    }

    #[test]
    fn a_whole_name_outranks_an_extension_and_the_longest_extension_wins() {
        assert_eq!(file_icon_name("Cargo.toml"), "toml");
        assert_eq!(file_icon_name("index.d.ts"), "typescript-def");
        assert_eq!(file_icon_name("a.ts"), "typescript");
    }

    #[test]
    fn falls_back_to_kindred_types_only_when_the_theme_has_no_rule() {
        assert_eq!(file_icon_name("a.azw3"), "epub");
        assert_eq!(file_icon_name("a.dylib"), "dll");
    }

    #[test]
    fn has_a_second_drawing_only_for_those_too_pale_on_a_light_ground() {
        assert_eq!(light_icon_name("toml").as_deref(), Some("toml_light"));
        assert_eq!(light_icon_name("rust"), None);
        assert_eq!(icon_svg("toml", false).unwrap().0, "toml_light");
        assert_eq!(icon_svg("toml", true).unwrap().0, "toml");
    }

    #[test]
    fn dotfile_has_no_extension_of_its_own() {
        assert_eq!(file_extension(".gitignore"), "");
        assert_eq!(file_extension("a.TAR.GZ"), "gz");
    }

    #[test]
    fn every_icon_the_tables_name_is_embedded_and_parses() {
        for (_, icon) in NAMES.iter().chain(EXTENSIONS) {
            assert!(ICONS.binary_search_by(|(k, _)| k.cmp(icon)).is_ok(), "{icon}");
        }
        for (name, svg) in ICONS {
            assert!(resvg::usvg::Tree::from_str(svg, &Default::default()).is_ok(), "{name}");
        }
    }
}
