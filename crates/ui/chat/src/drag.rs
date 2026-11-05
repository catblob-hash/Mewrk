//! `timelineDrag.ts`: dragging what is picked out on the timeline to somewhere else in it.
//!
//! What is picked need not sit together: a drag takes every picked card and row, however far apart, and sets them down side
//! by side. So a drag has three looks.
//!
//! - **Gathered.** The picked pieces leave the timeline, which closes up over where they were, and ride under the pointer as
//!   one tight stack. The insertion line marks where they would land.
//! - **Docked.** The pieces are set down at the line the drag began on, side by side and dimmed: what letting go there makes
//!   of them.
//! - **Apart.** The pieces sit where they were, dimmed, and the timeline is laid out as it was. Letting go changes nothing.
//!
//! A drag gathers the moment it begins. Where it began stays marked by the home line: the gap the grabbed piece came out of.
//! For pieces that sat together, set down at the home line is where they were: they are apart. For scattered ones the line
//! means two things, "bring them together right here" and "put them back", and it offers them in that order: the pieces dock,
//! and stay docked for [`TIMELINE_DRAG_DOCK_MS`], then go back apart. Leaving the line and coming back starts the count over.
//!
//! Upstream measures the page; here the timeline is virtual, so a [`DragGeometry`] answers for everything, built or not.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// How far a press has to travel before it is a drag and not a click.
pub const DRAG_THRESHOLD: f32 = 5.0;
/// How far past the home line's reach a drag taken apart has to go to gather again, so its edge does not flicker.
const HOME_HOLD: f32 = 12.0;
/// How far from where the press was made a drag still counts as not having left the home line.
const HOME_PIN_RADIUS: f32 = 24.0;
/// How long scattered pieces brought back to the home line stay docked there before they go back apart.
pub const TIMELINE_DRAG_DOCK_MS: u64 = 750;
/// How close to the top or bottom edge a drag scrolls the timeline.
const SCROLL_EDGE: f32 = 32.0;
/// The most a drag scrolls the timeline per frame.
const SCROLL_STEP: f32 = 18.0;
/// The most of one piece the stack under the pointer shows.
pub const GHOST_PIECE_HEIGHT: f32 = 132.0;
/// How many pieces the stack shows; the rest are counted on a badge.
pub const GHOST_PIECES: usize = 6;
/// The gap between two pieces of the stack that are not rows of one block.
const GHOST_GAP: f32 = 8.0;
/// How long a piece of the stack takes to fly to its place.
pub const MOTION: Duration = Duration::from_millis(220);
/// `.context-slot + .context-slot`, and what one docked piece keeps from the next.
pub const SLOT_GAP: f32 = 16.0;
/// `.timeline-block { padding: 1px 0 3px }`.
pub const BLOCK_PAD_TOP: f32 = 1.0;
pub const BLOCK_PAD_BOTTOM: f32 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Apart,
    Docked,
    Gathered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragOutcome {
    /// The press never became a drag.
    Click,
    /// Let go where it began, or called off.
    Cancel,
    /// Let go with the insertion line at this index.
    Drop(usize),
}

/// One card or row of the timeline, as a drag sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// The node drawing it.
    pub node: usize,
    /// Its row within a block, `None` for a card that is the whole of its node.
    pub row: Option<usize>,
    pub id: String,
    /// Every context it stands for: an answered question is one card for two.
    pub ids: Vec<String>,
    /// The raw index a drop above it lands at.
    pub index: usize,
    pub picked: bool,
}

/// One thing a drag lifts out of the timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub node: usize,
    /// The units it holds: all of a node's, or one row of a block that keeps others.
    pub units: Vec<usize>,
    /// A row lifted out of its block.
    pub row: bool,
}

/// Where a piece is drawn, in the window.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct PieceBox {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

/// What a change of layout moves: a whole row of the virtual list, or one card or row of a block that is losing others.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mover {
    Node(u64),
    Unit(u64),
    /// What a slot's row holds after its card, by the row's key: `TurnChanges` and its notices, measured at the row's foot. Siblings of
    /// the slot upstream, so they move by themselves when the slot is lifted or moves row by row.
    Extras(u64),
}

