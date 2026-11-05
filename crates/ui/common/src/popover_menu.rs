//! The shared application menu: upstream's `PopoverMenu.tsx`, `usePopoverAnchor.ts`, `MenuFlyout.tsx` and the placement half
//! of `lib/menuPlacement.ts`.
//!
//! A menu is a builder that turns into its trigger, with the panel lifted out of the tree wherever it is written:
//!
//! ```ignore
//! PopoverMenu::new(("model-menu", id), t("模型", "Model"), |open| chip(open))
//!     .menu_label(t("模型", "Model")).width(300.0).align_end().dense().search(t("搜索模型…", "Search models…"))
//!     .sections(|| vec![PopoverMenuSection::new("models").item(PopoverMenuItem::new("a", "Model A").on(|app| pick(app)))])
//!     .build()
//! ```
//!
//! `use_popover_anchor` is the placement and dismissal on their own, for a panel that is not a menu (`ContextUsageMeter`),
//! and `menu_flyout` a panel beside a row of another.
//!
//! Tests look elements up by `trigger_key` and `row_key`.

use crate::path_text::path_text;
use mewrk_client::prelude::*;
use std::cell::{Cell, RefCell};
use std::hash::Hash;

/// How close a panel may come to the window's edge.
const VIEWPORT_MARGIN: f32 = 8.0;
/// How far a panel stands off its trigger.
const ANCHOR_GAP: f32 = 6.0;
/// `.popover-menu__list`'s `min(420px, 100vh - 32px)`.
const LIST_MAX_HEIGHT: f32 = 420.0;
/// `.popover-menu__panel`'s `min(420px, 100vw - 16px)`.
const PANEL_MAX_WIDTH: f32 = 420.0;

/// Text views draw, recorded for tests that read a surface by what it says. Off unless a test turns it on.
pub mod probe {
    use super::*;

    thread_local! {
        static ON: Cell<bool> = const { Cell::new(false) };
        static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    /// Starts recording on this thread, and forgets what was recorded before.
    pub fn start() {
        ON.with(|on| on.set(true));
        LOG.with(|log| log.borrow_mut().clear());
    }

    /// What was said since the last `take`.
    pub fn take() -> Vec<String> {
        LOG.with(|log| std::mem::take(&mut *log.borrow_mut()))
    }

    pub fn say(text: &str) {
        if ON.with(Cell::get) {
            LOG.with(|log| log.borrow_mut().push(text.to_owned()));
        }
    }
}

use probe::say;

// ---------------------------------------------------------------- placement

/// The side of the trigger a panel prefers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    /// Opens above only when the panel does not fit below and there is more room above.
    #[default]
    Below,
    /// The mirror image, for a trigger whose content continues underneath it: above whenever it fits there, below when it
    /// does not but fits below, and the roomier side when it fits neither.
    Above,
}

/// Where a panel goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopoverPosition {
    pub left: f32,
    pub top: f32,
    /// The panel opens above the trigger, so what sits next to the trigger moves to the opposite edge.
    pub flipped: bool,
    pub min_width: f32,
}

/// `usePopoverAnchor`'s placement pass: a panel against its trigger. `align` is `Start` or `End`.
pub fn place_popover(anchor: Rect, panel: (f32, f32), viewport: (f32, f32), align: Align, placement: Placement) -> PopoverPosition {
    let (panel_w, panel_h) = panel;
    let (viewport_w, viewport_h) = viewport;
    let preferred_left = if align == Align::End { anchor.right() - panel_w } else { anchor.x };
    let max_left = VIEWPORT_MARGIN.max(viewport_w - panel_w - VIEWPORT_MARGIN);
    let left = preferred_left.max(VIEWPORT_MARGIN).min(max_left);

    let below = anchor.bottom() + ANCHOR_GAP;
    let room_below = viewport_h - below - VIEWPORT_MARGIN;
    let room_above = anchor.y - ANCHOR_GAP - VIEWPORT_MARGIN;
    let fits_below = panel_h <= room_below;
    let flipped = match placement {
        Placement::Above => panel_h <= room_above || (!fits_below && room_above > room_below),
        Placement::Below => !fits_below && room_above > room_below,
    };
    let top = if flipped { VIEWPORT_MARGIN.max(anchor.y - ANCHOR_GAP - panel_h) } else { below.min(VIEWPORT_MARGIN.max(viewport_h - panel_h - VIEWPORT_MARGIN)) };
    PopoverPosition { left, top, flipped, min_width: anchor.w }
}

/// `placeMenu` for a point: a menu opened at a spot. In the window's upper half it hangs down from the spot; in the lower half
/// it stands on it. Across, it opens to the right, or to the left when only the left has room.
pub fn place_menu_at_point(point: (f32, f32), panel: (f32, f32), viewport: (f32, f32)) -> (f32, f32, bool) {
    let (x, y) = point;
    let (width, height) = panel;
    let (viewport_w, viewport_h) = viewport;
    let above = y > viewport_h / 2.0;
    let preferred_top = if above { y - height } else { y };
    let top = preferred_top.max(VIEWPORT_MARGIN).min(VIEWPORT_MARGIN.max(viewport_h - height - VIEWPORT_MARGIN));
    let room_right = x + width <= viewport_w - VIEWPORT_MARGIN;
    let preferred_left = if room_right || x - width < VIEWPORT_MARGIN { x } else { x - width };
    let left = preferred_left.max(VIEWPORT_MARGIN).min(VIEWPORT_MARGIN.max(viewport_w - width - VIEWPORT_MARGIN));
    (left, top, above)
}

/// Whether a submenu hangs down from its row's top or, for a row at the foot of its panel, up from the row's foot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hang {
    #[default]
    Down,
    Up,
}

