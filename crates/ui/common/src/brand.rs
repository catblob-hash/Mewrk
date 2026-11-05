//! The application icon and the mark in it. `scripts/make-icon.sh` renders the Dock's `icon.icns` from these same drawings, and the
//! menu bar's picture, the status lights and the terminal's cursor draw this same mark: two drawings of one cat is how two different
//! cats happen.

use std::sync::LazyLock;

pub use fluent::mark::{BRAND_ORANGE, MARK_HEIGHT, MARK_PATH, MARK_WIDTH};
use mewrk_client::prelude::*;

pub const ICON_SVG: &str = include_str!("../assets/mewrk-icon.svg");
pub const ICON_SMALL_SVG: &str = include_str!("../assets/mewrk-icon-small.svg");
pub const MARK_SVG: &str = include_str!("../assets/mewrk-mark.svg");

/// The prompt and where the mark stands beside it, in the drawing's own coordinates, and the part of the drawing that holds both.
pub const PROMPT_PATH: &str = "M272 459L394 551L272 643";
pub const MARK_TRANSFORM: &str = "translate(490 319) scale(2.8)";
const BARE_VIEWBOX: &str = "192 192 640 640";

/// The drawing cut to its plate: the rest of its canvas is the margin and the shadow macOS wants around an application icon.
static PLATE_SVG: LazyLock<String> = LazyLock::new(|| ICON_SVG.replacen(r#"viewBox="0 0 1024 1024" width="1024" height="1024""#, r#"viewBox="100 100 824 824" width="824" height="824""#, 1));

/// `MewrkIcon`: the application icon as the Dock shows it, filling a square of `size`. It keeps its own colours on either theme.
pub fn mewrk_icon(size: f32) -> E {
    custom::<App>(move |_, ui, r| ui.svg_icon("mewrk-icon", &PLATE_SVG, r.x + r.w * 0.5, r.y + r.h * 0.5, r.w.max(r.h))).size(size, size).shrink0().pass_through()
}

/// `MewrkIcon plate={false}`: the prompt in the page's colour and the mark in the brand's orange, so both follow day and night.
pub fn mewrk_icon_bare(size: f32) -> E {
    let Color { r, g, b, .. } = theme().text_primary;
    let prompt = format!("#{r:02x}{g:02x}{b:02x}");
    let Color { r, g, b, .. } = BRAND_ORANGE;
    let svg = format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{BARE_VIEWBOX}"><path d="{PROMPT_PATH}" fill="none" stroke="{prompt}" stroke-width="60" stroke-linecap="round" stroke-linejoin="round"/><path d="{MARK_PATH}" transform="{MARK_TRANSFORM}" fill="#{r:02x}{g:02x}{b:02x}"/></svg>"##);
    let key = format!("mewrk-icon-bare:{prompt}");
    custom::<App>(move |_, ui, rect| ui.svg_icon(&key, &svg, rect.x + rect.w * 0.5, rect.y + rect.h * 0.5, rect.w.max(rect.h))).size(size, size).shrink0().pass_through()
}
