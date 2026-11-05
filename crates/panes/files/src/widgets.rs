//! Small pieces several of this crate's views share: upstream's `IconButton` and a spinner. Menus, `PathText` and Markdown are
//! `mewrk_ui_common`'s, and the pane's frame is the shell's `side_pane`.

use mewrk_client::prelude::*;

/// `IconButton` at a given box size. `name` is the Lucide name upstream imports.
pub fn tool(key: impl std::hash::Hash, name: &'static str, tip: impl Into<Arc<str>>, side: f32, glyph: f32) -> E {
    let t_ = theme();
    row().key(key).center().size(side, side).shrink0().radius(5.0).hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).tooltip(tip).child(icon(name, glyph).color(t_.text_secondary))
}

/// The same, shown pressed: `aria-pressed`.
pub fn tool_pressed(button: E, pressed: bool) -> E {
    let button = button.aria_pressed(pressed);
    if pressed { button.bg(theme().subtle_pressed) } else { button }
}

/// A turning `LoaderCircle`, which asks for its own frames.
pub fn spinner(size: f32) -> E {
    let color = theme().text_tertiary;
    custom(move |_app, ui, r| {
        let step = (ui.time * 10.0) as i32 % 12;
        ui.named_icon("LoaderCircle", r.x + r.w * 0.5, r.y + r.h * 0.5, size, color, step as f32 * 30.0);
        ui.wake_after(0.08);
    })
    .size(size, size)
    .shrink0()
    .pass_through()
}

pub type Action = Rc<dyn Fn(&mut App)>;