/// `placeFlyout`: beside its panel, its edge over the panel's own, and lined up with its row. To the right, or to the left when
/// the right has no room; up or down by however much would leave the window. `inset` is the submenu's own border and padding,
/// top and bottom, so its first or last row lines up with its row.
pub fn place_flyout(row: Rect, parent: Rect, size: (f32, f32), inset: (f32, f32), hang: Hang, viewport: (f32, f32)) -> (f32, f32) {
    // One logical pixel of border, which is what every panel here is drawn with.
    let right = parent.right() - 1.0;
    let (width, height) = size;
    let left = if right + width > viewport.0 - VIEWPORT_MARGIN { VIEWPORT_MARGIN.max(parent.x + 1.0 - width) } else { right };
    let preferred_top = match hang {
        Hang::Down => row.y - inset.0,
        Hang::Up => row.bottom() + inset.1 - height,
    };
    (left, VIEWPORT_MARGIN.max(preferred_top.min(viewport.1 - VIEWPORT_MARGIN - height)))
}

// ---------------------------------------------------------------- the anchor

#[derive(Default)]
struct AnchorState {
    open: bool,
    /// Where the press that opened it was, for a menu opened at the pointer.
    pointer: Option<(f32, f32)>,
    position: Option<PopoverPosition>,
    window: (f32, f32),
    /// What the frame's input said, for handlers that run after it and hear nothing of it.
    frame_pointer: Option<(f32, f32)>,
    frame_escape: bool,
}

/// Options for [`use_popover_anchor`].
#[derive(Clone, Copy, Debug, Default)]
pub struct AnchorOptions {
    pub align_end: bool,
    pub placement: Placement,
    /// Opens with the panel at the press instead of against the trigger. A keyboard opening has none, and uses the trigger.
    pub anchor_to_pointer: bool,
}

/// One panel's open state and placement. `use_popover_anchor` again next frame finds the same one.
#[derive(Clone)]
pub struct PopoverAnchor {
    state: Local<AnchorState>,
    trigger_key: u64,
    panel_key: u64,
    options: AnchorOptions,
}

/// `.key(...)` of a menu's trigger, for whatever needs to find it.
pub fn trigger_key(key: impl Hash) -> u64 {
    trigger_id(hash_key(key))
}

fn trigger_id(id: u64) -> u64 {
    hash_key(("popover-trigger", id))
}

/// `.key(...)` of a menu's panel.
pub fn panel_key(key: impl Hash) -> u64 {
    panel_id(hash_key(key))
}

fn panel_id(id: u64) -> u64 {
    hash_key(("popover-panel", id))
}

/// `.key(...)` of a row of a menu.
pub fn row_key(key: impl Hash, id: &str) -> u64 {
    row_id(hash_key(key), id)
}

fn row_id(menu: u64, id: &str) -> u64 {
    hash_key(("popover-row", menu, id))
}

/// `.key(...)` of a trailing action of a row or a heading of a menu.
pub fn action_key(key: impl Hash, id: &str) -> u64 {
    action_id(hash_key(key), id)
}

fn action_id(menu: u64, id: &str) -> u64 {
    hash_key(("popover-action", menu, id))
}

/// Whether the menu with this key is open, for a caller that only works out its rows when someone can see them.
pub fn menu_is_open(key: impl Hash) -> bool {
    use_state(("popover-anchor", hash_key(key)), AnchorState::default).borrow().open
}

/// `usePopoverAnchor`. Call it every frame the trigger is on screen. The position is worked out from where the trigger and the
/// panel were last frame, so it follows both, and is `None` until the panel has been measured.
pub fn use_popover_anchor(key: impl Hash, options: AnchorOptions) -> PopoverAnchor {
    anchor_of(hash_key(key), options)
}

fn anchor_of(id: u64, options: AnchorOptions) -> PopoverAnchor {
    let anchor = PopoverAnchor { state: use_state(("popover-anchor", id), AnchorState::default), trigger_key: trigger_id(id), panel_key: panel_id(id), options };
    anchor.update();
    anchor
}

impl PopoverAnchor {
    fn update(&self) {
        let mut state = self.state.borrow_mut();
        let window = window_size();
        if !state.open {
            state.position = None;
            state.window = window;
            return;
        }
        // The window changed under the measured coordinates. Transient panels close rather than stand somewhere stale.
        if state.window != window {
            state.open = false;
            state.position = None;
            state.window = window;
            repaint();
            return;
        }
        let (Some(trigger), Some(panel)) = (rect_of(self.trigger_key), rect_of(self.panel_key)) else {
            state.position = None;
            return;
        };
        let pointer = if self.options.anchor_to_pointer { state.pointer } else { None };
        let next = match pointer {
            Some(point) => {
                let (left, top, above) = place_menu_at_point(point, (panel.w, panel.h), window);
                PopoverPosition { left, top, flipped: above, min_width: trigger.w }
            }
            None => place_popover(trigger, (panel.w, panel.h), window, if self.options.align_end { Align::End } else { Align::Start }, self.options.placement),
        };
        if state.position != Some(next) {
            state.position = Some(next);
            repaint();
        }
    }

    pub fn is_open(&self) -> bool {
        self.state.borrow().open
    }

    /// `null` before measurement.
    pub fn position(&self) -> Option<PopoverPosition> {
        self.state.borrow().position
    }

    pub fn toggle(&self) {
        let mut state = self.state.borrow_mut();
        state.open = !state.open;
        state.position = None;
        state.window = window_size();
        repaint();
    }

    pub fn close(&self) {
        let mut state = self.state.borrow_mut();
        state.open = false;
        state.position = None;
        repaint();
    }

    /// Remembers the press a menu opened at the pointer opens at.
    pub fn press_at(&self, x: f32, y: f32) {
        self.state.borrow_mut().pointer = Some((x, y));
    }

    pub fn trigger_key(&self) -> u64 {
        self.trigger_key
    }

    pub fn panel_key(&self) -> u64 {
        self.panel_key
    }

    /// Whether the frame's pointer is over this panel: a submenu's dismissal asks, since a press in the menu is not outside it.
    fn pointer_on_panel(&self) -> bool {
        let state = self.state.borrow();
        match (state.frame_pointer, rect_of(self.panel_key)) {
            (Some((x, y)), Some(panel)) => panel.contains(x, y),
            _ => false,
        }
    }