/// A card or row on its way to where the layout has it. Only drawn: the layout already has it there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slide {
    pub dx: f32,
    pub dy: f32,
    /// What it fades up from, for one landing from the stack.
    pub fade_from: Option<f32>,
    pub started: Instant,
}

impl Slide {
    pub fn progress(&self, now: Instant) -> f32 {
        ease(now.saturating_duration_since(self.started).as_secs_f32() / MOTION.as_secs_f32())
    }

    pub fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= MOTION
    }

    /// How far it still has to go.
    pub fn offset(&self, now: Instant) -> (f32, f32) {
        let left = 1.0 - self.progress(now);
        (self.dx * left, self.dy * left)
    }
}

/// Where the pieces are set down at the home line. Between two rows of one block the line is inside that block; anywhere else it is
/// between rows of the virtual list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockAt {
    Before(usize),
    /// Among a block's rows, ahead of this unit.
    Inside { node: usize, before: usize },
    End,
}

/// The pieces as set down side by side: rows that end up next to each other share a block, as they will once really moved, and
/// everything else stands as a slot of its own.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct DockLayout {
    /// Each child of the dock: the pieces it holds, and its height.
    pub groups: Vec<(Vec<usize>, f32)>,
    /// Each piece's top within the dock.
    pub tops: Vec<f32>,
    pub height: f32,
    pub first_is_slot: bool,
    pub last_is_slot: bool,
}

/// What a drag needs to know about the timeline, in window coordinates, as it lies under the look last applied.
pub trait DragGeometry {
    /// Where a unit is drawn, top and bottom; a lifted one answers where it sits when set down.
    fn span(&self, unit: usize) -> (f32, f32);
    /// The scroller's top and bottom.
    fn view(&self) -> (f32, f32);
    /// The top of the column.
    fn column_top(&self) -> f32;
    /// One past the last context's index: where a drop below everything lands.
    fn end_index(&self) -> usize;
    /// Scrolls by this much, and answers how much it moved.
    fn scroll_by(&mut self, delta: f32) -> f32;
    /// Lays the timeline out for this look.
    fn apply_look(&mut self, session: &DragSession, look: Look);
    /// The top of the pieces set down at the home line.
    fn dock_top(&self, session: &DragSession) -> Option<f32>;
    /// `reflowSnapshot`: what a change of look moves, with where each is drawn now, slides still running included.
    fn reflow_snapshot(&self, session: &DragSession) -> Vec<(Mover, f32)>;
    /// `reflowFrom`: slides everything in `tops` from where it was to where the layout has it now.
    fn reflow_from(&mut self, session: &DragSession, tops: Vec<(Mover, f32)>, now: Instant);
    /// Sets a piece on its way to its own place from this far off.
    fn fly_piece(&mut self, session: &DragSession, piece: usize, dx: f32, dy: f32, now: Instant);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkState {
    /// A drop would land on the home line, or the pieces are apart at it.
    At,
    /// Faint: a mark of where the drag began.
    Away,
    /// The pieces are docked on it, and the count runs along it.
    Counting,
}

/// A flight of one piece of the stack, relative to the stack: x, y and height at each end. It stays on the entry once it lands, and
/// the piece stays where it took it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Flight {
    pub from: (f32, f32, f32),
    pub to: (f32, f32, f32),
    /// Going home, a piece fades out onto what is on the timeline again under it.
    pub fade: bool,
    pub started: Instant,
}

