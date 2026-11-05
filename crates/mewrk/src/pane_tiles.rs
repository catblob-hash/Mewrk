//! `PaneTiles.tsx`: the chat and the side panes as tiles of one field, with draggable dividers between them.
//!
//! The tiles are siblings in one element, placed by numbers worked out here, so a pane that moves to another column keeps
//! its identity. A `hidden` pane is not given to this at all: a surface that has to stay mounted behind a closed pane is
//! called and its elements dropped, which is what the terminal's signature asks for.

use std::collections::BTreeMap;

use mewrk_client::prelude::*;
use mewrk_client::side_panes::{default_side_flex_for_kind, pane_kind, CHAT_TILE_FLEX, MAX_SIDE_FLEX, MIN_SIDE_FLEX, NEW_COLUMN_FLEX};

use crate::chrome::TILE_RADIUS;

/// The gutter between tiles.
pub const GAP: f32 = 12.0;
pub const MIN_CHAT_WIDTH: f32 = 320.0;
pub const MIN_SIDE_WIDTH: f32 = 280.0;
pub const MIN_COLUMN_WIDTH: f32 = 200.0;
pub const MIN_TILE_HEIGHT: f32 = 100.0;
/// The 8 px under the top bar.
const FIELD_TOP: f32 = 8.0;

/// `.pane-tiles`' top: over a background the tiles rise into the bar's foot instead of keeping their distance.
fn field_top() -> f32 {
    if crate::glass::layers().layered() { -crate::glass::TILE_RISE } else { FIELD_TOP }
}

/// `minimumSideWidth`: the narrowest the side area may be dragged while it shows this many columns.
pub fn minimum_side_width(columns: usize) -> f32 {
    MIN_SIDE_WIDTH.max(columns as f32 * MIN_COLUMN_WIDTH + (columns.saturating_sub(1)) as f32 * GAP)
}

fn weight_of(value: Option<f64>) -> f64 {
    value.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(1.0)
}

#[derive(Clone, Debug, PartialEq)]
pub struct TileColumn {
    /// The column's index in `columns`, which `column_flex` follows.
    pub index: usize,
    pub weight: f64,
    pub panes: Vec<String>,
}

/// `arrangeColumns`: groups the visible panes into the columns they are drawn in, dropping columns with none.
pub fn arrange_columns(visible: &[String], columns: &[Vec<String>], column_flex: &[f64]) -> Vec<TileColumn> {
    let mut placed: Vec<String> = Vec::new();
    let mut arranged = Vec::new();
    for (index, column) in columns.iter().enumerate() {
        let ids: Vec<String> = column.iter().filter(|id| visible.contains(id) && !placed.contains(id)).cloned().collect();
        placed.extend(ids.iter().cloned());
        if !ids.is_empty() {
            arranged.push(TileColumn { index, weight: weight_of(column_flex.get(index).copied()), panes: ids });
        }
    }
    let rest: Vec<String> = visible.iter().filter(|id| !placed.contains(id)).cloned().collect();
    if !rest.is_empty() {
        arranged.push(TileColumn { index: columns.len(), weight: weight_of(column_flex.get(columns.len()).copied()), panes: rest });
    }
    arranged
}

/// `sharesOf`: `(start, size)` fractions.
pub fn shares_of(weights: &[f64]) -> Vec<(f64, f64)> {
    let total: f64 = weights.iter().sum();
    let mut start = 0.0;
    weights
        .iter()
        .map(|weight| {
            let share = (start / total, weight / total);
            start += weight;
            share
        })
        .collect()
}