    fn escape_this_frame(&self) -> bool {
        self.state.borrow().frame_escape
    }

    /// Takes the panel as an element and lifts it out of the tree at its place, hidden for the first frame while it is measured.
    /// A wheel turned outside it closes it, which is when a page behind it moves.
    pub fn place(&self, panel: E) -> E {
        let position = self.position();
        let (left, top) = position.map_or((0.0, 0.0), |p| (p.left, p.top));
        let (state, closer) = (self.state.clone(), self.clone());
        let watch = custom(move |_: &mut App, ui: &mut Ui<'_>, _: Rect| {
            {
                let mut state = state.borrow_mut();
                state.frame_pointer = ui.input.pointer;
                state.frame_escape = ui.input.keys.iter().any(|press| press.key == Key::Escape);
            }
            if ui.input.scroll != 0.0 {
                let over_a_panel = ui.input.pointer.is_some_and(|(x, y)| overlay_rects().iter().any(|r| r.contains(x, y)));
                if !over_a_panel {
                    closer.close();
                }
            }
        })
        .pass_through();
        let closer = self.clone();
        let placed = panel.child(watch).key(self.panel_key).popover_at_anchored(hash_key(self.trigger_key), left, top, move |_: &mut App| closer.close());
        if position.is_none() {
            repaint();
            placed.opacity(0.0)
        } else {
            placed
        }
    }
}

/// `MenuFlyout`: a submenu's panel beside the row that opened it, a panel of its own on the page rather than a box inside the one
/// it came from. `parent` is the anchor of the panel the row lies in; a press in that panel leaves the submenu alone, and a press
/// outside both, or Escape, closes the whole menu. `panel` is what the submenu draws; `inset` its own top and bottom padding.
pub fn menu_flyout(key: impl Hash, parent: &PopoverAnchor, row: u64, panel: E, inset: (f32, f32), hang: Hang) -> E {
    let id = hash_key(("menu-flyout", hash_key(key)));
    let viewport = window_size();
    let placed = match (rect_of(id), rect_of(row), rect_of(parent.panel_key)) {
        (Some(own), Some(row), Some(parent)) => Some(place_flyout(row, parent, (own.w, own.h), inset, hang, viewport)),
        _ => None,
    };
    let (left, top) = placed.unwrap_or((0.0, 0.0));
    let parent = parent.clone();
    let shown = panel.key(id).popover_at_anchored(hash_key(row), left, top, move |_: &mut App| {
        if parent.escape_this_frame() || !parent.pointer_on_panel() {
            parent.close();
        }
    });
    if placed.is_none() {
        repaint();
        shown.opacity(0.0)
    } else {
        shown
    }
}

/// The surface a menu stands on: the flyout's colour with nothing showing through it, since what lies under a menu is usually the
/// text of the page it opened over.
pub fn solid_panel() -> E {
    let t_ = theme();
    flyout_panel::<App>().bg(t_.flyout.with_alpha(1.0))
}

// ---------------------------------------------------------------- the menu

/// `.composer-option`: a word that opens a menu, told whether its menu is open. Nothing but the word at rest, lit under the pointer.
pub fn option_box(open: bool, disabled: bool) -> E {
    let t_ = theme();
    let option: E = row().h(25.0).min_w(0.0).items_center().gap(5.0).px(8.0).radius(7.0).when(open, |e| e.bg(t_.subtle_hover));
    if disabled { option } else { option.hover_bg(t_.subtle_hover) }
}

pub fn option_label(label: String, disabled: bool) -> E {
    let t_ = theme();
    text(label).ty(FINE).color(if disabled { t_.text_disabled } else { t_.text_secondary }).ellipsis().min_w(0.0)
}

/// A small icon button trailing a row or a section heading: a gear that opens the settings of what the row names, say. It is a
/// sibling of the row's own button, never inside it, and choosing it closes the menu like choosing the row.
pub struct PopoverMenuAction {
    /// Its tooltip; the button shows only its icon.
    pub label: String,
    pub icon: IconRef,
    pub on_select: Handler<App>,
}

impl PopoverMenuAction {
    pub fn new(label: impl Into<String>, icon: impl Into<IconRef>, on_select: impl Fn(&mut App) + 'static) -> Self {
        Self { label: label.into(), icon: icon.into(), on_select: Rc::new(on_select) }
    }
}

/// What the checkmark means. Upstream told assistive technology; here the two draw alike.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CheckedRole {
    /// One of a set where exactly one is picked.
    #[default]
    Radio,
    /// A setting that stands alone.
    Checkbox,
}

/// A row in the menu.
pub struct PopoverMenuItem {
    pub id: String,
    pub label: String,
    /// The label is a path, drawn so that it gives way in the middle.
    pub label_is_path: bool,
    /// Secondary text explaining this item or why it is disabled.
    pub description: Option<String>,
    pub description_is_path: bool,
    pub icon: Option<IconRef>,
    /// What goes where the icon does when it is a drawing and not a named icon.
    pub leading: Option<E>,
    /// Subtle trailing text, such as a shortcut or why a setting is not taking effect.
    pub hint: Option<String>,
    /// A small trailing mark about the row's state, such as a warm cache.
    pub badge: Option<E>,
    /// `Some(true)` shows a checkmark. `None` is an action.
    pub checked: Option<bool>,
    pub checked_role: CheckedRole,
    pub disabled: bool,
    /// On a disabled row, why: drawn on hover. Otherwise what a cut line in the row is short for.
    pub title: Option<String>,
    /// Expands into a nested list. Choosing an item with children only expands it.
    pub children: Vec<PopoverMenuItem>,
    pub on_select: Option<Handler<App>>,
    /// A trailing button beside the row, for acting on what the row names rather than choosing it.
    pub action: Option<PopoverMenuAction>,
}

