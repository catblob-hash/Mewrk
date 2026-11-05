//! `<LoaderCircle className="spin" />`.

use mewrk_client::prelude::*;

/// Asks for frames only while it is painted, so a hidden spinner costs nothing.
pub fn spinner(size: f32, color: Option<Color>) -> E {
    custom(move |_, ui: &mut Ui<'_>, r: Rect| {
        let c = color.unwrap_or(ui.theme.text_secondary);
        ui.named_icon("LoaderCircle", r.x + r.w * 0.5, r.y + r.h * 0.5, size, c, (ui.time * 360.0) % 360.0);
        ui.animating = true;
    })
    .size(size, size)
    .shrink0()
}
