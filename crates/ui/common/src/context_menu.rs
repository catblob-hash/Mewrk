//! `ContextMenu.tsx`, `lib/menuPlacement.ts` and `MenuFlyout.tsx`: a menu opened by a right click or by a row's `⋮`, for acting on
//! one thing. It shares `PopoverMenu`'s look, but not its trigger: the thing it acts on is a row or a link, which has no button
//! to anchor to. A row with a submenu opens it beside itself when the pointer rests on it, and a submenu can be read when it
//! first opens, so a menu that offers the programs that open a file only asks the system once someone wants to know.
//!
//! ```ignore
//! let menu = context_menu(("file-menu", id), ContextMenuProps::new(
//!     ContextMenuAnchor::Point { x, y }, t("文件", "File"), on_close,
//!     vec![ContextMenuSection::new("file", vec![ContextMenuItem::new("open", "Open").on_select(open_it)])],
//! ));
//! ```
//!
//! The panel is lifted out of the tree wherever it is written, and each submenu is a panel of its own. What tests look a row up by is
//! `menu_row_key`.

use crate::path_text::path_text;
use mewrk_client::prelude::*;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// How close a menu may come to the window's edge.
const VIEWPORT_MARGIN: f32 = 8.0;
const ANCHOR_GAP: f32 = 4.0;
/// How long the pointer rests on a row before its submenu opens or another one closes.
const HOVER_DELAY_SECONDS: f32 = 0.12;
/// `.popover-menu__panel--dense`'s padding and border, around a submenu's first row.
const PANEL_INSET: f32 = 4.0;
const BORDER: f32 = 1.0;

// ---------------------------------------------------------------- placement

/// Where a menu goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MenuPlacement {
    pub left: f32,
    pub top: f32,
    /// The menu stands on its anchor instead of hanging from it; its submenus grow up from their rows to match.
    pub above: bool,
}

/// Where a menu opens: at a point (a right click), or against a box (the control that opened it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ContextMenuAnchor {
    Point { x: f32, y: f32 },
    /// Lined up with the box's start edge, or its end edge.
    Box { rect: Rect, align_end: bool },
}

/// Where a menu opened at a point goes, given its measured size.
pub fn place_menu(x: f32, y: f32, w: f32, h: f32, window_w: f32, window_h: f32) -> MenuPlacement {
    place_menu_at(ContextMenuAnchor::Point { x, y }, 0.0, w, h, window_w, window_h)
}

/// Where a menu goes, given where it was opened and its measured size.
///
/// Where it was opened decides the side. In the window's upper half the menu hangs down from the spot; in the lower half it
/// stands on it, so a right click just above the composer opens over the timeline rather than down into the composer. That half
/// is always the roomier side, so there is no better one to try: a menu too tall for it is held inside the window.
///
/// Across, a point opens to its right, or to its left when only the left has room, as a desktop's own context menus do. A box
/// lines the menu up with its start or end edge.
pub fn place_menu_at(anchor: ContextMenuAnchor, gap: f32, w: f32, h: f32, window_w: f32, window_h: f32) -> MenuPlacement {
    let (viewport_w, viewport_h) = (if window_w > 0.0 { window_w } else { w }, if window_h > 0.0 { window_h } else { h });
    let (left_edge, top_edge, right_edge, bottom_edge) = match anchor {
        ContextMenuAnchor::Point { x, y } => (x, y, x, y),
        ContextMenuAnchor::Box { rect, .. } => (rect.x, rect.y, rect.right(), rect.bottom()),
    };
    let gap = if matches!(anchor, ContextMenuAnchor::Box { .. }) { gap } else { 0.0 };
    let above = (top_edge + bottom_edge) / 2.0 > viewport_h / 2.0;
    let preferred_top = if above { top_edge - gap - h } else { bottom_edge + gap };
    let top = preferred_top.max(VIEWPORT_MARGIN).min((viewport_h - h - VIEWPORT_MARGIN).max(VIEWPORT_MARGIN));
    let preferred_left = match anchor {
        ContextMenuAnchor::Box { align_end, .. } => {
            if align_end {
                right_edge - w
            } else {
                left_edge
            }
        }
        ContextMenuAnchor::Point { x, .. } => {
            let room_right = x + w <= viewport_w - VIEWPORT_MARGIN;
            if room_right || x - w < VIEWPORT_MARGIN { x } else { x - w }
        }
    };
    let left = preferred_left.max(VIEWPORT_MARGIN).min((viewport_w - w - VIEWPORT_MARGIN).max(VIEWPORT_MARGIN));
    MenuPlacement { left, top, above }
}