impl PopoverMenuItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), label_is_path: false, description: None, description_is_path: false, icon: None, leading: None, hint: None, badge: None, checked: None, checked_role: CheckedRole::Radio, disabled: false, title: None, children: Vec::new(), on_select: None, action: None }
    }

    pub fn on(mut self, f: impl Fn(&mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(f));
        self
    }

    pub fn checked(mut self, on: bool) -> Self {
        self.checked = Some(on);
        self
    }

    pub fn checkbox(mut self, on: bool) -> Self {
        self.checked = Some(on);
        self.checked_role = CheckedRole::Checkbox;
        self
    }

    pub fn disabled(mut self, on: bool) -> Self {
        self.disabled = on;
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn maybe_title(mut self, title: Option<String>) -> Self {
        self.title = title;
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn maybe_hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint;
        self
    }

    pub fn icon(mut self, icon: impl Into<IconRef>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub fn path_label(mut self) -> Self {
        self.label_is_path = true;
        self
    }

    pub fn path_description(mut self) -> Self {
        self.description_is_path = true;
        self
    }

    pub fn leading(mut self, leading: E) -> Self {
        self.leading = Some(leading);
        self
    }

    pub fn badge(mut self, badge: E) -> Self {
        self.badge = Some(badge);
        self
    }

    pub fn children(mut self, children: Vec<PopoverMenuItem>) -> Self {
        self.children = children;
        self
    }

    pub fn action(mut self, action: PopoverMenuAction) -> Self {
        self.action = Some(action);
        self
    }
}

/// A group of rows with an optional heading. Adjacent groups are separated.
pub struct PopoverMenuSection {
    pub id: String,
    pub label: Option<String>,
    /// A trailing button on the heading, for acting on what the whole group shares. Needs a `label`.
    pub action: Option<PopoverMenuAction>,
    pub items: Vec<PopoverMenuItem>,
}

impl PopoverMenuSection {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), label: None, action: None, items: Vec::new() }
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn action(mut self, action: PopoverMenuAction) -> Self {
        self.action = Some(action);
        self
    }

    pub fn item(mut self, item: PopoverMenuItem) -> Self {
        self.items.push(item);
        self
    }

    pub fn items(mut self, items: impl IntoIterator<Item = PopoverMenuItem>) -> Self {
        self.items.extend(items);
        self
    }
}

/// How a row with children opens them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Submenu {
    /// Nests them under the row, inside the panel's own scroll.
    #[default]
    Inline,
    /// Opens them beside the row: a choice whose second step refines the first keeps the chosen branch on screen beside it.
    Flyout,
}

fn matches_query(item: &PopoverMenuItem, needle: &str) -> bool {
    needle.is_empty() || item.label.to_lowercase().contains(needle) || item.description.as_deref().is_some_and(|text| text.to_lowercase().contains(needle)) || item.children.iter().any(|child| matches_query(child, needle))
}

/// The sections a query leaves: only top-level rows are filtered, and one that matches keeps all its children.
pub fn visible_sections(sections: Vec<PopoverMenuSection>, query: &str) -> Vec<PopoverMenuSection> {
    let needle = query.to_lowercase();
    sections
        .into_iter()
        .filter_map(|mut section| {
            section.items.retain(|item| matches_query(item, &needle));
            (!section.items.is_empty()).then_some(section)
        })
        .collect()
}

#[derive(Default)]
struct MenuState {
    query: String,
    expanded: Option<String>,
    /// The row the arrow keys are on, and the open submenu's.
    cursor: Option<String>,
    flyout_cursor: Option<String>,
    was_open: bool,
    published_rect: bool,
}

type SectionsFn = Box<dyn FnOnce() -> Vec<PopoverMenuSection>>;

/// The builder; see the module's head. `new`'s closure draws the trigger, told whether the menu is open.
pub struct PopoverMenu {
    id: u64,
    trigger: Box<dyn FnOnce(bool) -> E>,
    trigger_label: String,
    trigger_title: Option<String>,
    disabled: bool,
    disabled_reason: Option<String>,
    sections: Option<SectionsFn>,
    menu_label: String,
    width: Option<f32>,
    align_end: bool,
    placement: Placement,
    dense: bool,
    anchor_to_pointer: bool,
    search: Option<String>,
    empty_label: Option<String>,
    submenu: Submenu,
    on_open: Option<Handler<App>>,
    on_panel_rect: Option<Rc<dyn Fn(&mut App, Option<Rect>)>>,
}

impl PopoverMenu {
    pub fn new(key: impl Hash, trigger_label: impl Into<String>, trigger: impl FnOnce(bool) -> E + 'static) -> Self {
        Self { id: hash_key(key), trigger: Box::new(trigger), trigger_label: trigger_label.into(), trigger_title: None, disabled: false, disabled_reason: None, sections: None, menu_label: String::new(), width: None, align_end: false, placement: Placement::Below, dense: false, anchor_to_pointer: false, search: None, empty_label: None, submenu: Submenu::Inline, on_open: None, on_panel_rect: None }
    }

    /// Built only while the menu is open, so a menu with hundreds of rows costs nothing while it is shut.
    pub fn sections(mut self, sections: impl FnOnce() -> Vec<PopoverMenuSection> + 'static) -> Self {
        self.sections = Some(Box::new(sections));
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.trigger_title = Some(title.into());
        self
    }

    /// Why the trigger is disabled, where there is a reason to give.
    pub fn disabled_reason(mut self, why: Option<String>) -> Self {
        self.disabled_reason = why;
        self
    }

    pub fn maybe_title(mut self, title: Option<String>) -> Self {
        self.trigger_title = title;
        self
    }

    pub fn disabled(mut self, on: bool) -> Self {
        self.disabled = on;
        self
    }

    pub fn menu_label(mut self, label: impl Into<String>) -> Self {
        self.menu_label = label.into();
        self
    }

    /// Without one, the content's width, never less than the trigger's.
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    pub fn align_end(mut self) -> Self {
        self.align_end = true;
        self
    }

    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    pub fn above(self) -> Self {
        self.placement(Placement::Above)
    }

    /// Single-line rows with tighter metrics, for menus that carry no descriptions.
    pub fn dense(mut self) -> Self {
        self.dense = true;
        self
    }

