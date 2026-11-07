//! The cat loafing on the top rim of a draft composer, with its laptop: upstream's `ComposerCat.tsx`.
//!
//! This is the mascot's own drawing (`art.rs`), not the brand mark. The composer's own top border is the desk, so the cat is lowered
//! onto the border line: that is what sells the composer as the thing the cat is lying on rather than something it floats above.
//! The cat and the laptop are one path because they overlap, and under `fill-rule="evenodd"` the overlap cancels; that notch is the
//! gap that reads as the far paw resting on the keyboard. The figure breathes and the tail sways.

use crate::art;
use mewrk_client::prelude::*;

/// `.composer-cat`'s box, which keeps the drawing's aspect so the ledge stays where it is computed to be.
const WIDTH: f32 = 57.84;
const HEIGHT: f32 = 29.51;
/// How far above the composer's padding box the box starts: this lays the ledge half-way down the top border.
const TOP: f32 = -29.04;
/// Clears the composer-context row's tail rather than hugging the corner.
const RIGHT: f32 = 92.0;
/// Frames of the sway the atlas keeps. Each is drawn once.
const FRAMES: usize = 16;
const TAIL_SECONDS: f32 = 5.2;

/// `composer-cat-tail` as a function of its phase: 0 at the ends, 6° at 30%, -5° at 70%, eased between.
pub fn tail_angle(phase: f32) -> f32 {
    let ease = |t: f32| t * t * (3.0 - 2.0 * t);
    let phase = phase.rem_euclid(1.0);
    if phase < 0.3 {
        6.0 * ease(phase / 0.3)
    } else if phase < 0.7 {
        6.0 + (-5.0 - 6.0) * ease((phase - 0.3) / 0.4)
    } else {
        -5.0 * (1.0 - ease((phase - 0.7) / 0.3))
    }
}

/// `composer-cat-breathe` on the same clock: up by 3.5% about the ledge and back.
fn breath_scale(phase: f32) -> f32 {
    1.0 + 0.035 * (0.5 - 0.5 * (phase * std::f32::consts::TAU).cos())
}

fn frame_svg(frame: usize, color: &str, opacity: f32) -> String {
    let phase = frame as f32 / FRAMES as f32;
    let (left, top, width, _) = art::CAT_COMPOSER_VIEWBOX;
    let (root_x, root_y) = art::CAT_TAIL_RAISED_ROOT;
    let scale = breath_scale(phase);
    let (ledge_x, ledge_y) = (653.7, art::CAT_LEDGE_Y);
    // Square, because the atlas keeps squares: the drawing sits at the top of it and the rest is empty.
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{width}\" viewBox=\"{left} {top} {width} {width}\"><g fill=\"{color}\" fill-opacity=\"{opacity}\" transform=\"translate({ledge_x} {ledge_y}) scale(1 {scale}) translate(-{ledge_x} -{ledge_y})\"><g transform=\"{}\"><path transform=\"rotate({} {root_x} {root_y})\" d=\"{}\"/><path fill-rule=\"evenodd\" d=\"{}{}{}\"/></g></g></svg>",
        art::CAT_ON_LEDGE,
        tail_angle(phase),
        art::CAT_TAIL_RAISED,
        art::CAT_COMPOSER_BODY,
        art::CAT_LAPTOP,
        art::CAT_FACE
    )
}

/// The cat, positioned against the composer's box. It takes no pointer: it must not cover the chips under it.
pub fn composer_cat() -> E {
    let t_ = theme();
    let (color, opacity) = crate::landing::svg_paint(t_.text_tertiary);
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let still = fluent::reduced_motion();
        let frame = if still { 0 } else { ((ui.time / TAIL_SECONDS).rem_euclid(1.0) * FRAMES as f32) as usize % FRAMES };
        let key = format!("composer-cat:{color}:{opacity}:{frame}");
        let svg = frame_svg(frame, &color, opacity);
        ui.svg_icon(&key, &svg, r.x + WIDTH * 0.5, r.y + WIDTH * 0.5, WIDTH);
        // Only its painting changes, so a frame that paints the last tree again is enough for the next step. It steps rather than glides,
        // one picture of the tail per step.
        if !still {
            ui.repaint_after(TAIL_SECONDS / FRAMES as f32);
        }
    })
    .key("composer-cat")
    .absolute()
    .top(TOP)
    .right(RIGHT)
    .size(WIDTH, HEIGHT)
    .pass_through()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_swings_about_its_root_no_further_than_its_geometry_is_checked() {
        const SWING_CHECKED: f32 = 12.0;
        for step in 0..=100 {
            assert!(tail_angle(step as f32 / 100.0).abs() <= SWING_CHECKED);
        }
        assert_eq!(tail_angle(0.0), 0.0);
        assert!((tail_angle(0.3) - 6.0).abs() < 1e-3);
        assert!((tail_angle(0.7) + 5.0).abs() < 1e-3);
        assert!(tail_angle(1.0).abs() < 1e-3);
    }

    #[test]
    fn keeps_the_cat_its_size_and_lays_the_ledge_on_the_composers_border() {
        let (_, top, width, height) = art::CAT_COMPOSER_VIEWBOX;
        // Off the scene's aspect the drawing letterboxes and the ledge slides.
        assert!((WIDTH / HEIGHT - width / height).abs() < 0.01);
        // The scale the cat had in the streaming scene's box at 45px tall.
        assert!((HEIGHT / height - 45.0 / 610.0).abs() < 1e-3);
        // `top` is from the padding box, 1px inside the border's outer edge; half-way down the border is half a pixel above that.
        let ledge = TOP + HEIGHT * (art::CAT_LEDGE_Y - top) / height;
        assert!((ledge + 0.5).abs() < 0.1, "{ledge}");
    }

    #[test]
    fn every_frame_is_a_drawing_the_rasterizer_takes() {
        for frame in 0..FRAMES {
            assert!(frame_svg(frame, "#808080", 0.5).contains("fill-rule=\"evenodd\""));
        }
    }
}