impl Flight {
    pub fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= MOTION
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GhostEntry {
    pub piece: usize,
    /// Where it sits in the stack, from the pointer.
    pub x: f32,
    pub y: f32,
    pub height: f32,
    /// A piece taller than the stack shows of it fades out at its foot.
    pub clipped: bool,
    pub flight: Option<Flight>,
}

impl GhostEntry {
    /// Where it is drawn now, relative to the stack: x, y and height, and the opacity a flight home has left it. `None` is the stack's own.
    pub fn frame(&self, now: Instant) -> ((f32, f32, f32), Option<f32>) {
        let Some(flight) = &self.flight else { return ((self.x, self.y, self.height), None) };
        let t = ease(now.saturating_duration_since(flight.started).as_secs_f32() / MOTION.as_secs_f32());
        let at = |from: f32, to: f32| from + (to - from) * t;
        ((at(flight.from.0, flight.to.0), at(flight.from.1, flight.to.1), at(flight.from.2, flight.to.2)), flight.fade.then_some(1.0 - t))
    }
}

#[derive(Clone, Debug)]
pub struct DragSession {
    pub units: Vec<Unit>,
    /// Everything not being dragged, in timeline order: what a drop can land between.
    pub rest: Vec<usize>,
    pub pieces: Vec<Piece>,
    pub grabbed: usize,
    /// The ids of every context being dragged.
    pub ids: Vec<String>,
    pub lifted_ids: HashSet<String>,
    /// Whether the pieces sit apart on the timeline, so that gathering them where they are is a move.
    pub scattered: bool,
    /// The last thing above the grabbed piece that stays put, which the home line hangs from.
    pub anchor: Option<usize>,
    /// Where in `rest` the home line is: a drop there lands just above `rest[home_position]`.
    pub home_position: usize,
    /// How far below the anchor's foot the first dragged thing under it begins, as the timeline lay before the drag.
    pub lead_below: f32,
    pub reach_above: f32,
    pub reach_below: f32,
    pub press_x: f32,
    pub press_below: f32,
    /// Whether the pointer has yet to move off the press, which holds the drag to the home line: closing the timeline up puts
    /// other things under a pointer that has barely moved, and a drag let go there should land where it began.
    pub pinned: bool,
    /// How far setting the pieces down has moved the anchor on screen since they were last gathered.
    pub drift: f32,
    pub offsets_x: Vec<f32>,
    /// Where each piece sits and how big it is, as the timeline lay when the drag began: gathered, a piece has no height left to ask about.
    pub piece_boxes: Vec<PieceBox>,
    /// `stillMotion`: the system's "reduce motion", read as the drag began.
    pub still: bool,
    pub grab_y: f32,
    pub look: Look,
    /// Whether the drag is at the home line: it starts there, gathered.
    pub home: bool,
    pub dock_deadline: Option<Instant>,
    pub dock_started: Option<Instant>,
    /// Where a drop would land.
    pub target: Option<usize>,
    /// Where the timeline was last told to draw its insertion line: nowhere while the home line stands in for it.
    pub drawn: Option<usize>,
    pub mark_state: MarkState,
    pub mark_y: f32,
    /// The home line against the column, as `transition: top` runs it: where it set out from, where it is going, and since when.
    pub mark_glide: Option<(f32, f32, Instant)>,
    pub ghost: Vec<GhostEntry>,
    pub ghost_more: usize,
    /// `placeGhost`: where the stack hangs. It follows the pointer only while the pieces are gathered.
    pub layer: (f32, f32),
}

/// A press on something picked out, followed until it is let go.
#[derive(Clone, Debug)]
pub struct PressDrag {
    pub press: (f32, f32),
    pub pointer: (f32, f32),
    pub session: Option<DragSession>,
}

impl PressDrag {
    pub fn new(x: f32, y: f32) -> Self {
        Self { press: (x, y), pointer: (x, y), session: None }
    }

    /// Whether the press has become a drag.
    pub fn active(&self) -> bool {
        self.session.is_some()
    }