    pub fn anchor_to_pointer(mut self) -> Self {
        self.anchor_to_pointer = true;
        self
    }

    /// A search field, with this placeholder.
    pub fn search(mut self, placeholder: impl Into<String>) -> Self {
        self.search = Some(placeholder.into());
        self
    }

    /// What shows when search removes every row.
    pub fn empty_label(mut self, label: impl Into<String>) -> Self {
        self.empty_label = Some(label.into());
        self
    }

    pub fn submenu(mut self, submenu: Submenu) -> Self {
        self.submenu = submenu;
        self
    }

    /// Runs once whenever the menu opens, for lazy list loading.
    pub fn on_open(mut self, f: impl Fn(&mut App) + 'static) -> Self {
        self.on_open = Some(Rc::new(f));
        self
    }

    /// Hears the open panel's rectangle, and `None` once it closes: a pane whose body is a native page stacked beneath
    /// everything needs to tell when the panel covers it.
    pub fn on_panel_rect(mut self, f: impl Fn(&mut App, Option<Rect>) + 'static) -> Self {
        self.on_panel_rect = Some(Rc::new(f));
        self
    }

    pub fn build(mut self) -> E {
        let id = self.id;
        let anchor = anchor_of(id, AnchorOptions { align_end: self.align_end, placement: self.placement, anchor_to_pointer: self.anchor_to_pointer });
        let state = use_state(("popover-menu", id), MenuState::default);
        let open = anchor.is_open() && !self.disabled;
        // Search and nested expansion start clean on reopening.
        {
            let mut menu = state.borrow_mut();
            if !open && menu.was_open {
                menu.query.clear();
                menu.expanded = None;
                menu.cursor = None;
                menu.flyout_cursor = None;
            }
            menu.was_open = open;
        }
        let title = self.trigger_title.clone().unwrap_or_else(|| self.trigger_label.clone());
        say(&self.trigger_label);
        let draw_trigger = std::mem::replace(&mut self.trigger, Box::new(|_| div()));
        let draw_trigger_title = self.trigger_title.clone();
        let mut trigger: E = draw_trigger(open).key(anchor.trigger_key()).tooltip(title).map(|e| match draw_trigger_title {
            Some(title) => e.full_text(title),
            None => e,
        }).role(Role::Button).aria_label(self.trigger_label.clone()).aria_haspopup(HasPopup::Menu).aria_expanded(open);
        if self.disabled {
            trigger = trigger.disabled(true).on_click(|_: &mut App| {}).maybe_disabled_reason(self.disabled_reason.clone());
        } else {
            let (toggler, on_open) = (anchor.clone(), self.on_open.clone());
            trigger = trigger.on_click(move |app: &mut App| {
                toggler.toggle();
                if toggler.is_open()
                    && let Some(f) = &on_open
                {
                    f(app);
                }
            });
            if self.anchor_to_pointer {
                let pressed = anchor.clone();
                trigger = trigger.on_pointer(move |_: &mut App, event: PointerEvent| {
                    if event.phase == Phase::Down {
                        pressed.press_at(event.x, event.y);
                    }
                });
            }
        }
        let on_panel_rect = self.on_panel_rect.clone();
        let root = div::<App>().min_w(0.0).row();
        let root = if open { root.child(trigger).child(menu_panel_view(self, &anchor, &state)) } else { root.child(trigger) };
        // The rectangle goes out only once it is placed: a page must never be treated as covered by one it has not taken yet.
        match on_panel_rect {
            Some(f) => {
                let published = state.clone();
                let (position, panel) = (anchor.position(), anchor.panel_key());
                root.child(
                    custom(move |app: &mut App, _: &mut Ui<'_>, _: Rect| {
                        let rect = if position.is_some() { rect_of(panel) } else { None };
                        let mut menu = published.borrow_mut();
                        if let Some(rect) = rect {
                            menu.published_rect = true;
                            drop(menu);
                            f(app, Some(rect));
                        } else if menu.published_rect {
                            menu.published_rect = false;
                            drop(menu);
                            f(app, None);
                        }
                    })
                    .w(0.0)
                    .h(0.0),
                )
            }
            None => root,
        }
    }
}

/// Where the arrow keys can go, in order, and what Enter does there.
#[derive(Clone)]
struct KeyRow {
    id: String,
    expandable: bool,
    on_select: Option<Handler<App>>,
}

fn key_rows(items: &[PopoverMenuItem], expanded: Option<&str>, inline: bool, out: &mut Vec<KeyRow>) {
    for item in items {
        if !item.disabled {
            out.push(KeyRow { id: item.id.clone(), expandable: !item.children.is_empty(), on_select: item.on_select.clone() });
        }
        if inline && expanded == Some(item.id.as_str()) {
            key_rows(&item.children, None, false, out);
        }
    }
}

fn step(rows: &[KeyRow], cursor: Option<&str>, down: bool) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let at = cursor.and_then(|id| rows.iter().position(|row| row.id == id));
    let next = match at {
        None => if down { 0 } else { rows.len() - 1 },
        Some(at) if down => (at + 1) % rows.len(),
        Some(at) => (at + rows.len() - 1) % rows.len(),
    };
    Some(rows[next].id.clone())
}

