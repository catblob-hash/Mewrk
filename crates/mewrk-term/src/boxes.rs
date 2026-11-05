//! Line-drawing and block characters, drawn as rects. A font's glyphs stop short of a line box taller than the
//! font, so its frames come out broken, and these characters exist to join up.

use std::f32::consts::{FRAC_PI_2, PI};

use fluent::{rect, Color, Ui};

const LIGHT: f32 = 1.0;
const HEAVY: f32 = 2.0;
/// Left, right, up and down arms: 1 for light and 2 for heavy. Mixed weights, dashes and doubles stay with the font.
fn arms(c: char) -> Option<[u8; 4]> {
    Some(match c {
        '─' => [1, 1, 0, 0],
        '│' => [0, 0, 1, 1],
        '┌' => [0, 1, 0, 1],
        '┐' => [1, 0, 0, 1],
        '└' => [0, 1, 1, 0],
        '┘' => [1, 0, 1, 0],
        '├' => [0, 1, 1, 1],
        '┤' => [1, 0, 1, 1],
        '┬' => [1, 1, 0, 1],
        '┴' => [1, 1, 1, 0],
        '┼' => [1, 1, 1, 1],
        '╴' => [1, 0, 0, 0],
        '╶' => [0, 1, 0, 0],
        '╵' => [0, 0, 1, 0],
        '╷' => [0, 0, 0, 1],
        '━' => [2, 2, 0, 0],
        '┃' => [0, 0, 2, 2],
        '┏' => [0, 2, 0, 2],
        '┓' => [2, 0, 0, 2],
        '┗' => [0, 2, 2, 0],
        '┛' => [2, 0, 2, 0],
        '┣' => [0, 2, 2, 2],
        '┫' => [2, 0, 2, 2],
        '┳' => [2, 2, 0, 2],
        '┻' => [2, 2, 2, 0],
        '╋' => [2, 2, 2, 2],
        _ => return None,
    })
}

/// A rounded corner: which way its centre lies from the cell's, and where its quarter turn starts.
fn curve(c: char) -> Option<(f32, f32, f32)> {
    Some(match c {
        '╭' => (1.0, 1.0, PI * 1.5),
        '╮' => (-1.0, 1.0, 0.0),
        '╯' => (-1.0, -1.0, PI * 0.5),
        '╰' => (1.0, -1.0, PI),
        _ => return None,
    })
}

/// One rectangle as fractions of the cell, and how solid: left, top, right, bottom, opacity.
fn block(c: char) -> Option<[f32; 5]> {
    let eighth = |n: u32| n as f32 / 8.0;
    Some(match c {
        '█' => [0.0, 0.0, 1.0, 1.0, 1.0],
        '▀' => [0.0, 0.0, 1.0, 0.5, 1.0],
        '▐' => [0.5, 0.0, 1.0, 1.0, 1.0],
        '▔' => [0.0, 0.0, 1.0, 0.125, 1.0],
        '▕' => [0.875, 0.0, 1.0, 1.0, 1.0],
        '░' => [0.0, 0.0, 1.0, 1.0, 0.25],
        '▒' => [0.0, 0.0, 1.0, 1.0, 0.5],
        '▓' => [0.0, 0.0, 1.0, 1.0, 0.75],
        '▁'..='▇' => [0.0, 1.0 - eighth(c as u32 - 0x2580), 1.0, 1.0, 1.0],
        '▉'..='▏' => [0.0, 0.0, eighth(0x2590 - c as u32), 1.0, 1.0],
        _ => return None,
    })
}

/// The four quadrants of a cell that a character fills: 1 top left, 2 top right, 4 bottom left, 8 bottom right.
fn quadrants(c: char) -> Option<u8> {
    Some(match c {
        '▖' => 4,
        '▗' => 8,
        '▘' => 1,
        '▝' => 2,
        '▚' => 9,
        '▞' => 6,
        '▙' => 13,
        '▛' => 7,
        '▜' => 11,
        '▟' => 14,
        _ => return None,
    })
}