    /// Past the threshold, whether a session should start now.
    pub fn crossed_threshold(&self) -> bool {
        (self.pointer.0 - self.press.0).hypot(self.pointer.1 - self.press.1) >= DRAG_THRESHOLD
    }
}

/// `start`: what a drag lifts, from the units as they lie. `None` when nothing is picked. Rows whose whole node is picked
/// leave as one piece; one row of a block that keeps others leaves alone.
pub fn start_session(units: Vec<Unit>, press: (f32, f32), geometry: &dyn DragGeometry, grabbed_at: impl Fn(usize) -> bool, unit_box: impl Fn(usize) -> (f32, f32), still: bool) -> Option<DragSession> {
    if !units.iter().any(|unit| unit.picked) {
        return None;
    }
    let mut pieces = Vec::new();
    let mut from = 0;
    while from < units.len() {
        let node = units[from].node;
        let mut to = from + 1;
        while to < units.len() && units[to].node == node {
            to += 1;
        }
        let group: Vec<usize> = (from..to).collect();
        let lifted: Vec<usize> = group.iter().copied().filter(|&unit| units[unit].picked).collect();
        if !lifted.is_empty() {
            if lifted.len() == group.len() {
                pieces.push(Piece { node, units: group, row: false });
            } else {
                for unit in lifted {
                    pieces.push(Piece { node, units: vec![unit], row: units[unit].row.is_some() });
                }
            }
        }
        from = to;
    }
    let grabbed = pieces.iter().position(|piece| piece.units.iter().any(|&unit| grabbed_at(unit))).unwrap_or(0);
    let first_of_grabbed = pieces[grabbed].units[0];
    let mut anchor = first_of_grabbed;
    let anchor = loop {
        if anchor == 0 {
            break None;
        }
        anchor -= 1;
        if !units[anchor].picked {
            break Some(anchor);
        }
    };
    let rest: Vec<usize> = (0..units.len()).filter(|&unit| !units[unit].picked).collect();
    let positions: Vec<usize> = (0..units.len()).filter(|&unit| units[unit].picked).collect();
    let scattered = positions[positions.len() - 1] - positions[0] + 1 != positions.len();
    let home_position = anchor.map_or(0, |anchor| rest.iter().position(|&unit| unit == anchor).map_or(0, |at| at + 1));
    let (grabbed_top, _) = geometry.span(first_of_grabbed);
    let anchor_edge = anchor.map_or_else(|| geometry.column_top(), |anchor| geometry.span(anchor).1);
    let first_under_anchor = anchor.map_or(0, |anchor| anchor + 1);
    let lead_below = geometry.span(first_under_anchor).0 - anchor_edge;
    let ids: Vec<String> = units.iter().filter(|unit| unit.picked).flat_map(|unit| unit.ids.iter().cloned()).collect();
    let lifted_ids = ids.iter().cloned().collect();
    let piece_boxes: Vec<PieceBox> = pieces
        .iter()
        .map(|piece| {
            let top = geometry.span(piece.units[0]).0;
            let (left, width) = unit_box(piece.units[0]);
            PieceBox { left, top, width, height: geometry.span(piece.units[piece.units.len() - 1]).1 - top }
        })
        .collect();
    let offsets_x = piece_boxes.iter().map(|shown| shown.left - press.0).collect();
    Some(DragSession {
        rest,
        grabbed,
        ids,
        lifted_ids,
        scattered,
        anchor,
        home_position,
        lead_below,
        reach_above: f32::INFINITY,
        reach_below: f32::INFINITY,
        press_x: press.0,
        press_below: press.1 - anchor_edge,
        pinned: true,
        drift: 0.0,
        offsets_x,
        piece_boxes,
        still,
        grab_y: press.1 - grabbed_top,
        look: Look::Apart,
        home: true,
        dock_deadline: None,
        dock_started: None,
        target: None,
        drawn: None,
        mark_state: MarkState::Away,
        mark_y: 0.0,
        mark_glide: None,
        ghost: Vec::new(),
        ghost_more: 0,
        layer: press,
        pieces,
        units,
    })
}

impl DragSession {
    fn anchor_edge(&self, geometry: &dyn DragGeometry) -> f32 {
        self.anchor.map_or_else(|| geometry.column_top(), |anchor| geometry.span(anchor).1)
    }

    /// A drop above `rest[position]`, as the index the timeline knows it by.
    fn drop_above(&self, geometry: &dyn DragGeometry, position: usize) -> (usize, usize) {
        let index = self.rest.get(position).map_or_else(|| geometry.end_index(), |&unit| self.units[unit].index);
        (index, position)
    }

    /// The first thing staying put whose middle is below `y`: a drop lands just above it.
    fn rest_below(&self, geometry: &dyn DragGeometry, y: f32) -> usize {
        self.rest.partition_point(|&unit| {
            let (top, bottom) = geometry.span(unit);
            (top + bottom) * 0.5 <= y
        })
    }