/// Which way a submenu grows from its row: down from the row's top, or up from its foot for a menu standing on its anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MenuHang {
    #[default]
    Down,
    Up,
}

/// Where a submenu goes: beside its panel, its edge over the panel's own, and lined up with its row. It opens to the right, or
/// to the left when the right has no room; it moves up, or down, by however much would leave the window.
#[allow(clippy::too_many_arguments)]
pub fn place_submenu(row: Rect, parent: Rect, w: f32, h: f32, inset: f32, hang: MenuHang, window_w: f32, window_h: f32) -> (f32, f32) {
    let right = parent.right() - BORDER;
    let left = if right + w > window_w - VIEWPORT_MARGIN { VIEWPORT_MARGIN.max(parent.x + BORDER - w) } else { right };
    let preferred_top = match hang {
        MenuHang::Down => row.y - inset,
        MenuHang::Up => row.bottom() + inset - h,
    };
    let top = VIEWPORT_MARGIN.max(preferred_top.min(window_h - VIEWPORT_MARGIN - h));
    (left, top)
}

/// `MenuFlyout`: `panel` as a panel of its own beside the row that opened it, and in the same stacking as its menu. `row` and
/// `parent` are what was given to `.key(...)` of the row, and of the panel it lies in.
pub fn menu_flyout(key: impl Hash, row: u64, parent: u64, panel: E, hang: MenuHang, on_dismiss: impl Fn(&mut App) + 'static) -> E {
    let id = flyout_key(hash_key(key));
    let (window_w, window_h) = window_size();
    let placed = match (rect_of(id), rect_of(row), rect_of(parent)) {
        (Some(own), Some(row), Some(parent)) => Some(place_submenu(row, parent, own.w, own.h, PANEL_INSET, hang, window_w, window_h)),
        _ => None,
    };
    let (left, top) = placed.unwrap_or((0.0, 0.0));
    let shown = panel.key(id).popover_at_anchored(hash_key(row), left, top, on_dismiss);
    if placed.is_none() {
        // What it lists decides its size, which is only known once it has been laid out once.
        repaint();
        shown.opacity(0.0)
    } else {
        shown
    }
}

fn flyout_key(key: u64) -> u64 {
    hash_key(("context-flyout", key))
}

// ---------------------------------------------------------------- what a menu says

/// A section's rows, read the first time its row is reached. Answer through the `MenuLoader`, on the UI thread: the sections hold
/// closures, so an `app.invoke` builds them in its `done`. The loader itself travels to that closure.
pub type LoadChildren = Rc<dyn Fn(&mut App, MenuLoader)>;
pub type MenuHandler = Rc<dyn Fn(&mut App)>;

/// How a submenu read on opening answers. Reaches its menu by name, so answering after the menu closed does nothing.
#[derive(Clone, Debug)]
pub struct MenuLoader {
    level: u64,
    item: String,
}

impl MenuLoader {
    pub fn resolve(&self, sections: Vec<ContextMenuSection>) {
        level_state(self.level).update(|s| {
            s.loaded.insert(self.item.clone(), Ok(Rc::new(sections)));
        });
    }

    pub fn reject(&self, message: impl Into<String>) {
        let message = message.into();
        level_state(self.level).update(|s| {
            s.loaded.insert(self.item.clone(), Err(message));
        });
    }
}

/// A row of a context menu.
#[derive(Clone, Default)]
pub struct ContextMenuItem {
    pub id: String,
    pub label: String,
    /// The label is a path, drawn so that it gives way in the middle.
    pub label_is_path: bool,
    pub icon: Option<Rc<dyn Fn() -> E>>,
    /// Secondary text under the label, such as the path a workspace is at.
    pub description: Option<String>,
    /// The description is a path, drawn so that it gives way in the middle.
    pub description_is_path: bool,
    /// Subtle trailing text: the keys that do what the row does, say.
    pub hint: Option<String>,
    pub disabled: bool,
    /// On a disabled row, why: drawn on hover. Otherwise what a cut line in the row is short for.
    pub title: Option<String>,
    /// Drawn as a destructive action.
    pub danger: bool,
    pub on_select: Option<MenuHandler>,
    /// A submenu, opened beside the row on hover or with the right arrow key.
    pub children: Option<Rc<Vec<ContextMenuSection>>>,
    /// A submenu read the first time it opens: the programs that open a file, say.
    pub load_children: Option<LoadChildren>,
}