pub fn is_drawn(c: char) -> bool {
    matches!(c, '\u{2500}'..='\u{259F}') && (arms(c).is_some() || curve(c).is_some() || block(c).is_some() || quadrants(c).is_some())
}

/// A stroke width in logical pixels that is a whole number of physical ones, so that every line comes out alike.
fn stroke(ui: &Ui<'_>, weight: f32) -> f32 {
    (weight * ui.scale).round().max(1.0) / ui.scale
}

/// The centre of a stroke, on a physical pixel boundary that keeps its edges sharp.
fn snap(ui: &Ui<'_>, v: f32, width: f32) -> f32 {
    ((v - width * 0.5) * ui.scale).round() / ui.scale + width * 0.5
}

fn paint_lines(ui: &mut Ui<'_>, [l, r, u, d]: [u8; 4], x: f32, y: f32, cw: f32, ch: f32, color: Color) {
    let weight = |a: u8, b: u8| if a.max(b) == 2 { HEAVY } else { LIGHT };
    let (across, along) = (stroke(ui, weight(l, r)), stroke(ui, weight(u, d)));
    let (mx, my) = (snap(ui, x + cw * 0.5, along), snap(ui, y + ch * 0.5, across));
    let level = l != 0 || r != 0;
    // The arms must not overlap: text colours can be translucent, and a joint would show darker.
    if level {
        let left = if l != 0 { x } else { mx - along * 0.5 };
        let right = if r != 0 { x + cw } else { mx + along * 0.5 };
        ui.fill(rect(left, my - across * 0.5, right - left, across), 0.0, color);
    }
    let (above, below) = if level { (my - across * 0.5, my + across * 0.5) } else { (my, my) };
    if u != 0 && d != 0 && !level {
        ui.fill(rect(mx - along * 0.5, y, along, ch), 0.0, color);
        return;
    }
    if u != 0 {
        ui.fill(rect(mx - along * 0.5, y, along, above - y), 0.0, color);
    }
    if d != 0 {
        ui.fill(rect(mx - along * 0.5, below, along, y + ch - below), 0.0, color);
    }
}

fn paint_curve(ui: &mut Ui<'_>, (dx, dy, start): (f32, f32, f32), x: f32, y: f32, cw: f32, ch: f32, color: Color) {
    let width = stroke(ui, LIGHT);
    let (mx, my) = (snap(ui, x + cw * 0.5, width), snap(ui, y + ch * 0.5, width));
    let radius = cw * 0.5;
    ui.arc(mx + dx * radius, my + dy * radius, radius, width, start, FRAC_PI_2, color);
    // The rest of the way to the cell's edge. Sideways the curve already got there.
    let from = my + dy * radius;
    let to = if dy > 0.0 { y + ch } else { y };
    ui.fill(rect(mx - width * 0.5, from.min(to), width, (to - from).abs()), 0.0, color);
    let edge = if dx > 0.0 { x + cw } else { x };
    let side = mx + dx * radius;
    if (edge - side).abs() > 0.0 {
        ui.fill(rect(side.min(edge), my - width * 0.5, (edge - side).abs(), width), 0.0, color);
    }
}

