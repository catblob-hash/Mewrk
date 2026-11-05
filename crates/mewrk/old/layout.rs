//! Which side panes a conversation has open and where they sit, with Mewrk's placement rules.

use std::collections::HashMap;

use fluent::theme::Theme;
use fluent::ui::{rect, CursorKind, Rect, Ui};

pub const CHAT_TILE_FLEX: f32 = 2.0;
const MIN_SIDE_FLEX: f32 = 0.25;
const MAX_SIDE_FLEX: f32 = 8.0;
/// How much wider the side area grows for each column opened after the first: half the chat's weight.
const NEW_COLUMN_FLEX: f32 = CHAT_TILE_FLEX / 2.0;
/// The tallest a column grows: two columns hold six panes.
const MAX_PANES_PER_COLUMN: usize = 3;

pub const GAP: f32 = 12.0;
const MIN_CHAT: f32 = 320.0;
const MIN_SIDE: f32 = 280.0;
pub const TILE_RADIUS: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaneId {
    Terminal,
    Review,
    Preview,
    Files,
    Tasks,
    Settings,
    Subagent,
}

impl PaneId {
    fn default_side_flex(self) -> f32 {
        // The panes that carry a document open as wide as one needs, and the stacked lists narrower.
        match self {
            PaneId::Review | PaneId::Preview | PaneId::Files | PaneId::Settings => 3.0,
            _ => 1.0,
        }
    }