impl ContextMenuItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), ..Self::default() }
    }

    pub fn label_is_path(mut self) -> Self {
        self.label_is_path = true;
        self
    }

    pub fn icon(mut self, icon_ref: impl Into<IconRef>) -> Self {
        let icon_ref = icon_ref.into();
        self.icon = Some(Rc::new(move || icon(icon_ref, 14.0)));
        self
    }

    /// An icon that is not one of the line icons: a program's own, say.
    pub fn icon_el(mut self, f: impl Fn() -> E + 'static) -> Self {
        self.icon = Some(Rc::new(f));
        self
    }

    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub fn description_is_path(mut self) -> Self {
        self.description_is_path = true;
        self
    }

    pub fn hint(mut self, text: impl Into<String>) -> Self {
        self.hint = Some(text.into());
        self
    }

    pub fn disabled(mut self, on: bool) -> Self {
        self.disabled = on;
        self
    }

    pub fn title(mut self, text: impl Into<String>) -> Self {
        self.title = Some(text.into());
        self
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub fn on_select(mut self, f: impl Fn(&mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(f));
        self
    }

    pub fn children(mut self, sections: Vec<ContextMenuSection>) -> Self {
        self.children = Some(Rc::new(sections));
        self
    }

    pub fn load_children(mut self, f: impl Fn(&mut App, MenuLoader) + 'static) -> Self {
        self.load_children = Some(Rc::new(f));
        self
    }

    fn is_branch(&self) -> bool {
        self.children.is_some() || self.load_children.is_some()
    }
}

#[derive(Clone, Default)]
pub struct ContextMenuSection {
    pub id: String,
    pub label: Option<String>,
    pub items: Vec<ContextMenuItem>,
}