    /// Where a drop at `y` lands. Over a card too tall for both its ends to be on screen, that can put the line out of sight,
    /// so the end that can be seen is taken instead: the line is what says where a drop goes.
    fn target_at(&self, geometry: &dyn DragGeometry, y: f32) -> (usize, usize) {
        let (view_top, view_bottom) = geometry.view();
        let line_at = |position: usize| -> f32 {
            match self.rest.get(position) {
                Some(&unit) => geometry.span(unit).0,
                None => self.rest.last().map_or(view_top, |&unit| geometry.span(unit).1),
            }
        };
        let (sight_top, sight_bottom) = (view_top + SCROLL_EDGE, view_bottom - SCROLL_EDGE);
        let in_sight = |position: isize| -> bool {
            if position < 0 || position as usize > self.rest.len() {
                return false;
            }
            let line = line_at(position as usize);
            line >= sight_top && line <= sight_bottom
        };
        let mut below = self.rest_below(geometry, y);
        if !in_sight(below as isize) {
            let other = if line_at(below) > sight_bottom { below as isize - 1 } else { below as isize + 1 };
            if in_sight(other) {
                below = other as usize;
            }
        }
        self.drop_above(geometry, below)
    }

    /// Whether the pointer points at the home line: whether a drop here would land on it, as the timeline lies while gathered.
    /// That reach is measured whenever the pieces are gathered and kept as distances from the anchor, so it is the same stretch
    /// of screen once they are set down.
    fn points_home(&mut self, geometry: &dyn DragGeometry, pointer_y: f32) -> bool {
        let gathered = self.look == Look::Gathered;
        let edge = self.anchor_edge(geometry) - if gathered { 0.0 } else { self.drift };
        if gathered {
            let middle = |unit: usize| {
                let (top, bottom) = geometry.span(unit);
                (top + bottom) * 0.5
            };
            self.reach_above = self.anchor.map_or(f32::INFINITY, |anchor| edge - middle(anchor));
            self.reach_below = self.rest.get(self.home_position).map_or(f32::INFINITY, |&unit| middle(unit) - edge);
        }
        let hold = if gathered { 0.0 } else { HOME_HOLD };
        pointer_y >= edge - self.reach_above - hold && pointer_y < edge + self.reach_below + hold
    }

    /// Whether this piece is a whole block, which keeps its block's padding when it stands alone.
    fn piece_is_block(&self, piece: usize) -> bool {
        !self.pieces[piece].row && self.units[self.pieces[piece].units[0]].row.is_some()
    }

    /// `buildDock`, as sizes: in order and side by side, rows next to each other sharing a block.
    pub fn dock_layout(&self) -> DockLayout {
        let mut layout = DockLayout { tops: vec![0.0; self.pieces.len()], ..DockLayout::default() };
        let mut y = 0.0;
        let mut at = 0;
        while at < self.pieces.len() {
            if !layout.groups.is_empty() {
                y += SLOT_GAP;
            }
            let height = self.piece_boxes[at].height;
            if !self.pieces[at].row {
                let pad = if self.piece_is_block(at) { BLOCK_PAD_TOP + BLOCK_PAD_BOTTOM } else { 0.0 };
                layout.tops[at] = y;
                layout.groups.push((vec![at], height + pad));
                layout.first_is_slot |= at == 0;
                layout.last_is_slot = true;
                y += height + pad;
                at += 1;
                continue;
            }
            let mut group = Vec::new();
            let mut inner = BLOCK_PAD_TOP;
            while at < self.pieces.len() && self.pieces[at].row {
                layout.tops[at] = y + inner;
                inner += self.piece_boxes[at].height;
                group.push(at);
                at += 1;
            }
            let group_h = inner + BLOCK_PAD_BOTTOM;
            layout.groups.push((group, group_h));
            layout.last_is_slot = false;
            y += group_h;
        }
        layout.height = y;
        layout
    }

    /// Between two rows of one block the home line is inside that block; anywhere else it is between what the list draws.
    pub fn dock_place(&self) -> DockAt {
        let below = self.rest.get(self.home_position).copied();
        match (below, self.anchor) {
            (Some(below), Some(anchor)) if self.units[below].node == self.units[anchor].node && self.units[below].row.is_some() => DockAt::Inside { node: self.units[below].node, before: below },
            (Some(below), _) => DockAt::Before(self.units[below].node),
            _ => DockAt::End,
        }
    }

