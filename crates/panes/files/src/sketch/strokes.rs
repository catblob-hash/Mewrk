//! Strokes and how they lie (upstream's `Sketch/strokes.ts`, `history.ts`, `useSketch.ts`'s shortcuts).

use serde::{Deserialize, Serialize};
use std::f32::consts::FRAC_PI_4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SketchTool {
    Pen,
    Line,
    Arrow,
    Rect,
    Ellipse,
    Text,
}

/// Palette order. The first entry is the tool a fresh surface starts on.
pub const SKETCH_TOOLS: [SketchTool; 6] = [SketchTool::Pen, SketchTool::Line, SketchTool::Arrow, SketchTool::Rect, SketchTool::Ellipse, SketchTool::Text];
/// Red, blue, green, black. There is no custom colour picker.
pub const SKETCH_COLORS: [u32; 4] = [0xE03131, 0x1971C2, 0x2F9E44, 0x1F1E1D];
pub const SKETCH_STROKE_WIDTH: f32 = 4.0;
pub const SKETCH_TEXT_SIZE: f32 = 16.0;

#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Line,
    Arrow,
    Rect,
    Ellipse,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stroke {
    Freehand { points: Vec<Point>, color: u32, width: f32 },
    Shape { kind: Shape, from: Point, to: Point, color: u32, width: f32 },
    Text { at: Point, text: String, size: f32, color: u32 },
}

/// Shift-constrain: 45° increments for line and arrow, a square for rect and ellipse.
pub fn constrain_point(kind: Option<Shape>, origin: Point, point: Point, constrained: bool) -> Point {
    let Some(kind) = kind.filter(|_| constrained) else { return point };
    let (dx, dy) = (point.x - origin.x, point.y - origin.y);
    match kind {
        Shape::Line | Shape::Arrow => {
            let length = dx.hypot(dy);
            let angle = (dy.atan2(dx) / FRAC_PI_4).round() * FRAC_PI_4;
            Point { x: origin.x + length * angle.cos(), y: origin.y + length * angle.sin() }
        }
        Shape::Rect | Shape::Ellipse => {
            let side = dx.abs().max(dy.abs());
            // A zero delta counts as positive, so a square never collapses.
            let sign = |v: f32| if v < 0.0 { -1.0 } else { 1.0 };
            Point { x: origin.x + side * sign(dx), y: origin.y + side * sign(dy) }
        }
    }
}

/// The arrow's shaft end and head triangle, from the stroke width.
pub fn arrow_geometry(from: Point, to: Point, width: f32) -> (Point, [Point; 3]) {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let length = dx.hypot(dy).max(if dx == 0.0 && dy == 0.0 { 1.0 } else { 0.0 });
    let (ux, uy) = (dx / length, dy / length);
    let head = (width * 4.0).min(length);
    let cut = (head * 0.7).min(length);
    (
        Point { x: to.x - cut * ux, y: to.y - cut * uy },
        [
            to,
            Point { x: to.x - head * ux + head * 0.5 * uy, y: to.y - head * uy - head * 0.5 * ux },
            Point { x: to.x - head * ux - head * 0.5 * uy, y: to.y - head * uy + head * 0.5 * ux },
        ],
    )
}

