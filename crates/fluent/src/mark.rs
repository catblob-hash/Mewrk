//! The brand's mark, which is also what a terminal's block cursor draws: one cell of text whose two top corners rise into rounded
//! ears. The application icon's drawings carry `MARK_PATH` as they are.

use crate::theme::Color;

pub const MARK_WIDTH: f32 = 104.0;
pub const MARK_HEIGHT: f32 = 132.0;
/// `cell_path(MARK_WIDTH, MARK_HEIGHT)`: the mark at its own proportions, as the icon and the status lights draw it.
pub const MARK_PATH: &str = "M0 110V14A14 14 0 0 1 24.2 4.41L39.61 20.81A17 17 0 0 0 64.39 20.81L79.8 4.41A14 14 0 0 1 104 14V110A22 22 0 0 1 82 132H22A22 22 0 0 1 0 110Z";
/// The brand's orange: the middle of the icon's ground, where the mark stands on the page without its plate.
pub const BRAND_ORANGE: Color = Color::rgb(0xFA7A18);

/// The mark cut to a box of any shape, in the box's own units. The text a terminal's cursor stands level with has proportions of its
/// own, and stretching the mark to them would squash its round ears into slanted ones.
pub fn cell_path(width: f32, height: f32) -> String {
    let (w, h) = (f64::from(width), f64::from(height));
    let (tip, valley, corner) = (w * 14.0 / 104.0, w * 17.0 / 104.0, w * 22.0 / 104.0);
    let dip = h * 34.0 / 132.0;
    // The slope's angle from the outer edge, and how far from the sharp corner the round tip starts. That corner sits above the
    // cell by however much the rounding takes off, so that the tip itself touches y=0, and moving it changes the slope.
    let (mut angle, mut reach, mut apex) = (0.0_f64, 0.0_f64, 0.0_f64);
    for _ in 0..40 {
        angle = (w / 2.0).atan2(dip - apex);
        reach = tip / (angle / 2.0).tan();
        apex = tip - reach;
    }
    let (sin, cos) = angle.sin_cos();
    let tip_end = (sin * reach, apex + cos * reach);
    let back = valley / angle.tan();
    let valley_start = (w / 2.0 - sin * back, dip - cos * back);
    let valley_end = (w / 2.0 + sin * back, valley_start.1);
    format!(
        "M0 {}V{}A{t} {t} 0 0 1 {} {}L{} {}A{v} {v} 0 0 0 {} {}L{} {}A{t} {t} 0 0 1 {} {}V{}A{c} {c} 0 0 1 {} {}H{c}A{c} {c} 0 0 1 0 {}Z",
        n(h - corner),
        n(apex + reach),
        n(tip_end.0),
        n(tip_end.1),
        n(valley_start.0),
        n(valley_start.1),
        n(valley_end.0),
        n(valley_end.1),
        n(w - tip_end.0),
        n(tip_end.1),
        n(w),
        n(apex + reach),
        n(h - corner),
        n(w - corner),
        n(h),
        n(h - corner),
        t = n(tip),
        v = n(valley),
        c = n(corner),
    )
}

/// Two decimals with no trailing zeros, which is what the drawings' path is written in.
fn n(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cut_at_the_marks_own_proportions_is_the_mark() {
        assert_eq!(cell_path(MARK_WIDTH, MARK_HEIGHT), MARK_PATH);
    }
}