impl ContextMenuSection {
    pub fn new(id: impl Into<String>, items: Vec<ContextMenuItem>) -> Self {
        Self { id: id.into(), label: None, items }
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

pub struct ContextMenuProps {
    pub anchor: ContextMenuAnchor,
    pub sections: Vec<ContextMenuSection>,
    /// The menu's name, for a screen reader.
    pub label: String,
    pub on_close: MenuHandler,
    /// Shown while `sections` is empty, such as a menu still waiting on what to list.
    pub placeholder: Option<E>,
}

impl ContextMenuProps {
    pub fn new(anchor: ContextMenuAnchor, label: impl Into<String>, on_close: impl Fn(&mut App) + 'static, sections: Vec<ContextMenuSection>) -> Self {
        Self { anchor, sections, label: label.into(), on_close: Rc::new(on_close), placeholder: None }
    }

    pub fn placeholder(mut self, placeholder: E) -> Self {
        self.placeholder = Some(placeholder);
        self
    }
}

// ---------------------------------------------------------------- state

#[derive(Default)]
struct LevelState {
    open_id: Option<String>,
    /// The submenu was opened from the keyboard, so it takes focus.
    opened_by_key: bool,
    loaded: HashMap<String, Result<Rc<Vec<ContextMenuSection>>, String>>,
    loading: HashSet<String>,
    /// A rest in progress: the row to open, or nothing to close what is open, and since when (set by the first frame to see it).
    pending: Option<(Option<String>, Option<f32>)>,
    focused_first: bool,
}

#[derive(Default)]
struct RootState {
    window: (f32, f32),
    /// What had focus when the menu opened, and has been recorded.
    previous_focus: Option<u64>,
    /// Every row, for telling focus in the menu from focus put somewhere on purpose.
    rows: HashSet<u64>,
}

fn level_state(level: u64) -> Local<LevelState> {
    use_state(("context-menu-level", level), LevelState::default)
}

fn level_id(menu: u64, path: &[&str]) -> u64 {
    path.iter().fold(hash_key(("context-menu", menu)), |id, item| hash_key((id, *item)))
}

/// The key of a row, by the ids of the rows that lead to it: what `rect_of` and a test find it by.
pub fn menu_row_key(menu: impl Hash, path: &[&str], id: &str) -> u64 {
    row_key_in(level_id(hash_key(menu), path), id)
}

fn row_key_in(level: u64, id: &str) -> u64 {
    hash_key(("context-menu-row", level, id))
}

fn panel_key_in(level: u64, is_root: bool) -> u64 {
    if is_root { hash_key(("context-menu-panel", level)) } else { flyout_key(hash_key(level)) }
}

/// The key of a level's panel, by the ids of the rows that lead to it. The menu itself has an empty path.
pub fn menu_panel_key(menu: impl Hash, path: &[&str]) -> u64 {
    panel_key_in(level_id(hash_key(menu), path), path.is_empty())
}

// ---------------------------------------------------------------- the menu

/// A menu, lifted out of the tree wherever it is written. `key` names it: state lasts as long as a view keeps writing it.
pub fn context_menu(key: impl Hash, props: ContextMenuProps) -> E {
    let menu = hash_key(key);
    let root_level = level_id(menu, &[]);
    let panel_key = panel_key_in(root_level, true);
    let root = use_state(("context-menu-root", menu), RootState::default);
    let (window_w, window_h) = window_size();
    let ContextMenuProps { anchor, sections, label, on_close, placeholder } = props;

    {
        let mut r = root.borrow_mut();
        if r.previous_focus.is_none() {
            r.previous_focus = Some(focused_id());
            r.window = (window_w, window_h);
        }
        r.rows.clear();
    }

    // Whatever had focus gets it back when closing leaves focus nowhere: the menu was a detour from it. Focus that is somewhere
    // by then was put there on purpose, by the press that closed the menu or, after this, by the row chosen.
    let close: MenuHandler = {
        let (root, on_close) = (root.clone(), on_close.clone());
        Rc::new(move |app: &mut App| {
            let (previous, in_menu) = {
                let r = root.borrow();
                let now = focused_id();
                (r.previous_focus.unwrap_or(0), now == 0 || r.rows.contains(&now))
            };
            if in_menu {
                focus_request_id(previous);
            }
            on_close(app);
        })
    };

    let placed = rect_of(panel_key).map(|own| place_menu_at(anchor, ANCHOR_GAP, own.w, own.h, window_w, window_h));
    let hang = if placed.is_some_and(|p| p.above) { MenuHang::Up } else { MenuHang::Down };
    let max_w = PANEL_MAX_WIDTH.min(window_w - 16.0);
    let waiting = placeholder.is_some() && sections.is_empty();
    let mut panel = flyout_panel::<App>().key(panel_key).p(3.0).radius(8.0).min_w(180.0).max_w(max_w).shrink0().role(Role::Menu).aria_label(label).aria_busy(waiting);

    // The list is the menu; one still waiting on what to list is a menu saying so.
    if let Some(placeholder) = placeholder.filter(|_| sections.is_empty()) {
        let holder = row().items_center().gap(6.0).px(8.0).py(7.0).child(placeholder);
        let closer = close.clone();
        panel = panel.child(holder).on_key(move |app: &mut App, press: &KeyPress| {
            if press.key == Key::Escape {
                closer(app);
                return true;
            }
            false
        });
    } else {
        let level = Level { level: root_level, sections: &sections, auto_focus: placed.is_some(), hang, close_level: close.clone(), close_all: close.clone(), panel: panel_key, root: root.clone() };
        panel = menu_level(panel, level);
    }

    // The window changed under the measured coordinates, or the page moved behind the menu: it stands somewhere stale. Upstream
    // also closes on the window losing focus, which `Input` does not say.
    let watcher = {
        let (root, closer) = (root.clone(), close.clone());
        custom(move |app: &mut App, ui: &mut Ui<'_>, _: Rect| {
            let moved = ui.input.scroll != 0.0 && !ui.input.pointer.is_some_and(|(x, y)| overlay_rects().iter().any(|o| o.contains(x, y)));
            let resized = root.borrow().window != window_size();
            if moved || resized {
                closer(app);
            }
        })
        .absolute()
        .size(1.0, 1.0)
        .pass_through()
    };
    panel = panel.child(watcher);
    let (left, top) = placed.map_or((0.0, 0.0), |p| (p.left, p.top));
    let closer = close;
    let lifted = panel.popover_at(left, top, move |app: &mut App| closer(app));
    if placed.is_none() {
        // Placed from its measured size, so the first frame draws it nowhere, as upstream's `visibility: hidden` did.
        repaint();
        lifted.opacity(0.0)
    } else {
        lifted
    }
}

const PANEL_MAX_WIDTH: f32 = 420.0;

struct Level<'a> {
    level: u64,
    sections: &'a [ContextMenuSection],
    /// Focus the first row once this holds: the level was opened from the keyboard, or is the menu itself, placed.
    auto_focus: bool,
    hang: MenuHang,
    /// Escape or the left arrow inside this level.
    close_level: MenuHandler,
    /// A row was chosen.
    close_all: MenuHandler,
    panel: u64,
    root: Local<RootState>,
}