/// Draws a character `is_drawn` accepted into its cell.
pub fn paint(ui: &mut Ui<'_>, c: char, x: f32, y: f32, cw: f32, ch: f32, color: Color) {
    if let Some(arms) = arms(c) {
        paint_lines(ui, arms, x, y, cw, ch, color);
    } else if let Some(turn) = curve(c) {
        paint_curve(ui, turn, x, y, cw, ch, color);
    } else if let Some([x0, y0, x1, y1, solid]) = block(c) {
        ui.fill(rect(x + x0 * cw, y + y0 * ch, (x1 - x0) * cw, (y1 - y0) * ch), 0.0, color.fade(solid));
    } else if let Some(mask) = quadrants(c) {
        let (w, h) = (cw * 0.5, ch * 0.5);
        for (bit, qx, qy) in [(1, 0.0, 0.0), (2, w, 0.0), (4, 0.0, h), (8, w, h)] {
            if mask & bit != 0 {
                ui.fill(rect(x + qx, y + qy, w, h), 0.0, color);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use fluent::ui::{Input, NoClipboard, UiMemory};
    use fluent::{Scene, TextCtx, Theme};

    use super::*;

    /// What `c` draws in a 7×17 cell at (10, 20), as physical pixel rects `[x, y, w, h]`.
    fn drawn(c: char) -> Vec<[f32; 4]> {
        let (mut scene, mut text, input, mut mem) = (Scene::default(), TextCtx::new(2.0), Input::default(), UiMemory::default());
        let mut clip = NoClipboard;
        let mut ui = Ui::new(&mut scene, &mut text, Theme::dark(), &input, &mut mem, &mut clip, 2.0, (100.0, 100.0), 0.01, 0.0);
        paint(&mut ui, c, 10.0, 20.0, 7.0, 17.0, Color::WHITE);
        scene.layers[0].rects.iter().map(|r| r.rect).collect()
    }

    #[test]
    fn every_character_in_the_range_that_is_claimed_gets_drawn() {
        for c in '\u{2500}'..='\u{259F}' {
            if is_drawn(c) {
                assert!(!drawn(c).is_empty(), "{c} is claimed and draws nothing");
            }
        }
        assert!(!is_drawn('a') && !is_drawn('═') && !is_drawn('┄'), "the rest stays with the font");
    }

    #[test]
    fn lines_reach_the_edges_of_the_cell_so_that_they_join() {
        assert_eq!(drawn('─'), [[20.0, 56.0, 14.0, 2.0]]);
        assert_eq!(drawn('│'), [[26.0, 40.0, 2.0, 34.0]]);
        let heavy = drawn('━');
        assert_eq!((heavy[0][0], heavy[0][2], heavy[0][3]), (20.0, 14.0, 4.0));
    }

    #[test]
    fn arms_of_a_cross_do_not_overlap() {
        let parts = drawn('┼');
        assert_eq!(parts.len(), 3);
        let [across, up, down] = [parts[0], parts[1], parts[2]];
        assert_eq!(up[1] + up[3], across[1]);
        assert_eq!(across[1] + across[3], down[1]);
        assert_eq!(up[1], 40.0);
        assert_eq!(down[1] + down[3], 74.0);
    }

    #[test]
    fn corners_stop_at_the_stroke_they_meet() {
        let parts = drawn('┌');
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0][0] + parts[0][2], 34.0, "reaches the right edge");
        assert_eq!(parts[1][1] + parts[1][3], 74.0, "reaches the bottom edge");
        assert_eq!(parts[1][1], parts[0][1] + parts[0][3]);
    }

    #[test]
    fn rounded_corners_reach_their_two_edges() {
        for c in ['╭', '╮', '╯', '╰'] {
            let parts = drawn(c);
            assert!(parts.len() >= 2, "{c}: a curve and a stub");
            let stub = parts[1];
            let (top, bottom) = (stub[1], stub[1] + stub[3]);
            assert!(top == 40.0 || bottom == 74.0, "{c} stops short: {stub:?}");
        }
    }

    #[test]
    fn blocks_fill_their_fractions() {
        assert_eq!(drawn('█'), [[20.0, 40.0, 14.0, 34.0]]);
        assert_eq!(drawn('▀'), [[20.0, 40.0, 14.0, 17.0]]);
        assert_eq!(drawn('▄'), [[20.0, 57.0, 14.0, 17.0]]);
        assert_eq!(drawn('▌'), [[20.0, 40.0, 7.0, 34.0]]);
        assert_eq!(drawn('▐'), [[27.0, 40.0, 7.0, 34.0]]);
        assert_eq!(drawn('▁')[0][1] + drawn('▁')[0][3], 74.0);
        assert_eq!(drawn('▙').len(), 3);
    }
}