    /// `shownBoxes`: where each piece is drawn on the timeline now: its copy at the home line while docked, itself otherwise.
    fn shown_boxes(&self, geometry: &dyn DragGeometry) -> Vec<PieceBox> {
        let dock = (self.look == Look::Docked).then(|| geometry.dock_top(self)).flatten();
        let layout = dock.map(|_| self.dock_layout());
        self.pieces
            .iter()
            .enumerate()
            .map(|(index, piece)| {
                let top = match (dock, &layout) {
                    (Some(dock), Some(layout)) => dock + layout.tops[index],
                    _ => geometry.span(piece.units[0]).0,
                };
                PieceBox { top, ..self.piece_boxes[index] }
            })
            .collect()
    }

    /// `setLook` with `reflow` and `holdAnchor`: the layout changes with the anchor, and the home line with it, held where it is on
    /// screen, and what the change moves slides from where it was. A timeline at its top or its foot has nothing left to scroll, and
    /// there the anchor moves with everything else. Wherever the pieces are drawn now is where what stands in for them next sets out from.
    pub fn set_look(&mut self, geometry: &mut dyn DragGeometry, look: Look, pointer: (f32, f32), now: Instant) {
        let from = self.look;
        if from == look {
            return;
        }
        let leaving = self.shown_boxes(geometry);
        let tops = geometry.reflow_snapshot(self);
        let before = self.anchor_edge(geometry);
        geometry.apply_look(self, look);
        let shift = self.anchor_edge(geometry) - before;
        if shift != 0.0 {
            geometry.scroll_by(shift);
        }
        let moved = self.anchor_edge(geometry) - before;
        let moved = if moved.abs() < 1.0 { 0.0 } else { moved };
        geometry.reflow_from(self, tops, now);
        // Gathered, the home line's reach is measured afresh; set down, it stays where it was measured.
        self.drift = if look == Look::Gathered { 0.0 } else { self.drift + moved };
        // The press was made on the screen, not on an anchor that has since moved under it.
        if self.pinned {
            self.press_below -= moved;
        }
        self.look = look;
        if look == Look::Gathered {
            self.gather_ghost(&leaving, pointer, now);
        } else if from == Look::Gathered {
            let boxes = self.shown_boxes(geometry);
            self.part_ghost(&boxes, now);
        } else if look == Look::Apart {
            self.scatter(geometry, &leaving, now);
        }
    }

    /// `scatter`: sends the pieces from where their copies sat at the home line back to their own places.
    fn scatter(&self, geometry: &mut dyn DragGeometry, from: &[PieceBox], now: Instant) {
        if self.still {
            return;
        }
        for (index, piece) in self.pieces.iter().enumerate() {
            let to = geometry.span(piece.units[0]).0;
            geometry.fly_piece(self, index, 0.0, from[index].top - to, now);
        }
    }

    /// `present`: brings what is drawn up to the look and the pointer.
    fn present(&mut self, geometry: &dyn DragGeometry, pointer: (f32, f32), now: Instant) {
        let pointer_y = pointer.1;
        match self.look {
            Look::Gathered => {
                self.layer = pointer;
                let (index, position) = if self.pinned { self.drop_above(geometry, self.home_position) } else { self.target_at(geometry, pointer_y) };
                let on_home = position == self.home_position;
                self.target = Some(index);
                self.drawn = if on_home { None } else { Some(index) };
                self.mark_state = if on_home { MarkState::At } else { MarkState::Away };
            }
            Look::Docked => {
                // Let go now, the pieces stay as they are shown: together, at the home line.
                self.target = Some(self.drop_above(geometry, self.home_position).0);
                self.drawn = None;
                self.mark_state = MarkState::Counting;
            }
            Look::Apart => {
                self.target = None;
                self.drawn = None;
                self.mark_state = MarkState::At;
            }
        }
        let column = geometry.column_top();
        let below = self.rest.get(self.home_position).copied();
        let line = match self.look {
            Look::Apart => self.anchor_edge(geometry) + self.lead_below,
            Look::Docked => geometry.dock_top(self).unwrap_or_else(|| self.anchor_edge(geometry)),
            Look::Gathered => match (below, self.anchor) {
                (Some(below), _) => geometry.span(below).0,
                (None, Some(anchor)) => geometry.span(anchor).1 + 2.0,
                (None, None) => column + 2.0,
            },
        };
        self.mark_y = line;
        // `transition: top`: the line glides there. Against the column, so scrolling it carries the line and starts nothing.
        let against_column = line - column;
        match self.mark_glide {
            Some((_, to, _)) if (to - against_column).abs() <= 0.5 => {}
            Some(_) if self.still => self.mark_glide = Some((against_column, against_column, now)),
            Some(_) => self.mark_glide = Some((self.mark_drawn(now), against_column, now)),
            None => self.mark_glide = Some((against_column, against_column, now)),
        }
    }