/// `chat_w` and `side_w` as flexbox settles them: grow by weight, then the minimums win.
pub fn split_field(inner_w: f32, side_flex: f64, columns: usize) -> (f32, f32) {
    if columns == 0 {
        return (inner_w, 0.0);
    }
    let available = (inner_w - GAP).max(0.0);
    let side_min = minimum_side_width(columns);
    let total = CHAT_TILE_FLEX + side_flex;
    let mut chat = available * (CHAT_TILE_FLEX / total) as f32;
    let mut side = available - chat;
    if side < side_min {
        side = side_min;
        chat = (available - side).max(MIN_CHAT_WIDTH);
    } else if chat < MIN_CHAT_WIDTH {
        chat = MIN_CHAT_WIDTH;
        side = (available - chat).max(side_min);
    }
    (chat, side)
}

/// Where a tile lies in the side area: the stylesheet's placement formulas.
pub fn tile_rect(side: Rect, columns: usize, column: usize, column_share: (f64, f64), rows: usize, row_index: usize, row_share: (f64, f64)) -> Rect {
    let w_room = side.w - (columns as f32 - 1.0) * GAP;
    let h_room = side.h - (rows as f32 - 1.0) * GAP;
    rect(side.x + w_room * column_share.0 as f32 + column as f32 * GAP, side.y + h_room * row_share.0 as f32 + row_index as f32 * GAP, w_room * column_share.1 as f32, (h_room * row_share.1 as f32).max(32.0))
}

/// `ResizeMeasure`, as numbers taken when a drag begins.
#[derive(Clone, Debug, PartialEq)]
pub enum Measure {
    Side { chat_w: f32, side_w: f32, minimum: f32, maximum: f32, start_flex: f64 },
    Columns { first_w: f32, second_w: f32, sum: f64, first_index: usize, second_index: usize, given: Vec<f64>, minimum: f32, maximum: f32 },
    Rows { first: String, second: String, first_h: f32, second_h: f32, sum: f64, measured: BTreeMap<String, f64>, start: BTreeMap<String, f64>, minimum: f32, maximum: f32 },
}

impl Measure {
    pub fn minimum(&self) -> f32 {
        match self {
            Self::Side { minimum, .. } | Self::Columns { minimum, .. } | Self::Rows { minimum, .. } => *minimum,
        }
    }

    pub fn maximum(&self) -> f32 {
        match self {
            Self::Side { maximum, .. } | Self::Columns { maximum, .. } | Self::Rows { maximum, .. } => *maximum,
        }
    }

    /// What the props become after the handle moved by `delta`.
    pub fn apply(&self, delta: f32) -> ResizeChange {
        match self {
            // The chat tile gives up exactly the pixels the side area gains, so the ratio must use both post-drag widths or
            // the handle drifts away from the pointer.
            Self::Side { chat_w, side_w, minimum, maximum, .. } => {
                let moved = delta.clamp(*minimum, *maximum);
                ResizeChange::SideFlex((f64::from(side_w - moved) / f64::from(chat_w + moved) * CHAT_TILE_FLEX).clamp(MIN_SIDE_FLEX, MAX_SIDE_FLEX))
            }
            Self::Columns { first_w, second_w, sum, first_index, second_index, given, minimum, maximum } => {
                let width = first_w + second_w;
                let first = f64::from(first_w + delta.clamp(*minimum, *maximum)) / f64::from(width) * sum;
                let mut next = given.clone();
                next[*first_index] = first;
                next[*second_index] = sum - first;
                ResizeChange::ColumnFlex(next)
            }
            Self::Rows { first, second, first_h, second_h, sum, measured, start, minimum, maximum } => {
                let movement = delta.clamp(*minimum, *maximum);
                if movement == 0.0 {
                    return ResizeChange::PaneFlex(start.clone());
                }
                let height = first_h + second_h;
                let first_flex = f64::from(first_h + movement) / f64::from(height) * sum;
                let mut next = start.clone();
                next.extend(measured.iter().map(|(k, v)| (k.clone(), *v)));
                next.insert(first.clone(), first_flex);
                next.insert(second.clone(), sum - first_flex);
                ResizeChange::PaneFlex(next)
            }
        }
    }