/// Opens a row's submenu, or closes the one that is open, reading it first if it is read on opening.
fn open_row(app: &mut App, level: u64, state: &Local<LevelState>, item: Option<&ContextMenuItem>, by_key: bool) {
    state.update(|s| {
        s.pending = None;
        s.opened_by_key = by_key;
        s.open_id = item.map(|i| i.id.clone());
    });
    if let Some(item) = item {
        load(app, level, state, item);
    }
}

fn load(app: &mut App, level: u64, state: &Local<LevelState>, item: &ContextMenuItem) {
    let Some(loader) = &item.load_children else { return };
    let first = state.update(|s| s.loading.insert(item.id.clone()));
    if first {
        loader(app, MenuLoader { level, item: item.id.clone() });
    }
}

#[derive(Clone)]
struct RowEntry {
    id: String,
    key: u64,
    item: ContextMenuItem,
}

/// The rows of one level go onto `panel`, keys and all.
fn menu_level(panel: E, props: Level<'_>) -> E {
    let t_ = theme();
    let Level { level, sections, auto_focus, hang, close_level, close_all, panel: panel_key, root } = props;
    let state = level_state(level);
    let (open_id, opened_by_key, pending) = state.with(|s| (s.open_id.clone(), s.opened_by_key, s.pending.is_some()));

    // The rows that can take focus, in order.
    let mut rows: Vec<RowEntry> = Vec::new();
    for section in sections {
        for item in &section.items {
            let key = row_key_in(level, &item.id);
            root.borrow_mut().rows.insert(hash_key(key));
            if !item.disabled {
                rows.push(RowEntry { id: item.id.clone(), key, item: item.clone() });
            }
        }
    }
    if auto_focus && !state.with(|s| s.focused_first) {
        state.borrow_mut().focused_first = true;
        if let Some(first) = rows.first() {
            focus_request(first.key);
        }
    }
    let rows = Rc::new(rows);

    let mut list = col().min_h(0.0);
    let mut flyouts = Vec::new();
    for (shown_sections, section) in sections.iter().filter(|s| !s.items.is_empty()).enumerate() {
        if shown_sections > 0 {
            // `.popover-menu__panel--dense .popover-menu__divider`.
            list = list.child(div().h(1.0).my(3.0).mx(2.0).bg(t_.divider).shrink0());
        }
        if let Some(text_) = &section.label {
            list = list.child(text(text_.clone()).ty(TypeStyle { size: 10.0, line: 13.0, weight: Weight::Semibold, mono: false, italic: false, serif: false }).color(t_.text_tertiary).ellipsis().px(7.0).pt(4.0).pb(2.0));
        }
        for item in &section.items {
            let branch = item.is_branch();
            let expanded = branch && open_id.as_deref() == Some(item.id.as_str());
            let key = row_key_in(level, &item.id);
            list = list.child(row_element(level, item, key, expanded, &state, &close_all));
            if expanded {
                flyouts.push(flyout_for(level, item, key, panel_key, hang, opened_by_key, &state, &close_all, &root));
            }
        }
    }
    let mut panel = panel.child(list).children(flyouts);

    // A rest on a row only counts after a moment.
    if pending {
        let state = state.clone();
        let items: Vec<ContextMenuItem> = sections.iter().flat_map(|s| s.items.iter().cloned()).collect();
        let watch = custom(move |app: &mut App, ui: &mut Ui<'_>, _: Rect| {
            let ready = match &mut state.borrow_mut().pending {
                Some((target, since)) => {
                    let waited = ui.time - *since.get_or_insert(ui.time);
                    if waited >= HOVER_DELAY_SECONDS {
                        Some(target.clone())
                    } else {
                        ui.wake_after(HOVER_DELAY_SECONDS - waited);
                        None
                    }
                }
                None => None,
            };
            if let Some(target) = ready {
                let item = target.as_deref().and_then(|id| items.iter().find(|i| i.id == id));
                open_row(app, level, &state, item, false);
            }
        })
        .absolute()
        .size(1.0, 1.0)
        .pass_through();
        panel = panel.child(watch);
    }

    // A nested level handles its own keys and stops them; what reaches here is this level's.
    let keys_state = state.clone();
    panel.on_key(move |app: &mut App, press: &KeyPress| {
        let now = focused_id();
        let index = rows.iter().position(|r| hash_key(r.key) == now);
        let count = rows.len() as i32;
        let at = index.map_or(-1, |i| i as i32);
        let focus = |i: i32| {
            if count > 0 {
                focus_request(rows[i.rem_euclid(count) as usize].key);
            }
        };
        match press.key {
            Key::Down => focus(at + 1),
            Key::Up => focus(at - 1),
            Key::Home => focus(0),
            Key::End => focus(count - 1),
            Key::Right => {
                let Some(entry) = index.map(|i| &rows[i]) else { return false };
                if !entry.item.is_branch() {
                    return false;
                }
                open_row(app, level, &keys_state, Some(&entry.item), true);
            }
            Key::Left | Key::Escape => close_level(app),
            Key::Tab => close_all(app),
            // A button's own keys.
            Key::Enter | Key::Char(' ') => {
                let Some(entry) = index.map(|i| &rows[i]) else { return false };
                activate(app, level, &keys_state, &entry.item, keys_state.with(|s| s.open_id.as_deref() == Some(entry.id.as_str())), true, &close_all);
            }
            _ => return false,
        }
        true
    })
}