    pub fn tag(self) -> u64 {
        match self {
            PaneId::Terminal => 1,
            PaneId::Review => 2,
            PaneId::Files => 3,
            PaneId::Tasks => 4,
            PaneId::Settings => 5,
            PaneId::Subagent => 6,
            PaneId::Preview => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TasksTab {
    #[default]
    Tasks,
    History,
}

#[derive(Clone, Debug)]
pub struct PaneLayout {
    pub panes: Vec<PaneId>,
    /// Columns left to right, each listing its panes top to bottom. No column is empty.
    pub columns: Vec<Vec<PaneId>>,
    pub side_flex: f32,
    pub column_flex: Vec<f32>,
    pub pane_flex: HashMap<PaneId, f32>,
    /// Shown alone over the whole field. Never persisted.
    pub expanded: Option<PaneId>,
    pub tasks_tab: TasksTab,
}

impl Default for PaneLayout {
    fn default() -> Self {
        Self { panes: Vec::new(), columns: Vec::new(), side_flex: 1.0, column_flex: Vec::new(), pane_flex: HashMap::new(), expanded: None, tasks_tab: TasksTab::default() }
    }
}

fn clamp_flex(v: f32) -> f32 {
    if v.is_nan() { MIN_SIDE_FLEX } else { v.clamp(MIN_SIDE_FLEX, MAX_SIDE_FLEX) }
}

fn column_limit(count: usize) -> usize {
    if count > 2 * MAX_PANES_PER_COLUMN { 3 } else { 2 }
}

/// The column with the fewest panes, the rightmost of equals: it sits next to where a new column would open.
fn shortest(columns: &[Vec<PaneId>]) -> usize {
    let mut best = 0;
    for (i, c) in columns.iter().enumerate() {
        if c.len() <= columns[best].len() {
            best = i;
        }
    }
    best
}

impl PaneLayout {
    pub fn is_open(&self, p: PaneId) -> bool {
        self.panes.contains(&p)
    }

    pub fn open(&mut self, p: PaneId) {
        if self.is_open(p) {
            return;
        }
        self.expanded = None;
        self.panes.push(p);
        if self.columns.is_empty() {
            self.columns.push(vec![p]);
            self.column_flex = vec![1.0];
            self.side_flex = p.default_side_flex();
            return;
        }
        let last = self.columns.len() - 1;
        // A column holding a single pane takes the new one beneath it, and a fuller one is left alone for a new
        // column beside it. Once all the allowed columns are open, the pane joins the shortest.
        let target = if self.columns[last].len() == 1 {
            Some(last)
        } else if self.columns.len() < column_limit(self.panes.len()) {
            None
        } else {
            Some(shortest(&self.columns))
        };
        match target {
            Some(i) => self.columns[i].push(p),
            None => {
                self.columns.push(vec![p]);
                // The chat gives up the room, and the area is then split evenly: weighing the new column against
                // the old ones would leave it a sliver beside a document-wide first column.
                self.column_flex = vec![1.0; self.columns.len()];
                self.side_flex = clamp_flex(self.side_flex + NEW_COLUMN_FLEX);
            }
        }
    }

    pub fn close(&mut self, p: PaneId) {
        if !self.is_open(p) {
            return;
        }
        self.panes.retain(|x| *x != p);
        self.pane_flex.remove(&p);
        if self.expanded == Some(p) {
            self.expanded = None;
        }
        for c in &mut self.columns {
            c.retain(|x| *x != p);
        }
        if let Some(i) = self.columns.iter().position(Vec::is_empty) {
            self.remove_column(i);
        }
        while self.columns.len() > column_limit(self.panes.len()) {
            let dissolved = shortest(&self.columns);
            let moving = self.columns[dissolved].clone();
            self.remove_column(dissolved);
            for id in moving {
                let into = shortest(&self.columns);
                self.columns[into].push(id);
                // A height weighed against its old neighbours means nothing among new ones.
                self.pane_flex.remove(&id);
            }
        }
    }

    fn remove_column(&mut self, index: usize) {
        let weights = self.weights();
        let total: f32 = weights.iter().sum();
        let width = self.side_flex * weights[index] / total;
        self.columns.remove(index);
        let mut rest: Vec<f32> = weights;
        rest.remove(index);
        let sum: f32 = rest.iter().sum();
        self.column_flex = if sum > 0.0 { rest.iter().map(|w| w * rest.len() as f32 / sum).collect() } else { vec![1.0; rest.len()] };
        // Taking back what opening it took, or an opened and closed column would leave the area narrower every time.
        if !self.columns.is_empty() {
            self.side_flex = clamp_flex(self.side_flex - NEW_COLUMN_FLEX.min(width));
        }
    }

    fn weights(&self) -> Vec<f32> {
        (0..self.columns.len()).map(|i| self.column_flex.get(i).copied().filter(|w| *w > 0.0).unwrap_or(1.0)).collect()
    }

    pub fn toggle(&mut self, p: PaneId) {
        if self.is_open(p) {
            self.close(p);
        } else {
            self.open(p);
        }
    }

    pub fn toggle_expand(&mut self, p: PaneId) {
        self.expanded = if self.expanded == Some(p) { None } else { Some(p) };
    }
}

/// A tile's rectangle and corners. A corner is round where it meets the gutter and square where it meets the window.
#[derive(Clone, Copy, Debug)]
pub struct Tile {
    pub r: Rect,
    pub radii: [f32; 4],
}

pub struct Tiles {
    pub chat: Option<Tile>,
    pub panes: Vec<(PaneId, Tile)>,
}

/// Works out where everything goes and handles the dividers. `field` is the whole area under the top bar, reaching the window's right and bottom edges.
pub fn place(ui: &mut Ui<'_>, layout: &mut PaneLayout, field: Rect, id: u64, t: &Theme) -> Tiles {
    let radius = TILE_RADIUS;
    if let Some(p) = layout.expanded.filter(|p| layout.is_open(*p)) {
        return Tiles { chat: None, panes: vec![(p, Tile { r: field, radii: [radius, 0.0, 0.0, radius] })] };
    }
    if layout.columns.is_empty() {
        return Tiles { chat: Some(Tile { r: field, radii: [radius, 0.0, 0.0, radius] }), panes: Vec::new() };
    }

    // Chat against the side area.
    let usable = (field.w - GAP).max(0.0);
    let side_w = (usable * layout.side_flex / (layout.side_flex + CHAT_TILE_FLEX)).clamp(MIN_SIDE.min(usable), (usable - MIN_CHAT).max(MIN_SIDE.min(usable)));
    let chat_w = usable - side_w;
    let chat = Tile { r: rect(field.x, field.y, chat_w, field.h), radii: [radius, radius, radius, radius] };
    let side = rect(field.x + chat_w + GAP, field.y, side_w, field.h);

    let handle = rect(field.x + chat_w, field.y, GAP, field.h);
    if let Some(px) = divider(ui, id, handle, true, t).map(|p| p.0) {
        let new_side = (field.right() - px - GAP * 0.5).clamp(MIN_SIDE, (usable - MIN_CHAT).max(MIN_SIDE));
        layout.side_flex = clamp_flex(CHAT_TILE_FLEX * new_side / (usable - new_side).max(1.0));
    }

    let cols = layout.columns.len();
    let col_area = (side.w - GAP * (cols as f32 - 1.0)).max(0.0);
    let weights = layout.weights();
    let total: f32 = weights.iter().sum();
    let mut out = Vec::with_capacity(layout.panes.len());
    let mut x = side.x;
    for (ci, w) in weights.iter().enumerate() {
        let col_w = col_area * w / total;
        let col = layout.columns[ci].clone();
        let flexes: Vec<f32> = col.iter().map(|p| layout.pane_flex.get(p).copied().unwrap_or(1.0)).collect();
        let flex_total: f32 = flexes.iter().sum();
        let rows_h = (side.h - GAP * (col.len() as f32 - 1.0)).max(0.0);
        let mut y = side.y;
        for (ri, p) in col.iter().enumerate() {
            let h = rows_h * flexes[ri] / flex_total;
            let last_col = ci + 1 == cols;
            let last_row = ri + 1 == col.len();
            let radii = [radius, if last_col && ri == 0 { 0.0 } else { radius }, if last_col && last_row { 0.0 } else { radius }, radius];
            out.push((*p, Tile { r: rect(x, y, col_w, h), radii }));
            y += h;
            if !last_row {
                let hr = rect(x, y, col_w, GAP);
                if let Some((_, py)) = divider(ui, id.wrapping_add(100 + (ci as u64) * 16 + ri as u64), hr, false, t) {
                    let top = y - h;
                    let pair = h + rows_h * flexes[ri + 1] / flex_total;
                    let new_h = (py - top - GAP * 0.5).clamp(40.0, (pair - 40.0).max(40.0));
                    let f = flexes[ri] + flexes[ri + 1];
                    layout.pane_flex.insert(*p, f * new_h / pair.max(1.0));
                    layout.pane_flex.insert(col[ri + 1], f * (pair - new_h) / pair.max(1.0));
                }
                y += GAP;
            }
        }
        x += col_w;
        if ci + 1 < cols {
            let hr = rect(x, side.y, GAP, side.h);
            if let Some((px, _)) = divider(ui, id.wrapping_add(50 + ci as u64), hr, true, t) {
                let left = x - col_w;
                let pair = col_w + col_area * weights[ci + 1] / total;
                let new_w = (px - left - GAP * 0.5).clamp(200.0, (pair - 200.0).max(200.0));
                let f = weights[ci] + weights[ci + 1];
                layout.column_flex = weights.clone();
                layout.column_flex[ci] = f * new_w / pair.max(1.0);
                layout.column_flex[ci + 1] = f * (pair - new_w) / pair.max(1.0);
            }
            x += GAP;
        }
    }
    Tiles { chat: Some(chat), panes: out }
}

/// A draggable gap. Returns the pointer while it is being dragged.
fn divider(ui: &mut Ui<'_>, id: u64, r: Rect, vertical: bool, _t: &Theme) -> Option<(f32, f32)> {
    let resp = ui.interact(id, r);
    if resp.hovered || resp.pressed {
        ui.cursor = if vertical { CursorKind::ColResize } else { CursorKind::RowResize };
    }
    let t = ui.theme;
    if resp.hover_t > 0.0 || resp.pressed {
        let color = if resp.pressed { t.accent } else { t.text_tertiary.fade(resp.hover_t) };
        let (w, h) = if vertical { (3.0, 56.0) } else { (56.0, 3.0) };
        ui.fill(rect(r.x + (r.w - w) * 0.5, r.y + (r.h - h) * 0.5, w, h), 1.5, color);
    }
    if resp.pressed { ui.input.pointer } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PaneId::*;

    #[test]
    fn panes_stack_then_open_a_column_then_fill_the_shortest() {
        let mut l = PaneLayout::default();
        l.open(Terminal);
        assert_eq!(l.columns, vec![vec![Terminal]]);
        assert_eq!(l.side_flex, 1.0);
        l.open(Review);
        assert_eq!(l.columns, vec![vec![Terminal, Review]]);
        l.open(Files);
        assert_eq!(l.columns, vec![vec![Terminal, Review], vec![Files]]);
        assert_eq!(l.side_flex, 2.0);
        l.open(Tasks);
        assert_eq!(l.columns, vec![vec![Terminal, Review], vec![Files, Tasks]]);
        l.open(Settings);
        assert_eq!(l.columns, vec![vec![Terminal, Review], vec![Files, Tasks, Settings]]);
    }

    #[test]
    fn a_column_opened_and_closed_leaves_the_width_it_found() {
        let mut l = PaneLayout::default();
        l.open(Review);
        l.open(Terminal);
        let before = l.side_flex;
        l.open(Files);
        l.close(Files);
        assert_eq!(l.side_flex, before);
        assert_eq!(l.columns.len(), 1);
    }

    #[test]
    fn closing_the_expanded_pane_ends_the_expansion() {
        let mut l = PaneLayout::default();
        l.open(Review);
        l.toggle_expand(Review);
        assert_eq!(l.expanded, Some(Review));
        l.close(Review);
        assert_eq!(l.expanded, None);
    }
}