    pub fn restore(&self) -> ResizeChange {
        match self {
            Self::Side { start_flex, .. } => ResizeChange::SideFlex(*start_flex),
            Self::Columns { given, .. } => ResizeChange::ColumnFlex(given.clone()),
            Self::Rows { start, .. } => ResizeChange::PaneFlex(start.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResizeChange {
    SideFlex(f64),
    ColumnFlex(Vec<f64>),
    PaneFlex(BTreeMap<String, f64>),
}

#[derive(Clone)]
struct ResizeSession {
    measure: Measure,
    handle: String,
    layout_key: String,
}

pub struct PaneTilesProps {
    pub conversation_id: String,
    /// The field's whole size: below the top bar, right of the sidebar.
    pub field: (f32, f32),
    /// Where the field lies in the window, for the glass under its tiles.
    pub field_at: (f32, f32),
    /// `--field-left`: 8 beside the sidebar, 0 with it closed.
    pub field_left: f32,
    pub chat: E,
    pub panes: Vec<(String, E)>,
    pub columns: Vec<Vec<String>>,
    pub side_flex: f64,
    pub column_flex: Vec<f64>,
    pub pane_flex: BTreeMap<String, f64>,
    pub expanded: Option<String>,
    pub on_change: Rc<dyn Fn(&mut App, ResizeChange)>,
    /// The cursor of the drag that began, or `None` when it ended.
    pub on_resize_state: Rc<dyn Fn(&mut App, Option<CursorKind>)>,
}

/// Every rectangle of a frame, field-relative, worked out where a test can read them.
#[derive(Clone, Debug, PartialEq)]
pub struct TileLayout {
    pub arranged: Vec<TileColumn>,
    pub chat: Rect,
    pub side: Rect,
    pub tiles: Vec<(String, Rect)>,
    pub solo: Option<String>,
}

/// The arithmetic of `PaneTiles`.
pub fn tile_layout(field: (f32, f32), field_left: f32, visible: &[String], columns: &[Vec<String>], side_flex: f64, column_flex: &[f64], pane_flex: &BTreeMap<String, f64>, expanded: Option<&str>) -> TileLayout {
    // A pane whose subject vanished is not in `visible`; expanding it would leave an empty workspace.
    let solo = expanded.filter(|id| visible.iter().any(|pane| pane == id)).map(str::to_owned);
    let arranged = arrange_columns(visible, columns, column_flex);
    let inner_w = field.0 - field_left;
    let field_top = field_top();
    let inner_h = field.1 - field_top;
    let (chat_w, side_w) = split_field(inner_w, side_flex, arranged.len());
    let chat = rect(field_left, field_top, chat_w, inner_h);
    let side = if solo.is_some() { rect(field_left, field_top, inner_w, inner_h) } else { rect(field_left + chat_w + GAP, field_top, side_w, inner_h) };
    let column_shares = shares_of(&arranged.iter().map(|column| column.weight).collect::<Vec<_>>());
    let mut tiles = Vec::new();
    for (index, column) in arranged.iter().enumerate() {
        let weights: Vec<f64> = column.panes.iter().map(|id| weight_of(pane_flex.get(id).copied())).collect();
        let row_shares = shares_of(&weights);
        for (row_index, id) in column.panes.iter().enumerate() {
            tiles.push((id.clone(), tile_rect(side, arranged.len(), index, column_shares[index], column.panes.len(), row_index, row_shares[row_index])));
        }
    }
    if let Some(id) = &solo
        && let Some(entry) = tiles.iter_mut().find(|(pane, _)| pane == id)
    {
        entry.1 = side;
    }
    TileLayout { arranged, chat, side, tiles, solo }
}

fn measure_side(layout: &TileLayout, side_flex: f64) -> Option<Measure> {
    let side_min = minimum_side_width(layout.arranged.len());
    let (chat_w, side_w) = (layout.chat.w, layout.side.w);
    if chat_w < MIN_CHAT_WIDTH || side_w < side_min {
        return None;
    }
    Some(Measure::Side { chat_w, side_w, minimum: MIN_CHAT_WIDTH - chat_w, maximum: side_w - side_min, start_flex: side_flex })
}

fn given_column_weights(layout: &TileLayout, columns: &[Vec<String>], column_flex: &[f64]) -> Vec<f64> {
    let len = columns.len().max(layout.arranged.iter().map(|column| column.index + 1).max().unwrap_or(0));
    (0..len).map(|index| weight_of(column_flex.get(index).copied())).collect()
}

fn measure_columns(layout: &TileLayout, index: usize, columns: &[Vec<String>], column_flex: &[f64]) -> Option<Measure> {
    let first = layout.arranged.get(index)?;
    let second = layout.arranged.get(index + 1)?;
    let width_of = |column: &TileColumn| layout.tiles.iter().find(|(id, _)| *id == column.panes[0]).map_or(0.0, |(_, r)| r.w);
    let (first_w, second_w) = (width_of(first), width_of(second));
    if first_w <= 0.0 || second_w <= 0.0 {
        return None;
    }
    // A column already under its floor cannot be made narrower, but must still be draggable wider: refusing the whole
    // drag left the divider dead exactly when a column most needed room.
    Some(Measure::Columns { first_w, second_w, sum: first.weight + second.weight, first_index: first.index, second_index: second.index, given: given_column_weights(layout, columns, column_flex), minimum: (MIN_COLUMN_WIDTH - first_w).min(0.0), maximum: (second_w - MIN_COLUMN_WIDTH).max(0.0) })
}

fn measure_rows(layout: &TileLayout, column: usize, index: usize, pane_flex: &BTreeMap<String, f64>) -> Option<Measure> {
    let ids = &layout.arranged.get(column)?.panes;
    let (first, second) = (ids.get(index)?.clone(), ids.get(index + 1)?.clone());
    let height_of = |id: &str| layout.tiles.iter().find(|(pane, _)| pane == id).map_or(0.0, |(_, r)| r.h);
    let (first_h, second_h) = (height_of(&first), height_of(&second));
    if first_h <= 0.0 || second_h <= 0.0 {
        return None;
    }
    let height = first_h + second_h;
    let sum = weight_of(pane_flex.get(&first).copied()) + weight_of(pane_flex.get(&second).copied());
    // A floor-constrained tile's measured height is not proportional to its flex. One pixel-to-flex scale for the
    // column's tiles keeps its unrelated separators fixed.
    let measured = ids.iter().map(|id| (id.clone(), f64::from(height_of(id)) / f64::from(height) * sum)).collect();
    Some(Measure::Rows { first, second, first_h, second_h, sum, measured, start: pane_flex.clone(), minimum: (MIN_TILE_HEIGHT - first_h).min(0.0), maximum: (second_h - MIN_TILE_HEIGHT).max(0.0) })
}

/// Arrow keys move the handle; Home and End take the value it reports to its end. The chat's handle reports the side
/// area's share, which grows as the handle moves left, so its Home and End run the other way.
pub fn keyboard_delta(press: &KeyPress, along_x: bool, measure: &Measure, reversed: bool) -> Option<f32> {
    let step = if press.shift { 48.0 } else { 24.0 };
    match press.key {
        Key::Left if along_x => Some(-step),
        Key::Right if along_x => Some(step),
        Key::Up if !along_x => Some(-step),
        Key::Down if !along_x => Some(step),
        Key::Home => Some(if reversed { measure.maximum() } else { measure.minimum() }),
        Key::End => Some(if reversed { measure.minimum() } else { measure.maximum() }),
        _ => None,
    }
}

fn handle_pill(active: bool, along_x: bool) -> E {
    let t_ = theme();
    let (w, h) = if along_x { (3.0, 56.0) } else { (56.0, 3.0) };
    let pill: E = div().size(w, h).radius(99.0).bg(if active { t_.text_primary } else { t_.text_primary.with_alpha(0.2) }).pass_through();
    if active { pill } else { pill.reveal_on_parent_hover() }
}

/// The corners of a tile against the edges it lies on: square where it meets the window, round against the gutter.
fn corners(top: bool, right: bool, bottom: bool, left_corner: f32, on_left: bool) -> [f32; 4] {
    let r = TILE_RADIUS;
    [if top && on_left { left_corner } else { r }, if top && right { 0.0 } else { r }, if bottom && right { 0.0 } else { r }, if bottom && on_left { left_corner } else { r }]
}

pub fn chat_ground() -> Color {
    let t_ = theme();
    if crate::glass::layers().glass {
        return crate::glass::glass_tile();
    }
    if t_.dark { Color::rgb(0x262626) } else { Color::rgb(0xFBFBF9) }
}

pub fn gutter_ground() -> Color {
    if crate::glass::layers().layered() {
        return Color::TRANSPARENT;
    }
    solid_gutter()
}

/// The tile gutter's colour, which is also `.app-backdrop`'s solid ground.
pub fn solid_gutter() -> Color {
    let t_ = theme();
    if t_.dark { Color::rgb(0x161616) } else { Color::rgb(0xE7E7E2) }
}

pub fn pane_ground() -> Color {
    let t_ = theme();
    if crate::glass::layers().glass {
        return crate::glass::glass_pane();
    }
    if t_.dark { Color::rgb(0x202020) } else { Color::rgb(0xF4F4F1) }
}

pub fn pane_tiles(p: PaneTilesProps) -> E {
    let t_ = theme();
    let PaneTilesProps { conversation_id: _, field, field_at, field_left, chat, panes, columns, side_flex, column_flex, pane_flex, expanded, on_change, on_resize_state } = p;
    let visible: Vec<String> = panes.iter().map(|(id, _)| id.clone()).collect();
    let layout = tile_layout(field, field_left, &visible, &columns, side_flex, &column_flex, &pane_flex, expanded.as_deref());
    let solo = layout.solo.clone();
    let layout_key = format!("{visible:?}|{solo:?}|{:?}", layout.arranged.iter().map(|column| &column.panes).collect::<Vec<_>>());
    let session = use_state("pane-tiles-resize", || None::<ResizeSession>);
    // `useLayoutEffect`: a layout that changed under a drag ends it.
    if session.with(|s| s.as_ref().is_some_and(|s| s.layout_key != layout_key)) {
        session.set(None);
        let report = on_resize_state.clone();
        fluent_defer(move |app| report(app, None));
    }
    let active = session.with(|s| s.as_ref().map(|s| s.handle.clone()));
    let left_corner = if field_left > 0.0 { TILE_RADIUS } else { 0.0 };

    let make_handle = |handle_key: String, name: &'static str, share: f64, along_x: bool, reversed: bool, measure: Option<Measure>, r: Rect, reset: Handler<App>, double: bool| -> E {
        let (press, on_change_press, on_state, layout_key_c, handle_key_c, measure_c) = (session.clone(), on_change.clone(), on_resize_state.clone(), layout_key.clone(), handle_key.clone(), measure.clone());
        let is_active = active.as_deref() == Some(handle_key.as_str());
        let mut handle: E = row()
            .key(("pane-tiles-handle", handle_key.as_str()))
            .role(Role::Splitter)
            .aria_label(name)
            .aria_orientation(if along_x { Orientation::Vertical } else { Orientation::Horizontal })
            .aria_value((100.0 * share).round(), 0.0, 100.0)
            .absolute()
            .left(r.x)
            .top(r.y)
            .size(r.w, r.h)
            .center()
            .cursor(if along_x { CursorKind::ColResize } else { CursorKind::RowResize })
            .focusable()
            .child(handle_pill(is_active, along_x))
            .on_pointer(move |app: &mut App, e| match e.phase {
                Phase::Down => {
                    if let Some(measure) = measure_c.clone() {
                        press.set(Some(ResizeSession { measure, handle: handle_key_c.clone(), layout_key: layout_key_c.clone() }));
                        on_state(app, Some(if along_x { CursorKind::ColResize } else { CursorKind::RowResize }));
                    }
                }
                Phase::Move => {
                    let delta = if along_x { e.dx } else { e.dy };
                    if let Some(s) = press.borrow().as_ref() {
                        let change = s.measure.apply(delta);
                        on_change_press(app, change);
                    }
                }
                Phase::Up => {
                    if press.update(Option::take).is_some() {
                        on_state(app, None);
                    }
                }
            });
        if double {
            handle = handle.on_double_click(move |app: &mut App| reset(app));
        }
        let (key_change, key_session, key_measure) = (on_change.clone(), session.clone(), measure);
        handle = handle.on_key(move |app: &mut App, press: &KeyPress| {
            let Some(measure) = &key_measure else { return false };
            if key_session.with(Option::is_some) {
                return false;
            }
            let Some(delta) = keyboard_delta(press, along_x, measure, reversed) else { return false };
            key_change(app, measure.apply(delta));
            true
        });
        handle
    };

    let mut field_el: E = div().key("pane-tiles").flex1().min_h(0.0).min_w(0.0).bg(gutter_ground());

    // ---- the chat
    let any_visible = !visible.is_empty();
    let chat_corners = corners(true, !any_visible, true, left_corner, true);
    let field_top = field_top();
    let chat_rect = if any_visible { layout.chat } else { rect(field_left, field_top, field.0 - field_left, field.1 - field_top) };
    let in_window = |r: Rect| rect(field_at.0 + r.x, field_at.1 + r.y, r.w, r.h);
    if solo.is_none() {
        crate::glass::surface(in_window(chat_rect), chat_corners);
    }
    field_el = field_el.child(col().key("pane-tiles-chat").when(solo.is_some(), |chat_tile| chat_tile.invisible()).absolute().left(chat_rect.x).top(chat_rect.y).size(chat_rect.w, chat_rect.h).radii(chat_corners).bg(chat_ground()).when(crate::glass::layers().glass, |chat_tile| chat_tile.border(crate::glass::glass_edge())).clip().child(chat));

    if any_visible && solo.is_none() {
        let first = layout.arranged[0].panes[0].clone();
        let (reset_change, columns_n) = (on_change.clone(), layout.arranged.len());
        let reset: Handler<App> = Rc::new(move |app| reset_change(app, ResizeChange::SideFlex(default_side_flex_for_kind(pane_kind(&first)) + (columns_n as f64 - 1.0) * NEW_COLUMN_FLEX)));
        let r = rect(layout.chat.right(), field_top, GAP, layout.chat.h);
        field_el = field_el.child(make_handle("side".to_owned(), t("调整面板宽度", "Resize panes"), side_flex / (CHAT_TILE_FLEX + side_flex), true, true, measure_side(&layout, side_flex), r, reset, true));
    }

    // ---- the tiles
    let mut solo_tile = None;
    for (pane, node) in panes {
        let Some((_, r)) = layout.tiles.iter().find(|(id, _)| *id == pane) else { continue };
        let r = *r;
        let place = layout.arranged.iter().enumerate().find_map(|(c, column)| column.panes.iter().position(|id| *id == pane).map(|row_index| (c, row_index, column.panes.len())));
        let is_solo = solo.as_deref() == Some(pane.as_str());
        let radii = if is_solo {
            corners(true, true, true, left_corner, true)
        } else if let Some((c, row_index, rows)) = place {
            corners(row_index == 0, c == layout.arranged.len() - 1, row_index == rows - 1, 0.0, false)
        } else {
            [TILE_RADIUS; 4]
        };
        if solo.is_none() || is_solo {
            crate::glass::surface(in_window(r), radii);
        }
        let mut tile: E = col().key(("pane-tile", pane.as_str())).absolute().left(r.x).top(r.y).size(r.w, r.h).radii(radii).bg(pane_ground()).border(if crate::glass::layers().glass { crate::glass::glass_edge() } else { t_.card_stroke }).clip().child(node);
        if is_solo {
            solo_tile = Some(tile);
        } else if solo.is_none() {
            field_el = field_el.child(tile);
        } else {
            // Covered: it keeps its box and stays mounted, but is not drawn. Lying under the one on show would not hide it: a tile's ground may be glass, and text lying in the layer it is drawn in goes over its ground.
            field_el = field_el.child(tile.invisible());
        }
    }
    if solo.is_none() {
        // ---- the dividers inside the side area
        for (c, column) in layout.arranged.iter().enumerate() {
            for row_index in 0..column.panes.len().saturating_sub(1) {
                let upper = column.panes[row_index].clone();
                let lower = column.panes[row_index + 1].clone();
                let Some((_, u)) = layout.tiles.iter().find(|(id, _)| *id == upper) else { continue };
                let r = rect(u.x, u.bottom(), u.w, GAP);
                let (upper_weight, lower_weight) = (weight_of(pane_flex.get(&upper).copied()), weight_of(pane_flex.get(&lower).copied()));
                let (reset_change, reset_flex) = (on_change.clone(), pane_flex.clone());
                let reset: Handler<App> = Rc::new(move |app| {
                    let mut next = reset_flex.clone();
                    next.insert(upper.clone(), 1.0);
                    next.insert(lower.clone(), 1.0);
                    reset_change(app, ResizeChange::PaneFlex(next));
                });
                field_el = field_el.child(make_handle(format!("rows:{c}:{row_index}"), t("调整面板高度", "Resize panes"), upper_weight / (upper_weight + lower_weight), false, false, measure_rows(&layout, c, row_index, &pane_flex), r, reset, true));
            }
            if c + 1 < layout.arranged.len() {
                let Some((_, first)) = layout.tiles.iter().find(|(id, _)| *id == column.panes[0]) else { continue };
                let r = rect(first.right(), field_top, GAP, layout.side.h);
                let (left, right) = (column.clone(), layout.arranged[c + 1].clone());
                let share = left.weight / (left.weight + right.weight);
                let (reset_change, given) = (on_change.clone(), given_column_weights(&layout, &columns, &column_flex));
                let reset: Handler<App> = Rc::new(move |app| {
                    let mut next = given.clone();
                    next[left.index] = (left.weight + right.weight) / 2.0;
                    next[right.index] = (left.weight + right.weight) / 2.0;
                    reset_change(app, ResizeChange::ColumnFlex(next));
                });
                field_el = field_el.child(make_handle(format!("columns:{c}"), t("调整列宽度", "Resize columns"), share, true, false, measure_columns(&layout, c, &columns, &column_flex), r, reset, true));
            }
        }
    }
    if let Some(tile) = solo_tile {
        field_el = field_el.child(tile);
    }
    if session.with(Option::is_some) {
        // Escape puts everything back.
        let (cancel, restore_change, on_state) = (session.clone(), on_change.clone(), on_resize_state.clone());
        field_el = field_el.on_key_global(move |app: &mut App, press| {
            if press.key != Key::Escape {
                return false;
            }
            if let Some(s) = cancel.update(Option::take) {
                restore_change(app, s.measure.restore());
                on_state(app, None);
            }
            true
        });
    }
    field_el
}

/// A state change a view found it has to make; runs before the next frame.
pub fn fluent_defer(f: impl Fn(&mut App) + 'static) {
    DEFERRED.with(|d| d.borrow_mut().push(Rc::new(f)));
    repaint();
}

thread_local! {
    static DEFERRED: std::cell::RefCell<Vec<Rc<dyn Fn(&mut App)>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs what views deferred. The frame calls this before building the view.
pub fn run_deferred(app: &mut App) {
    let jobs: Vec<_> = DEFERRED.with(|d| std::mem::take(&mut *d.borrow_mut()));
    for job in jobs {
        job(app);
    }
}