fn activate(app: &mut App, level: u64, state: &Local<LevelState>, item: &ContextMenuItem, expanded: bool, by_key: bool, close_all: &MenuHandler) {
    if item.is_branch() {
        open_row(app, level, state, if expanded { None } else { Some(item) }, by_key);
        return;
    }
    close_all(app);
    if let Some(f) = &item.on_select {
        f(app);
    }
}

fn row_element(level: u64, item: &ContextMenuItem, key: u64, expanded: bool, state: &Local<LevelState>, close_all: &MenuHandler) -> E {
    let t_ = theme();
    let branch = item.is_branch();
    let focused = focused_id() == hash_key(key);
    let fg = if item.disabled {
        t_.text_disabled
    } else if item.danger {
        t_.critical
    } else {
        t_.text_primary
    };
    let body = TypeStyle { size: 12.0, line: 16.0, weight: Weight::Regular, mono: false, italic: false, serif: false };
    let small = TypeStyle { size: 10.0, line: 13.5, weight: Weight::Regular, mono: false, italic: false, serif: false };
    let mut element = row().key(key).w_full().min_h(24.0).shrink0().items_center().gap(7.0).px(7.0).py(2.0).radius(5.0).role(Role::MenuItem).aria_disabled(item.disabled);
    if branch {
        element = element.aria_haspopup(HasPopup::Menu).aria_expanded(expanded);
    }
    if !item.disabled {
        element = element.hover_bg(t_.subtle_hover).focusable();
    }
    if expanded || focused {
        // The row a submenu came from stays lit while it is open, as a menu's trigger does.
        element = element.bg(t_.subtle_hover);
    }
    if let Some(title) = &item.title {
        element = if item.disabled { element.disabled_reason(title.clone()) } else { element.full_text(title.clone()) };
    }
    if let Some(make) = &item.icon {
        let icon_color = if item.disabled {
            t_.text_disabled
        } else if item.danger {
            t_.critical
        } else {
            t_.text_secondary
        };
        element = element.child(div().shrink0().child(make().color(icon_color)));
    }
    let path_key = |part: &str| (key, part.to_owned());
    let mut copy = col().flex1().min_w(0.0).gap(1.0);
    copy = copy.child(if item.label_is_path { path_text(path_key("label"), item.label.clone(), body, fg) } else { text(item.label.clone()).ty(body).color(fg).ellipsis() });
    if let Some(description) = &item.description {
        let faint = if item.disabled { t_.text_disabled } else { t_.text_tertiary };
        copy = copy.child(if item.description_is_path { path_text(path_key("description"), description.clone(), small, faint) } else { text(description.clone()).ty(small).color(faint).ellipsis() });
    }
    element = element.child(copy);
    if let Some(hint) = &item.hint {
        element = element.child(text(hint.clone()).ty(small).color(t_.text_tertiary).nowrap().shrink0());
    }
    if branch {
        element = element.child(icon("ChevronRight", 13.0).color(t_.text_tertiary).shrink0());
    }

    // Hover is the row's, and a pointer travelling into its own submenu leaves this row for that panel without reaching another
    // row of this level, so what is open stays open.
    let hover_item = item.clone();
    let hover_state = state.clone();
    element = element.on_hover_change(move |app: &mut App, entering: bool| {
        if !entering {
            return;
        }
        let branch = hover_item.is_branch() && !hover_item.disabled;
        // A submenu read on opening starts reading as the pointer reaches its row, so the wait before it opens covers some of
        // the reading.
        if branch {
            load(app, level, &hover_state, &hover_item);
        }
        let target = branch.then(|| hover_item.id.clone());
        hover_state.update(|s| {
            if target == s.open_id {
                s.pending = None;
            } else {
                s.pending = Some((target, None));
            }
        });
    });
    if !item.disabled {
        let (click_item, click_state, closer) = (item.clone(), state.clone(), close_all.clone());
        element = element.on_click(move |app: &mut App| {
            let expanded = click_state.with(|s| s.open_id.as_deref() == Some(click_item.id.as_str()));
            activate(app, level, &click_state, &click_item, expanded, false, &closer);
        });
    }
    element
}