fn menu_panel_view(mut menu: PopoverMenu, anchor: &PopoverAnchor, state: &Local<MenuState>) -> E {
    let t_ = theme();
    let id = menu.id;
    let (query, expanded, cursor, flyout_cursor) = {
        let s = state.borrow();
        (s.query.clone(), s.expanded.clone(), s.cursor.clone(), s.flyout_cursor.clone())
    };
    let sections = visible_sections(menu.sections.take().map_or_else(Vec::new, |f| f()), &query);
    let inline = menu.submenu == Submenu::Inline;
    let (viewport_w, viewport_h) = window_size();
    let mut rows = Vec::new();
    for section in &sections {
        key_rows(&section.items, expanded.as_deref(), inline, &mut rows);
    }
    let mut flyout_rows = Vec::new();
    if !inline
        && let Some(open) = expanded.as_deref()
        && let Some(item) = sections.iter().flat_map(|section| &section.items).find(|item| item.id == open)
    {
        key_rows(&item.children, None, false, &mut flyout_rows);
    }

    let flipped = anchor.position().is_some_and(|p| p.flipped);
    let placed = anchor.position().is_some();
    let search_field = menu.search.clone().map(|placeholder| {
        say(&placeholder);
        let placeholder_name = placeholder.clone();
        let writer = state.clone();
        let field = field(("popover-search", id), query.clone())
            .placeholder(placeholder)
            .bare()
            .ty(CAPTION)
            .on_change(move |_: &mut App, value| {
                writer.update(|s| {
                    s.query = value;
                    s.cursor = None;
                });
            })
            .build()
            .aria_label(placeholder_name)
            .flex1()
            .h(CAPTION.line);
        let bar: E = row().items_center().gap(6.0).min_h(30.0).px(7.0).mx(2.0).shrink0().child(icon("Search", 13.0).color(t_.text_tertiary)).child(field);
        if flipped { bar.mt(4.0).mb(2.0).border_top(t_.divider) } else { bar.mt(2.0).mb(4.0).border_bottom(t_.divider) }
    });

    let cx = RowCx { menu_id: id, dense: menu.dense, inline, anchor: anchor.clone(), state: state.clone(), expanded: expanded.clone(), cursor: cursor.clone(), flyout_cursor };
    let mut list: E = col().key(("popover-list", id)).min_h(0.0).max_h(LIST_MAX_HEIGHT.min(viewport_h - 32.0)).scroll_y();
    let empty = sections.is_empty();
    for (index, section) in sections.into_iter().enumerate() {
        let mut block: E = col().shrink0();
        if index > 0 {
            block = block.child(div().h(1.0).my(if menu.dense { 3.0 } else { 4.0 }).mx(2.0).bg(t_.divider).shrink0());
        }
        if let Some(label) = &section.label {
            say(label);
            let heading: E = text(label.clone()).ty(FINE_STRONG).color(t_.text_tertiary);
            block = block.child(match section.action {
                Some(action) => {
                    let action_id = format!("section:{}", section.id);
                    row().items_center().pl(8.0).pr(3.0).pt(3.0).pb(1.0).child(heading.ellipsis().min_w(0.0).shrink0()).child(action_button(&cx, &action_id, action))
                }
                None => div().px(if menu.dense { 7.0 } else { 8.0 }).pt(if menu.dense { 4.0 } else { 6.0 }).pb(if menu.dense { 2.0 } else { 3.0 }).child(heading),
            });
        }
        for item in section.items {
            block = block.child(render_item(&cx, item, 0));
        }
        list = list.child(block);
    }
    if empty {
        let label = menu.empty_label.clone().unwrap_or_else(|| t("没有匹配项", "No matches").to_owned());
        say(&label);
        list = list.child(div().px(8.0).py(9.0).child(text(label).ty(CAPTION).color(t_.text_tertiary)));
    }

    let mut panel: E = solid_panel().p(if menu.dense { 3.0 } else { 5.0 }).max_w(PANEL_MAX_WIDTH.min(viewport_w - 16.0)).role(Role::Menu).aria_label(menu.menu_label.clone());
    panel = match menu.width {
        Some(width) => panel.w(width),
        None => panel.min_w(anchor.position().map_or(0.0, |p| p.min_width)),
    };
    let top_field = placed && !flipped;
    let (top_search, bottom_search) = if top_field { (search_field, None) } else { (None, search_field) };
    panel = panel.child(top_search).child(list).child(bottom_search);

    // The arrows walk the rows from wherever the focus is: nothing here can tab into a panel lifted out of the tree.
    let keys = {
        let (state, anchor) = (state.clone(), anchor.clone());
        let (rows, flyout_rows) = (Rc::new(rows), Rc::new(flyout_rows));
        move |app: &mut App, press: &KeyPress| -> bool {
            let in_flyout = !inline && state.borrow().expanded.is_some() && state.borrow().flyout_cursor.is_some();
            match press.key {
                Key::Down | Key::Up => {
                    let down = press.key == Key::Down;
                    state.update(|s| {
                        if in_flyout {
                            s.flyout_cursor = step(&flyout_rows, s.flyout_cursor.as_deref(), down);
                        } else {
                            s.cursor = step(&rows, s.cursor.as_deref(), down);
                        }
                    });
                    true
                }
                Key::Right if !inline => {
                    let current = state.borrow().cursor.clone();
                    let Some(row) = current.and_then(|id| rows.iter().find(|row| row.id == id).cloned()) else { return false };
                    if !row.expandable {
                        return false;
                    }
                    // Its rows are known only once it is drawn, so the first one is taken there.
                    state.update(|s| {
                        s.expanded = Some(row.id.clone());
                        s.flyout_cursor = Some(String::new());
                    });
                    true
                }
                Key::Left if in_flyout => {
                    state.update(|s| {
                        s.expanded = None;
                        s.flyout_cursor = None;
                    });
                    true
                }
                Key::Enter => {
                    let chosen = if in_flyout {
                        let at = state.borrow().flyout_cursor.clone();
                        at.and_then(|id| flyout_rows.iter().find(|row| row.id == id).cloned())
                    } else {
                        let at = state.borrow().cursor.clone();
                        at.and_then(|id| rows.iter().find(|row| row.id == id).cloned())
                    };
                    let Some(row) = chosen else { return false };
                    if row.expandable {
                        state.update(|s| {
                            s.expanded = if s.expanded.as_deref() == Some(row.id.as_str()) { None } else { Some(row.id.clone()) };
                        });
                    } else {
                        if let Some(f) = &row.on_select {
                            f(app);
                        }
                        anchor.close();
                    }
                    true
                }
                _ => false,
            }
        }
    };
    anchor.place(panel.on_key_global(keys))
}