    /// The home line as drawn, against the column.
    pub fn mark_drawn(&self, now: Instant) -> f32 {
        let Some((from, to, started)) = self.mark_glide else { return 0.0 };
        const TRANSITION: Duration = Duration::from_millis(200);
        from + (to - from) * ease(now.saturating_duration_since(started).as_secs_f32() / TRANSITION.as_secs_f32())
    }

    fn mark_moving(&self, now: Instant) -> bool {
        self.mark_glide.is_some_and(|(from, to, started)| from != to && now.saturating_duration_since(started) < Duration::from_millis(200))
    }

    /// `step`: brings the drag up to where the pointer is. Answers whether it wants another frame (it is scrolling, or counting).
    pub fn step(&mut self, geometry: &mut dyn DragGeometry, pointer: (f32, f32), final_step: bool, now: Instant) -> bool {
        let mut scrolled = false;
        if !final_step {
            let (top, bottom) = geometry.view();
            let stride = if pointer.1 < top + SCROLL_EDGE {
                -SCROLL_STEP.min(top + SCROLL_EDGE - pointer.1)
            } else if pointer.1 > bottom - SCROLL_EDGE {
                SCROLL_STEP.min(pointer.1 - (bottom - SCROLL_EDGE))
            } else {
                0.0
            };
            if stride != 0.0 {
                scrolled = geometry.scroll_by(stride) != 0.0;
            }
        }
        if self.pinned {
            let at_press = self.anchor_edge(geometry) + self.press_below;
            self.pinned = (pointer.0 - self.press_x).hypot(pointer.1 - at_press) <= HOME_PIN_RADIUS;
        }
        // Measured even while the pin answers for it: the reach is what the line has once the pieces are set down.
        let home = self.points_home(geometry, pointer.1) || self.pinned;
        if home != self.home {
            self.home = home;
            self.dock_deadline = None;
            self.dock_started = None;
            if !home {
                self.set_look(geometry, Look::Gathered, pointer, now);
            } else if self.scattered {
                self.set_look(geometry, Look::Docked, pointer, now);
                self.dock_deadline = Some(now + Duration::from_millis(TIMELINE_DRAG_DOCK_MS));
                self.dock_started = Some(now);
            } else {
                // Pieces that sat together are, set down at the home line, simply where they were.
                self.set_look(geometry, Look::Apart, pointer, now);
            }
        }
        // The count after which pieces docked at the home line go back apart.
        if self.look == Look::Docked && self.dock_deadline.is_some_and(|deadline| now >= deadline) {
            self.dock_deadline = None;
            self.dock_started = None;
            self.set_look(geometry, Look::Apart, pointer, now);
        }
        self.present(geometry, pointer, now);
        scrolled || self.dock_deadline.is_some() || self.mark_moving(now) || self.ghost.iter().any(|entry| entry.flight.is_some_and(|flight| !flight.done(now)))
    }