#[allow(clippy::too_many_arguments)]
fn flyout_for(level: u64, item: &ContextMenuItem, row_id: u64, parent_panel: u64, hang: MenuHang, opened_by_key: bool, state: &Local<LevelState>, close_all: &MenuHandler, root: &Local<RootState>) -> E {
    let t_ = theme();
    let child_level = hash_key((level, item.id.as_str()));
    let own_panel = panel_key_in(child_level, false);
    let loaded: Option<Result<Rc<Vec<ContextMenuSection>>, String>> = state.with(|s| s.loaded.get(&item.id).cloned());
    let children = item.children.clone().or_else(|| loaded.as_ref().and_then(|l| l.as_ref().ok().cloned()));
    let failure = loaded.and_then(|l| l.err());
    let empty_ty = TypeStyle { size: 11.0, line: 15.0, weight: Weight::Regular, mono: false, italic: false, serif: false };
    let empty = |message: String| text(message).ty(empty_ty).color(t_.text_tertiary).px(8.0).py(9.0);
    let mut panel = flyout_panel::<App>().p(3.0).radius(8.0).min_w(150.0).max_w(PANEL_MAX_WIDTH).role(Role::Menu).aria_label(item.label.clone());
    panel = match children {
        Some(sections) if sections.iter().any(|s| !s.items.is_empty()) => {
            // Back to the row the level came from, as Escape and the left arrow go.
            let (back_state, back_row) = (state.clone(), row_id);
            let close_level: MenuHandler = Rc::new(move |app: &mut App| {
                open_row(app, level, &back_state, None, false);
                focus_request(back_row);
            });
            let placed = rect_of(own_panel).is_some();
            let props = Level { level: child_level, sections: &sections, auto_focus: opened_by_key && placed, hang, close_level, close_all: close_all.clone(), panel: own_panel, root: root.clone() };
            menu_level(panel, props)
        }
        Some(_) => panel.child(empty(t("没有可用的程序", "Nothing to offer").to_string())),
        None => match failure {
            Some(message) => panel.child(empty(message)),
            None => panel.child(row().items_center().gap(6.0).px(8.0).py(9.0).child(spinner(12.0, t_.text_tertiary)).child(text(t("正在读取…", "Loading…")).ty(empty_ty).color(t_.text_tertiary))),
        },
    };
    let dismiss_state = state.clone();
    menu_flyout(child_level, row_id, parent_panel, panel, hang, move |app: &mut App| {
        // A press outside the whole menu closes it too, and a press in the menu is only about this submenu.
        open_row(app, level, &dismiss_state, None, false);
    })
}

/// `LoaderCircle` with its `.spin`.
fn spinner(size: f32, color: Color) -> E {
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        ui.named_icon("LoaderCircle", r.x + r.w * 0.5, r.y + r.h * 0.5, size, color, (ui.time * 360.0) % 360.0);
        ui.animating = true;
    })
    .size(size, size)
    .shrink0()
    .pass_through()
}