struct RowCx {
    menu_id: u64,
    dense: bool,
    inline: bool,
    anchor: PopoverAnchor,
    state: Local<MenuState>,
    expanded: Option<String>,
    cursor: Option<String>,
    flyout_cursor: Option<String>,
}

fn action_button(cx: &RowCx, id: &str, action: PopoverMenuAction) -> E {
    let t_ = theme();
    say(&action.label);
    let (anchor, on_select) = (cx.anchor.clone(), action.on_select);
    row()
        .key(action_id(cx.menu_id, id))
        .center()
        .size(20.0, 20.0)
        .radius(5.0)
        .shrink0()
        .hover_bg(t_.subtle_hover)
        .press_bg(t_.subtle_pressed)
        .tooltip(action.label)
        .on_click(move |app: &mut App| {
            anchor.close();
            on_select(app);
        })
        .child(icon(action.icon, 13.0).color(t_.text_tertiary))
}

fn render_item(cx: &RowCx, mut item: PopoverMenuItem, depth: usize) -> E {
    let t_ = theme();
    let expandable = !item.children.is_empty();
    let is_expanded = expandable && cx.expanded.as_deref() == Some(item.id.as_str());
    let this_row = row_id(cx.menu_id, &item.id);
    let in_flyout = depth > 0 && !cx.inline;
    let keyed = if in_flyout { cx.flyout_cursor.as_deref() == Some(item.id.as_str()) } else { cx.cursor.as_deref() == Some(item.id.as_str()) };
    let fg = if item.disabled { t_.text_disabled } else { t_.text_primary };
    say(&item.label);

    let mut copy: E = col().flex1().min_w(0.0).gap(1.0).child(if item.label_is_path { path_text(("popover-row-path", cx.menu_id, item.id.as_str()), item.label.clone(), CAPTION, fg) } else { text(item.label.clone()).ty(CAPTION).color(fg).ellipsis() });
    if let Some(description) = &item.description {
        say(description);
        copy = copy.child(if item.description_is_path { path_text(("popover-row-description", cx.menu_id, item.id.as_str()), description.clone(), FINE, t_.text_tertiary) } else { text(description.clone()).ty(FINE).color(t_.text_tertiary).ellipsis() });
    }
    let mut button: E = row().key(this_row).min_h(24.0).px(7.0).py(2.0).gap(7.0).items_center().radius(5.0).shrink0();
    button = match (item.checked, item.checked_role) {
        (None, _) => button.role(Role::MenuItem),
        (Some(on), CheckedRole::Radio) => button.role(Role::MenuItemRadio).aria_checked(on),
        (Some(on), CheckedRole::Checkbox) => button.role(Role::MenuItemCheckBox).aria_checked(on),
    };
    if expandable {
        button = button.aria_haspopup(HasPopup::Menu).aria_expanded(is_expanded);
    }
    if let Some(leading) = item.icon {
        button = button.child(icon(leading, 14.0).color(if item.disabled { t_.text_disabled } else { t_.text_secondary }));
    }
    button = button.child(item.leading.take());
    button = button.child(copy);
    if let Some(hint) = &item.hint {
        say(hint);
        button = button.child(text(hint.clone()).ty(FINE).color(t_.text_tertiary).nowrap().shrink0());
    }
    if item.checked == Some(true) {
        say("checked");
        button = button.child(icon("Check", 14.0).color(t_.text_primary));
    }
    // After the checkmark, so badges line up at the edge whichever row is checked.
    button = button.child(item.badge.take());
    if expandable {
        button = button.child(icon("ChevronRight", 13.0).color(t_.text_tertiary).rotate(if is_expanded && cx.inline { 90.0 } else { 0.0 }));
    }
    if let Some(title) = item.title.clone() {
        button = if item.disabled { button.disabled_reason(title) } else { button.full_text(title) };
    }
    if keyed || (is_expanded && !cx.inline) {
        button = button.bg(t_.subtle_hover);
    }
    if item.disabled {
        button = button.disabled(true).on_click(|_: &mut App| {});
    } else {
        button = button.hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed);
        let (state, anchor) = (cx.state.clone(), cx.anchor.clone());
        let (id, on_select) = (item.id.clone(), item.on_select.clone());
        button = button.on_click(move |app: &mut App| {
            if expandable {
                state.update(|s| {
                    s.expanded = if s.expanded.as_deref() == Some(id.as_str()) { None } else { Some(id.clone()) };
                    s.flyout_cursor = None;
                });
                return;
            }
            if let Some(f) = &on_select {
                f(app);
            }
            anchor.close();
        });
    }

    let line: E = match item.action.take() {
        Some(action) => row().items_center().gap(2.0).w_full().shrink0().child(button.flex1().min_w(0.0)).child(action_button(cx, &format!("row:{}", item.id), action)),
        None => button,
    };
    let mut block: E = col().shrink0().child(line);
    if is_expanded {
        let children = std::mem::take(&mut item.children);
        if cx.inline {
            let mut nested: E = col().ml(if cx.dense { 8.0 } else { 10.0 }).pl(if cx.dense { 7.0 } else { 8.0 }).my(if cx.dense { 1.0 } else { 2.0 }).border_left(t_.divider).gap(if cx.dense { 0.0 } else { 1.0 }).role(Role::Menu).aria_label(item.label.clone());
            for child in children {
                nested = nested.child(render_item(cx, child, depth + 1));
            }
            block = block.child(nested);
        } else {
            let padding = if cx.dense { 3.0 } else { 5.0 };
            // Asked for the first row by the keyboard, which knows no row names: the marker resolves to it here.
            let first = children.iter().find(|child| !child.disabled).map(|child| child.id.clone());
            if cx.flyout_cursor.as_deref() == Some("")
                && let Some(first) = first
            {
                cx.state.update(|s| s.flyout_cursor = Some(first));
            }
            let mut panel: E = solid_panel().p(padding).min_w(150.0).max_w(PANEL_MAX_WIDTH).role(Role::Menu).aria_label(item.label.clone());
            for child in children {
                panel = panel.child(render_item(cx, child, depth + 1));
            }
            block = block.child(menu_flyout(("popover-submenu", cx.menu_id, item.id.as_str()), &cx.anchor, this_row, panel, (padding + 1.0, padding + 1.0), Hang::Down));
        }
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: (f32, f32) = (1000.0, 800.0);
    const PANEL_HEIGHT: f32 = 120.0;

    fn trigger(top: f32) -> Rect {
        rect(40.0, top, 90.0, 25.0)
    }

    #[test]
    fn opens_below_by_default_even_when_there_is_room_above() {
        let p = place_popover(trigger(500.0), (200.0, PANEL_HEIGHT), VIEW, Align::Start, Placement::Below);
        assert!(!p.flipped);
        assert_eq!(p.top, 500.0 + 25.0 + 6.0);
        assert_eq!(p.min_width, 90.0);
    }

    #[test]
    fn opens_above_the_trigger_when_asked_to_and_the_panel_fits_there() {
        let p = place_popover(trigger(500.0), (200.0, PANEL_HEIGHT), VIEW, Align::Start, Placement::Above);
        assert!(p.flipped);
        assert_eq!(p.top, 500.0 - 6.0 - PANEL_HEIGHT);
    }

    #[test]
    fn falls_back_below_when_there_is_no_room_above() {
        let p = place_popover(trigger(40.0), (200.0, PANEL_HEIGHT), VIEW, Align::Start, Placement::Above);
        assert!(!p.flipped);
        assert_eq!(p.top, 40.0 + 25.0 + 6.0);
    }

    #[test]
    fn a_default_panel_flips_only_when_it_does_not_fit_below_and_above_is_roomier() {
        let p = place_popover(trigger(760.0), (200.0, PANEL_HEIGHT), VIEW, Align::Start, Placement::Below);
        assert!(p.flipped);
        assert_eq!(p.top, 760.0 - 6.0 - PANEL_HEIGHT);
    }

    #[test]
    fn lines_up_with_the_end_edge_and_stays_inside_the_window() {
        let p = place_popover(rect(900.0, 10.0, 90.0, 25.0), (200.0, PANEL_HEIGHT), VIEW, Align::End, Placement::Below);
        assert_eq!(p.left, 990.0 - 200.0);
        let q = place_popover(rect(2.0, 10.0, 20.0, 25.0), (200.0, PANEL_HEIGHT), VIEW, Align::End, Placement::Below);
        assert_eq!(q.left, VIEWPORT_MARGIN);
        let r = place_popover(rect(950.0, 10.0, 20.0, 25.0), (200.0, PANEL_HEIGHT), VIEW, Align::Start, Placement::Below);
        assert_eq!(r.left, 1000.0 - 200.0 - VIEWPORT_MARGIN);
    }

    #[test]
    fn a_menu_at_a_point_hangs_in_the_upper_half_and_stands_in_the_lower() {
        assert_eq!(place_menu_at_point((100.0, 100.0), (200.0, 120.0), VIEW), (100.0, 100.0, false));
        assert_eq!(place_menu_at_point((100.0, 700.0), (200.0, 120.0), VIEW), (100.0, 580.0, true));
        // Only the left has room.
        assert_eq!(place_menu_at_point((900.0, 100.0), (200.0, 120.0), VIEW).0, 700.0);
    }

    #[test]
    fn a_submenu_lies_over_its_panels_edge_lined_up_with_its_row() {
        let parent = rect(100.0, 100.0, 200.0, 300.0);
        let row = rect(105.0, 150.0, 190.0, 24.0);
        assert_eq!(place_flyout(row, parent, (150.0, 80.0), (6.0, 6.0), Hang::Down, VIEW), (299.0, 144.0));
        assert_eq!(place_flyout(row, parent, (150.0, 80.0), (6.0, 6.0), Hang::Up, VIEW), (299.0, 100.0));
        // No room on the right: the left, its edge over the panel's left edge.
        let far = rect(800.0, 100.0, 195.0, 300.0);
        assert_eq!(place_flyout(row, far, (150.0, 80.0), (6.0, 6.0), Hang::Down, VIEW).0, 651.0);
        // Moved up by what would leave the window.
        assert_eq!(place_flyout(rect(105.0, 780.0, 190.0, 24.0), parent, (150.0, 80.0), (6.0, 6.0), Hang::Down, VIEW).1, 800.0 - 8.0 - 80.0);
    }

    #[test]
    fn search_matches_labels_descriptions_and_children_but_keeps_all_of_a_match() {
        let sections = vec![PopoverMenuSection::new("s")
            .item(PopoverMenuItem::new("a", "Alpha"))
            .item(PopoverMenuItem::new("b", "Beta").description("second letter"))
            .item(PopoverMenuItem::new("c", "Group").children(vec![PopoverMenuItem::new("x", "Deep"), PopoverMenuItem::new("y", "Other")]))];
        let visible = visible_sections(sections, "DEEP");
        assert_eq!(visible[0].items.len(), 1);
        assert_eq!(visible[0].items[0].children.len(), 2);
        let sections = vec![PopoverMenuSection::new("s").item(PopoverMenuItem::new("b", "Beta").description("second letter"))];
        assert_eq!(visible_sections(sections, "second").len(), 1);
        let sections = vec![PopoverMenuSection::new("s").item(PopoverMenuItem::new("a", "Alpha"))];
        assert!(visible_sections(sections, "zzz").is_empty());
    }

    #[test]
    fn arrows_wrap_around_the_rows() {
        let rows: Vec<KeyRow> = ["a", "b"].iter().map(|id| KeyRow { id: (*id).to_owned(), expandable: false, on_select: None }).collect();
        assert_eq!(step(&rows, None, true).as_deref(), Some("a"));
        assert_eq!(step(&rows, None, false).as_deref(), Some("b"));
        assert_eq!(step(&rows, Some("b"), true).as_deref(), Some("a"));
        assert_eq!(step(&rows, Some("a"), false).as_deref(), Some("b"));
    }
}