    /// `buildGhost`: the stack hangs from the grabbed piece, which keeps the pointer where the press took hold of it.
    fn build_ghost(&mut self) {
        let first = self.grabbed.saturating_sub(2).min(self.pieces.len().saturating_sub(GHOST_PIECES));
        let last = (first + GHOST_PIECES).min(self.pieces.len());
        let mut shown: Vec<GhostEntry> = (first..last)
            .map(|piece| {
                let whole = self.piece_boxes[piece].height;
                let height = whole.min(GHOST_PIECE_HEIGHT);
                GhostEntry { piece, x: self.offsets_x[piece], y: 0.0, height, clipped: height < whole, flight: None }
            })
            .collect();
        let held = shown.iter().position(|entry| entry.piece == self.grabbed).unwrap_or(0);
        let gap = |pieces: &[Piece], upper: usize, lower: usize| if pieces[upper].row && pieces[lower].row && pieces[upper].node == pieces[lower].node { 0.0 } else { GHOST_GAP };
        shown[held].y = -(self.grab_y.min(shown[held].height - 12.0).max(0.0));
        for at in held + 1..shown.len() {
            shown[at].y = shown[at - 1].y + shown[at - 1].height + gap(&self.pieces, shown[at - 1].piece, shown[at].piece);
        }
        for at in (0..held).rev() {
            shown[at].y = shown[at + 1].y - gap(&self.pieces, shown[at].piece, shown[at + 1].piece) - shown[at].height;
        }
        self.ghost_more = self.pieces.len() - shown.len();
        self.ghost = shown;
    }

    /// `gatherGhost`: brings the stack out from where the pieces sit on the timeline. A piece still on its way home turns round from
    /// wherever it has got to.
    fn gather_ghost(&mut self, boxes: &[PieceBox], pointer: (f32, f32), now: Instant) {
        let fresh = self.ghost.is_empty();
        if fresh {
            self.build_ghost();
        }
        let was = std::mem::replace(&mut self.layer, pointer);
        let still = self.still;
        for entry in &mut self.ghost {
            let at = boxes[entry.piece];
            let from = match entry.flight {
                Some(_) if !fresh => {
                    let (drawn, _) = entry.frame(now);
                    (was.0 + drawn.0 - pointer.0, was.1 + drawn.1 - pointer.1, drawn.2)
                }
                _ => (at.left - pointer.0, at.top - pointer.1, at.height),
            };
            entry.flight = (!still).then_some(Flight { from, to: (entry.x, entry.y, entry.height), fade: false, started: now });
        }
    }

    /// `partGhost`: sends the stack onto the pieces, which are on the timeline again under it: themselves, or their copies at the home line.
    fn part_ghost(&mut self, boxes: &[PieceBox], now: Instant) {
        let layer = self.layer;
        let still = self.still;
        for entry in &mut self.ghost {
            let (from, _) = entry.frame(now);
            let at = boxes[entry.piece];
            entry.flight = (!still).then_some(Flight { from, to: (at.left - layer.0, at.top - layer.1, at.height), fade: true, started: now });
        }
    }

    /// Whether the stack is on screen: while the pieces are gathered, and for as long as it takes them to go home after.
    pub fn ghost_shown(&self, now: Instant) -> bool {
        !self.ghost.is_empty() && (self.look == Look::Gathered || self.ghost.iter().any(|entry| entry.flight.is_some_and(|flight| flight.fade && !flight.done(now))))
    }

    /// What letting go means now.
    pub fn outcome(&self) -> DragOutcome {
        match (self.look, self.target) {
            (Look::Apart, _) | (_, None) => DragOutcome::Cancel,
            (_, Some(index)) => DragOutcome::Drop(index),
        }
    }

    /// How far along the count is, for the line that shows it.
    pub fn dock_progress(&self, now: Instant) -> Option<f32> {
        let started = self.dock_started?;
        Some((now.duration_since(started).as_secs_f32() * 1000.0 / TIMELINE_DRAG_DOCK_MS as f32).min(1.0))
    }
}

/// `MOTION_EASING`, `cubic-bezier(.2, .75, .25, 1)`.
pub fn ease(t: f32) -> f32 {
    const X1: f32 = 0.2;
    const Y1: f32 = 0.75;
    const X2: f32 = 0.25;
    const Y2: f32 = 1.0;
    let t = t.clamp(0.0, 1.0);
    if t == 0.0 || t == 1.0 {
        return t;
    }
    let bezier = |p1: f32, p2: f32, u: f32| 3.0 * p1 * u * (1.0 - u) * (1.0 - u) + 3.0 * p2 * u * u * (1.0 - u) + u * u * u;
    // `x` only ever grows along this curve, so bisection finds where it reaches `t`.
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..24 {
        let middle = (low + high) * 0.5;
        if bezier(X1, X2, middle) < t {
            low = middle;
        } else {
            high = middle;
        }
    }
    bezier(Y1, Y2, (low + high) * 0.5)
}