pub const SKETCH_HISTORY_LIMIT: usize = 100;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SketchHistory {
    pub past: Vec<Vec<Stroke>>,
    pub present: Vec<Stroke>,
    pub future: Vec<Vec<Stroke>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryState {
    pub count: usize,
    pub can_undo: bool,
    pub can_redo: bool,
}

impl SketchHistory {
    pub fn push(&mut self, present: Vec<Stroke>) {
        self.past.push(std::mem::replace(&mut self.present, present));
        if self.past.len() > SKETCH_HISTORY_LIMIT {
            let excess = self.past.len() - SKETCH_HISTORY_LIMIT;
            self.past.drain(..excess);
        }
        self.future.clear();
    }

    /// Undo past the cap still removes the newest stroke: once `past` has been trimmed there is no recorded predecessor,
    /// so the present is peeled back one entry rather than restored wholesale.
    pub fn undo(&mut self) {
        match self.past.pop() {
            Some(previous) => self.future.insert(0, std::mem::replace(&mut self.present, previous)),
            None => {
                if self.present.is_empty() {
                    return;
                }
                let mut peeled = self.present.clone();
                peeled.pop();
                self.future.insert(0, std::mem::replace(&mut self.present, peeled));
            }
        }
    }

    pub fn redo(&mut self) {
        if self.future.is_empty() {
            return;
        }
        let next = self.future.remove(0);
        self.past.push(std::mem::replace(&mut self.present, next));
    }

    pub fn state(&self) -> HistoryState {
        HistoryState { count: self.present.len(), can_undo: !self.past.is_empty() || !self.present.is_empty(), can_redo: !self.future.is_empty() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SketchCommand {
    Undo,
    Redo,
}

/// Cmd/Ctrl+Z undoes, Cmd/Ctrl+Shift+Z redoes, and Ctrl+Y also redoes off Apple platforms.
pub fn sketch_command(key: char, alt: bool, shift: bool, ctrl: bool, meta: bool, apple: bool) -> Option<SketchCommand> {
    if alt {
        return None;
    }
    let primary = if apple { meta } else { ctrl };
    if !primary || (apple && ctrl) {
        return None;
    }
    match key.to_ascii_lowercase() {
        'z' => Some(if shift { SketchCommand::Redo } else { SketchCommand::Undo }),
        'y' if !apple && !shift => Some(SketchCommand::Redo),
        _ => None,
    }
}

pub fn shortcut_label(command: SketchCommand, apple: bool) -> &'static str {
    match (command, apple) {
        (SketchCommand::Undo, true) => "⌘Z",
        (SketchCommand::Redo, true) => "⇧⌘Z",
        (SketchCommand::Undo, false) => "Ctrl+Z",
        (SketchCommand::Redo, false) => "Ctrl+Shift+Z",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(i: usize) -> Stroke {
        Stroke::Shape { kind: Shape::Line, from: Point { x: i as f32, y: 0.0 }, to: Point { x: i as f32, y: 10.0 }, color: SKETCH_COLORS[0], width: SKETCH_STROKE_WIDTH }
    }

    fn pushed(count: usize) -> SketchHistory {
        let mut h = SketchHistory::default();
        for i in 0..count {
            let mut next = h.present.clone();
            next.push(stroke(i));
            h.push(next);
        }
        h
    }

    #[test]
    fn keeps_the_past_under_the_cap_while_the_present_keeps_growing() {
        let h = pushed(150);
        assert_eq!(SKETCH_HISTORY_LIMIT, 100);
        assert_eq!(h.past.len(), 100);
        assert_eq!(h.present.len(), 150);
        assert!(h.future.is_empty());
        assert_eq!(h.state(), HistoryState { count: 150, can_undo: true, can_redo: false });
    }

    #[test]
    fn undoes_one_push_at_a_time_and_redoes_back_to_where_it_was() {
        let mut h = pushed(2);
        h.undo();
        assert_eq!(h.state(), HistoryState { count: 1, can_undo: true, can_redo: true });
        h.redo();
        assert_eq!(h.state(), HistoryState { count: 2, can_undo: true, can_redo: false });
    }

    #[test]
    fn peels_strokes_off_the_present_once_the_capped_past_runs_out() {
        let mut h = pushed(150);
        for _ in 0..100 {
            h.undo();
        }
        assert!(h.past.is_empty());
        assert_eq!(h.present.len(), 50);
        h.undo();
        assert_eq!(h.present.len(), 49);
    }

    #[test]
    fn stops_undoing_at_an_empty_present_and_redoing_at_an_empty_future() {
        let mut h = SketchHistory::default();
        h.undo();
        h.redo();
        assert_eq!(h, SketchHistory::default());
        assert_eq!(h.state(), HistoryState::default());
    }

    #[test]
    fn drops_the_redo_branch_as_soon_as_a_new_stroke_lands() {
        let mut h = pushed(1);
        h.undo();
        assert_eq!(h.future.len(), 1);
        h.push(vec![stroke(1)]);
        assert!(h.future.is_empty());
    }

    const O: Point = Point { x: 0.0, y: 0.0 };

    #[test]
    fn leaves_the_point_alone_when_shift_is_not_held() {
        let p = Point { x: 13.0, y: 77.0 };
        assert_eq!(constrain_point(Some(Shape::Line), O, p, false), p);
    }

    #[test]
    fn snaps_line_and_arrow_to_45_degree_increments_keeping_the_drag_length() {
        let flat = constrain_point(Some(Shape::Line), O, Point { x: 100.0, y: 10.0 }, true);
        assert!((flat.x - 100f32.hypot(10.0)).abs() < 1e-3 && flat.y.abs() < 1e-3);
        let diagonal = constrain_point(Some(Shape::Arrow), O, Point { x: 100.0, y: 90.0 }, true);
        let length = 100f32.hypot(90.0);
        assert!((diagonal.x - length * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-2);
        assert!((diagonal.y - length * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-2);
    }

    #[test]
    fn forces_rect_and_ellipse_square_on_the_longer_axis() {
        assert_eq!(constrain_point(Some(Shape::Rect), O, Point { x: 100.0, y: 40.0 }, true), Point { x: 100.0, y: 100.0 });
        assert_eq!(constrain_point(Some(Shape::Ellipse), O, Point { x: -100.0, y: 40.0 }, true), Point { x: -100.0, y: 100.0 });
    }

    #[test]
    fn treats_a_zero_delta_as_positive_so_the_square_never_collapses() {
        let at = Point { x: 5.0, y: 5.0 };
        assert_eq!(constrain_point(Some(Shape::Rect), at, Point { x: 105.0, y: 5.0 }, true), Point { x: 105.0, y: 105.0 });
    }

    #[test]
    fn never_constrains_freehand_or_text() {
        let p = Point { x: 3.0, y: 9.0 };
        assert_eq!(constrain_point(None, O, p, true), p);
    }

    #[test]
    fn shortens_the_arrow_shaft_and_builds_the_head_triangle_from_the_stroke_width() {
        let (shaft, [_, left, right]) = arrow_geometry(O, Point { x: 100.0, y: 0.0 }, SKETCH_STROKE_WIDTH);
        let head = (SKETCH_STROKE_WIDTH * 4.0).min(100.0);
        assert_eq!(head, 16.0);
        assert_eq!(shaft, Point { x: 100.0 - head * 0.7, y: 0.0 });
        assert_eq!(left, Point { x: 100.0 - head, y: -head / 2.0 });
        assert_eq!(right, Point { x: 100.0 - head, y: head / 2.0 });
    }

    #[test]
    fn maps_the_shortcuts_per_platform() {
        assert_eq!(sketch_command('z', false, false, true, false, false), Some(SketchCommand::Undo));
        assert_eq!(sketch_command('Z', false, true, true, false, false), Some(SketchCommand::Redo));
        assert_eq!(sketch_command('y', false, false, true, false, false), Some(SketchCommand::Redo));
        assert_eq!(sketch_command('z', false, false, false, true, true), Some(SketchCommand::Undo));
        assert_eq!(sketch_command('z', false, true, false, true, true), Some(SketchCommand::Redo));
        assert_eq!(sketch_command('y', false, false, true, false, true), None);
        assert_eq!(sketch_command('z', false, false, false, false, false), None);
        assert_eq!(sketch_command('z', true, false, true, false, false), None);
    }

    #[test]
    fn keeps_the_six_tools_in_palette_order_and_the_four_ink_colours() {
        assert_eq!(SKETCH_TOOLS, [SketchTool::Pen, SketchTool::Line, SketchTool::Arrow, SketchTool::Rect, SketchTool::Ellipse, SketchTool::Text]);
        assert_eq!(SKETCH_COLORS, [0xE03131, 0x1971C2, 0x2F9E44, 0x1F1E1D]);
    }
}
