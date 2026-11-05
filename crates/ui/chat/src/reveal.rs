//! `components/StreamRevealText.tsx`: plain text that is still arriving, with what each commit added sweeping in from the left,
//! the one-line counterpart of the Markdown reveal. When the text is a continuation only the continuation moves; when it is a
//! different line altogether, the whole line is new and sweeps in.

use std::hash::Hash;
use std::time::Instant;

use mewrk_client::prelude::*;
pub use mewrk_client::runs::stream_reveal::fresh_alpha;
use mewrk_client::runs::stream_reveal::STREAM_REVEAL_MS;
use mewrk_client::runs::{next_stream_reveal_split, StreamRevealSplit};

struct Reveal {
    split: Option<StreamRevealSplit>,
    arrived: Instant,
}

/// A line's advance, which unlike the width `Ui::label` and `Ui::measure` report counts its trailing spaces.
pub(crate) fn advance(ui: &mut Ui<'_>, value: &str, ty: &TypeStyle) -> f32 {
    ui.text.line(value, ty, Color::WHITE).advance / ui.scale
}

/// Shaped lines are cached by colour, so a colour that moves a little every frame would shape a new line every frame.
pub(crate) fn quantized(alpha: f32) -> f32 {
    (alpha * 16.0).round() / 16.0
}

/// Paints `value[from..]` a character at a time, each in the colour `shade` gives for its centre, which is how a gradient over text
/// is drawn without a mask. A character starts where the text before it ends, so the words keep their kerning and their spaces.
pub(crate) fn paint_characters(ui: &mut Ui<'_>, x: f32, y: f32, value: &str, from: usize, ty: &TypeStyle, mut shade: impl FnMut(f32) -> Color) {
    for (at, character) in value[from..].char_indices() {
        if character.is_whitespace() {
            continue;
        }
        let (start, end) = (from + at, from + at + character.len_utf8());
        let (left, right) = (advance(ui, &value[..start], ty), advance(ui, &value[..end], ty));
        ui.label(x + left, y, &value[start..end], ty, shade(left + (right - left) * 0.5));
    }
}

pub fn stream_reveal_text(key: impl Hash, value: &str, ty: TypeStyle, color: Color) -> E {
    let state = use_state(("stream-reveal", hash_key(key)), || Reveal { split: None, arrived: Instant::now() });
    let (settled, arrived) = {
        let mut reveal = state.borrow_mut();
        let next = next_stream_reveal_split(reveal.split.as_ref(), value);
        // The same text keeps its split, so a sweep still in flight is not cut short by a frame drawn for some other reason.
        if reveal.split.as_ref() != Some(&next) {
            reveal.arrived = Instant::now();
            reveal.split = Some(next.clone());
        }
        (next.settled, reveal.arrived)
    };
    let (painted, measured) = (value.to_owned(), value.to_owned());
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let y = r.y + (r.h - ty.line) * 0.5;
        let progress = arrived.elapsed().as_secs_f32() * 1000.0 / STREAM_REVEAL_MS as f32;
        // A line too long for its box ends in an ellipsis, past which nothing fresh could be seen anyway. The reveal is decoration, and
        // the text lands fully opaque without it.
        let cut = ui.measure(&painted, &ty) > r.w;
        if cut {
            ui.line_was_cut(r, &painted);
        }
        if progress >= 1.0 || fluent::reduced_motion() || settled >= painted.len() || cut {
            ui.label_ellipsis(r.x, y, r.w, &painted, &ty, color);
            return;
        }
        ui.wake_after(1.0 / 60.0);
        let before = &painted[..settled];
        if !before.is_empty() {
            ui.label(r.x, y, before, &ty, color);
        }
        let start = advance(ui, before, &ty);
        let fresh_width = advance(ui, &painted, &ty) - start;
        paint_characters(ui, r.x, y, &painted, settled, &ty, |centre| color.fade(quantized(fresh_alpha(centre - start, fresh_width, ty.size, progress))));
    })
    .measure(move |_, ui, _, _| (ui.measure(&measured, &ty), ty.line))
    .min_w(0.0)
    .with_style(|s| s.flex_shrink = 1.0)
    .pass_through()
}
