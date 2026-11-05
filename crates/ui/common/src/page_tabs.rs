//! `PageTabs.tsx`: the title bar of a pane that shows one page at a time, as a single row of tabs that never scrolls. Every
//! tab is measured at rest, the strip draws the longest run from the start that fits, and the rest are rows of an
//! overflow menu at the strip's end; the active tab is always among the drawn ones. With `on_reorder`, the strip and the
//! menu are two views of one order: a tab drags within either and across between them.

use std::collections::HashMap;

use mewrk_client::prelude::*;

/// One page of a pane, as its tab.
pub struct PageTab {
    pub id: String,
    /// The tab's label, its overflow row, its hover text and the name it is called by.
    pub label: String,
    /// Drawn in the tab instead of `label`. The strip is fitted to `label`, so this has to draw at the label's width.
    pub content: Option<E>,
    /// Hover text; defaults to `label`.
    pub title: Option<String>,
    pub icon: Option<IconRef>,
    /// Drawn in the icon's place, for an icon that is more than a name: a file type's, a status spinner.
    pub leading: Option<E>,
    /// Italic label: the page is on loan.
    pub italic: bool,
    /// A short tag before the label, such as the number of the workspace a page belongs to.
    pub badge: Option<String>,
    /// Trailing secondary text on the tab's overflow-menu row.
    pub hint: Option<String>,
    pub closable: bool,
    /// The close control is shown but spent: the page is already on its way out.
    pub close_disabled: bool,
    /// Held at the strip's start, ahead of every tab that is not. The caller lists pinned tabs first.
    pub pinned: bool,
}

impl PageTab {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), content: None, title: None, icon: None, leading: None, italic: false, badge: None, hint: None, closable: true, close_disabled: false, pinned: false }
    }
}

pub struct PageTabs {
    /// Names this strip among all others.
    pub key: String,
    pub tabs: Vec<PageTab>,
    pub active_id: Option<String>,
    /// The tablist's name.
    pub aria_label: Arc<str>,
    pub more_label: Arc<str>,
    pub on_select: Rc<dyn Fn(&mut App, &str)>,
    pub on_close: Option<Rc<dyn Fn(&mut App, &str)>>,
    /// A tab's close control's name; defaults to "Close <label>".
    pub close_label: Option<Rc<dyn Fn(&PageTab) -> String>>,
    /// Delete and Backspace close the focused tab. Only for pages that cost nothing to reopen.
    pub close_on_delete_key: bool,
    /// Makes the order the user's. Receives every id, in the new order.
    pub on_reorder: Option<Rc<dyn Fn(&mut App, Vec<String>)>>,
    pub max_tab_width: f32,
    pub on_tab_context_menu: Option<Rc<dyn Fn(&mut App, &str, (f32, f32))>>,
    pub on_tab_double_click: Option<Rc<dyn Fn(&mut App, &str)>>,
    /// An editor drawn in a tab in place of its label: a rename field. The tab's id, and the editor.
    pub editor: Option<(String, E)>,
    /// Controls after the strip and its overflow trigger, such as the one that opens another page.
    pub trailing: Option<E>,
    /// The width `trailing` takes, for the fit.
    pub trailing_width: f32,
}

/// What the strip measured: the room it has and the width each tab takes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageTabsMetrics {
    pub widths: HashMap<String, f32>,
    pub available: f32,
    pub gap: f32,
    pub trigger: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageTabsFit {
    pub shown: Vec<String>,
    pub hidden: Vec<String>,
}

/// Sub-pixel slack, so a tab measured at 99.6 is not pushed out of a 99.5 gap by rounding.
const FIT_TOLERANCE: f32 = 0.5;
/// How far outside a list a dragged tab still counts as over it.
const DRAG_SLOP: f32 = 24.0;
/// How far a press has to move to be a drag rather than a click: `usePointerDrag`'s threshold.
const DRAG_THRESHOLD: f32 = 5.0;
const GAP: f32 = 2.0;
const TRIGGER: f32 = 24.0;
const TAB_H: f32 = 24.0;
const CLOSE: f32 = 18.0;
const LABEL_TY: TypeStyle = CAPTION;
const LEADING: f32 = 14.0;

/// Which tabs the strip draws and which go to the overflow menu.
///
/// The strip shows the longest run from the start that fits, leaving room for the overflow trigger whenever something
/// does not. The active tab is never the one left out: when it falls past the run it takes the run's last slot, more
/// than one if it is the wider, for as long as it stays active, without moving in the caller's order. With nothing
/// measured every tab is shown.
pub fn fit_page_tabs(ids: &[String], active_id: Option<&str>, metrics: Option<&PageTabsMetrics>) -> PageTabsFit {
    let shown_all = || PageTabsFit { shown: ids.to_vec(), hidden: Vec::new() };
    let Some(metrics) = metrics else { return shown_all() };
    if metrics.available <= 0.0 || ids.is_empty() {
        return shown_all();
    }
    let width = |id: &str| metrics.widths.get(id).copied().unwrap_or(0.0);
    let span = |run: &[String]| run.iter().enumerate().map(|(i, id)| width(id) + if i > 0 { metrics.gap } else { 0.0 }).sum::<f32>();
    if span(ids) <= metrics.available + FIT_TOLERANCE {
        return shown_all();
    }
    let budget = metrics.available - metrics.gap - metrics.trigger + FIT_TOLERANCE;
    let mut count = 0;
    let mut used = -metrics.gap;
    for id in ids {
        let next = used + metrics.gap + width(id);
        if next > budget {
            break;
        }
        used = next;
        count += 1;
    }
    let mut shown: Vec<String> = ids[..count].to_vec();
    if let Some(index) = active_id.and_then(|a| ids.iter().position(|id| id == a))
        && index >= count
    {
        let active = ids[index].clone();
        shown.truncate(count.saturating_sub(1));
        while !shown.is_empty() {
            let mut trial = shown.clone();
            trial.push(active.clone());
            if span(&trial) <= budget {
                break;
            }
            shown.pop();
        }
        shown.push(active);
    }
    // Too narrow for even one tab and the trigger: the one that matters most still shows, clipped.
    if shown.is_empty() {
        shown.push(ids[0].clone());
    }
    let hidden = ids.iter().filter(|id| !shown.contains(id)).cloned().collect();
    PageTabsFit { shown, hidden }
}

/// `reorderItems`: `items` with `source` moved before or after `target`.
pub fn reorder_ids(items: &[String], source: &str, target: &str, after: bool) -> Vec<String> {
    if source == target {
        return items.to_vec();
    }
    let (Some(from), true) = (items.iter().position(|i| i == source), items.iter().any(|i| i == target)) else { return items.to_vec() };
    let mut next = items.to_vec();
    let moving = next.remove(from);
    let at = next.iter().position(|i| i == target).expect("checked");
    next.insert(at + usize::from(after), moving);
    next
}

/// Where a dragged tab would land.
#[derive(Clone, Debug, PartialEq)]
pub struct DropSlot {
    pub id: String,
    pub after: bool,
}

/// The slot in a list nearest the pointer, along the axis the list runs: before or after one of its rows, the dragged
/// one aside. `None` once the pointer is more than `slop` outside the list.
pub fn slot_at(list: Option<Rect>, rows: &[(String, Rect)], source: &str, point: (f32, f32), horizontal: bool, slop: f32) -> Option<DropSlot> {
    let list = list?;
    if list.w <= 0.0 || list.h <= 0.0 {
        return None;
    }
    if point.0 < list.x - slop || point.0 > list.right() + slop || point.1 < list.y - slop || point.1 > list.bottom() + slop {
        return None;
    }
    let coordinate = if horizontal { point.0 } else { point.1 };
    let mut nearest: Option<(&str, f32, f32)> = None;
    for (id, r) in rows {
        if id == source || r.w <= 0.0 || r.h <= 0.0 {
            continue;
        }
        let center = if horizontal { r.x + r.w / 2.0 } else { r.y + r.h / 2.0 };
        let distance = (coordinate - center).abs();
        if nearest.is_none_or(|(_, _, d)| distance < d) {
            nearest = Some((id, center, distance));
        }
    }
    let (id, center, _) = nearest?;
    Some(DropSlot { id: id.to_owned(), after: coordinate >= center })
}

#[derive(Clone, Default)]
struct Drag {
    source: Option<String>,
    dragging: bool,
    slot: Option<(DropSlot, bool)>,
    /// Swallows the click that ends a drag.
    suppress_click: bool,
}

fn tab_key(strip: &str, id: &str) -> u64 {
    hash_key(("page-tab", strip, id))
}

fn close_control(strip: &str, tab: &PageTab, label: String, active: bool, on_close: Rc<dyn Fn(&mut App, &str)>) -> E {
    let t_ = theme();
    let id = tab.id.clone();
    let mut b: E = row().key(("page-tab-close", strip, tab.id.as_str())).center().size(CLOSE, CLOSE).radius(5.0).mr(3.0).shrink0().tooltip(label).child(icon("X", 11.0).color(t_.text_secondary));
    if tab.close_disabled {
        b = b.disabled(true);
    } else {
        b = b.hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).on_click(move |app: &mut App| on_close(app, &id));
    }
    // Present but unlit until the tab is pointed at or in use, so a row of tabs reads as names rather than as controls.
    b.reveal_unless(active)
}

/// The strip, as a row that fills its container.
pub fn page_tabs(props: PageTabs) -> E {
    let t_ = theme();
    let PageTabs { key, tabs, active_id, aria_label, more_label, on_select, on_close, close_label, close_on_delete_key, on_reorder, max_tab_width, on_tab_context_menu, on_tab_double_click, mut editor, trailing, trailing_width } = props;
    let root_key = hash_key(("page-tabs", key.as_str()));
    let metrics = use_state(("page-tabs-metrics", root_key), || None::<PageTabsMetrics>);
    let drag = use_state(("page-tabs-drag", root_key), Drag::default);
    let menu_open = use_state(("page-tabs-menu", root_key), || false);

    let ids: Vec<String> = tabs.iter().map(|tab| tab.id.clone()).collect();
    let pinned_count = tabs.iter().filter(|tab| tab.pinned).count();
    let current_id = active_id.filter(|a| ids.contains(a));
    let fit = fit_page_tabs(&ids, current_id.as_deref(), metrics.borrow().as_ref());
    let hidden_any = !fit.hidden.is_empty();
    let reorderable = on_reorder.is_some();

    // What the fit reads, taken where text can be measured. A frame's fit may lag by that one frame, which the measuring
    // leaf repairs by asking for another.
    let measure_inputs: Vec<(String, String, Option<f32>, bool, bool)> = tabs.iter().map(|tab| (tab.id.clone(), tab.label.clone(), if tab.leading.is_some() { Some(LEADING) } else { tab.icon.is_some().then_some(12.0) }, tab.badge.is_some(), tab.closable && on_close.is_some())).collect();
    let measure_metrics = metrics.clone();
    let measurer: E = custom(move |_, ui: &mut Ui<'_>, _| {
        let mut widths = HashMap::new();
        for (id, label, icon_width, has_badge, closable) in &measure_inputs {
            let mut w = 8.0 + ui.measure(label, &LABEL_TY);
            if let Some(icon_width) = icon_width {
                w += icon_width + 5.0;
            }
            if *has_badge {
                w += 14.0 + 5.0;
            }
            w += if *closable { GAP + CLOSE + 3.0 } else { 8.0 };
            widths.insert(id.clone(), w.ceil().min(max_tab_width));
        }
        let available = rect_of(root_key).map_or(0.0, |r| r.w) - if trailing_width > 0.0 { trailing_width + GAP } else { 0.0 };
        let next = PageTabsMetrics { widths, available, gap: GAP, trigger: TRIGGER };
        if measure_metrics.borrow().as_ref() != Some(&next) {
            measure_metrics.set(Some(next));
        }
    })
    .size(1.0, 1.0)
    .absolute()
    .pass_through();

    let strip_key = hash_key(("page-tabs-strip", root_key));
    let panel_key = hash_key(("page-tabs-panel", root_key));
    let trigger_key = hash_key(("page-tabs-trigger", root_key));

    // Finds where a dragged tab would land: over the open menu it is the menu's, otherwise the strip's with some slop,
    // and nothing lands among the pinned tabs.
    let drop_slot_at = {
        let (shown, hidden, ids, pinned_ids, strip) = (fit.shown.clone(), fit.hidden.clone(), ids.clone(), tabs.iter().filter(|tab| tab.pinned).map(|tab| tab.id.clone()).collect::<Vec<_>>(), key.clone());
        let menu_is_open = menu_open.clone();
        Rc::new(move |source: &str, point: (f32, f32)| -> Option<(DropSlot, bool)> {
            let unpinned = |slot: Option<DropSlot>| slot.map(|slot| if pinned_ids.contains(&slot.id) { DropSlot { id: ids[pinned_count - 1].clone(), after: true } } else { slot });
            let rects = |list: &[String], menu: bool| -> Vec<(String, Rect)> {
                list.iter().filter_map(|id| rect_of(if menu { hash_key(("page-tab-row", strip.as_str(), id.as_str())) } else { tab_key(&strip, id) }).map(|r| (id.clone(), r))).collect()
            };
            let panel = if menu_is_open.get() { rect_of(panel_key) } else { None };
            let in_menu = |slop: f32| unpinned(slot_at(panel, &rects(&hidden, true), source, point, false, slop)).map(|s| (s, true));
            if panel.is_some_and(|p| p.contains(point.0, point.1)) {
                return in_menu(0.0);
            }
            match unpinned(slot_at(rect_of(strip_key), &rects(&shown, false), source, point, true, DRAG_SLOP)) {
                Some(slot) => Some((slot, false)),
                None => in_menu(DRAG_SLOP),
            }
        })
    };

    let pinned_ids: Vec<String> = tabs.iter().filter(|tab| tab.pinned).map(|tab| tab.id.clone()).collect();
    let drag_handler = |source_id: &str| -> Option<PointerHandler<App>> {
        if !reorderable || pinned_ids.iter().any(|id| id == source_id) {
            return None;
        }
        let (drag, ids, drop_at, on_reorder, source) = (drag.clone(), ids.clone(), drop_slot_at.clone(), on_reorder.clone(), source_id.to_owned());
        Some(Rc::new(move |app: &mut App, e: PointerEvent| match e.phase {
            Phase::Down => drag.update(|d| {
                *d = Drag { source: Some(source.clone()), ..Drag::default() };
            }),
            Phase::Move => {
                let far = (e.dx * e.dx + e.dy * e.dy).sqrt() >= DRAG_THRESHOLD;
                drag.update(|d| {
                    if d.source.is_none() {
                        return;
                    }
                    if d.dragging || far {
                        d.dragging = true;
                        d.slot = drop_at(&source, (e.x, e.y));
                    }
                });
            }
            Phase::Up => {
                let finished = drag.update(|d| {
                    let finished = d.dragging.then(|| d.slot.clone()).flatten();
                    d.suppress_click = d.dragging;
                    d.source = None;
                    d.dragging = false;
                    d.slot = None;
                    finished
                });
                if let Some((slot, _)) = finished {
                    let next = reorder_ids(&ids, &source, &slot.id, slot.after);
                    if next != ids
                        && let Some(f) = &on_reorder
                    {
                        f(app, next);
                    }
                }
            }
        }))
    };

    let dragging_now = drag.with(|d| d.dragging);
    let slot_now = drag.with(|d| d.slot.clone());
    let source_now = drag.with(|d| if d.dragging { d.source.clone() } else { None });

    let mut by_id: HashMap<String, PageTab> = tabs.into_iter().map(|tab| (tab.id.clone(), tab)).collect();
    let close_name = |tab: &PageTab| -> String { close_label.as_ref().map_or_else(|| tf("关闭 {name}", "Close {name}", &[("name", &tab.label)]), |f| f(tab)) };

    // ---- the strip
    let mut strip: E = row().key(strip_key).items_center().gap(GAP).min_w(0.0).shrink0().with_style(|s| s.flex_shrink = 1.0).clip().role(Role::TabList).aria_label(aria_label);
    // One Tab stop in the strip: `tabStopId`.
    let tab_stop = current_id.clone().or_else(|| fit.shown.first().cloned());
    for id in &fit.shown {
        let Some(tab) = by_id.get_mut(id) else { continue };
        let active = current_id.as_deref() == Some(id.as_str());
        let tkey = tab_key(&key, id);
        let is_editing = editor.as_ref().is_some_and(|(eid, _)| eid == id);
        let mut boxed: E = row().key(tkey).items_center().gap(GAP).h(TAB_H).radius(6.0).min_w(0.0).with_style(|s| s.flex_shrink = 1.0).max_w(max_tab_width);
        if is_editing {
            let (_, field) = editor.take().expect("checked");
            strip = strip.child(boxed.shrink0().child(field));
            continue;
        }
        if active {
            boxed = boxed.bg(t_.subtle_pressed);
        }
        boxed = boxed.hover_bg(t_.subtle_hover);
        if source_now.as_deref() == Some(id.as_str()) {
            boxed = boxed.opacity(0.45);
        }
        let color = if active { t_.text_primary } else { t_.text_secondary };
        // The button fills the tab, so the whole of it selects and drags, not just the name.
        let mut label_box: E = row().key(("page-tab-label", key.as_str(), id.as_str())).items_center().gap(5.0).h_full().pl(8.0).radius(6.0).flex1().full_text(tab.title.clone().unwrap_or_else(|| tab.label.clone())).role(Role::Tab).aria_selected(active);
        match tab.leading.take() {
            Some(leading) => label_box = label_box.child(leading),
            None => {
                if let Some(i) = tab.icon {
                    label_box = label_box.child(icon(i, 12.0).color(t_.text_tertiary));
                }
            }
        }
        if let Some(badge) = &tab.badge {
            label_box = label_box.child(badge_mark(badge));
        }
        label_box = match tab.content.take() {
            Some(content) => label_box.child(content.flex1()),
            None => label_box.child(text(tab.label.clone()).ty(TypeStyle { italic: tab.italic, ..LABEL_TY }).color(color).ellipsis().flex1()),
        };
        let (select, id_owned, swallow) = (on_select.clone(), id.clone(), drag.clone());
        label_box = label_box.skip_tab(tab_stop.as_deref() != Some(id.as_str())).on_click(move |app: &mut App| {
            if swallow.update(|d| std::mem::take(&mut d.suppress_click)) {
                return;
            }
            select(app, &id_owned);
        });
        if let Some(f) = on_tab_double_click.clone() {
            let id_owned = id.clone();
            label_box = label_box.on_double_click(move |app: &mut App| f(app, &id_owned));
        }
        if let Some(f) = on_tab_context_menu.clone() {
            let id_owned = id.clone();
            label_box = label_box.on_context_menu(move |app: &mut App, e| f(app, &id_owned, (e.x, e.y)));
        }
        // The middle button closes what the × would: not a tab that cannot close, nor one whose × is spent.
        if let (true, false, Some(f)) = (tab_has_close(tab, on_close.is_some()), tab.close_disabled, on_close.clone()) {
            let id_owned = id.clone();
            label_box = label_box.on_middle_click(move |app: &mut App| f(app, &id_owned));
        }
        if let Some(h) = drag_handler(id) {
            label_box = label_box.on_pointer(move |app: &mut App, e| h(app, e));
        }
        // The arrows move the selection in the caller's order, wrapping, and with Ctrl+Shift they move the tab.
        label_box = label_box.on_key({
            let (ids, select, on_reorder, on_close, pinned, closable, strip_name, id_owned) = (ids.clone(), on_select.clone(), on_reorder.clone(), on_close.clone(), tab.pinned, tab.closable && !tab.close_disabled, key.clone(), id.clone());
            move |app: &mut App, press: &KeyPress| tab_key_press(app, press, &ids, &id_owned, pinned, pinned_count, closable && close_on_delete_key, &select, on_reorder.as_ref(), on_close.as_ref(), &strip_name)
        });
        if !tab_has_close(tab, on_close.is_some()) {
            label_box = label_box.pr(8.0);
        }
        boxed = boxed.child(label_box);
        if tab_has_close(tab, on_close.is_some())
            && let Some(f) = on_close.clone()
        {
            boxed = boxed.child(close_control(&key, tab, close_name(tab), active, f));
        }
        if let Some((slot, in_menu)) = &slot_now
            && !*in_menu
            && slot.id == *id
        {
            boxed = boxed.child(drop_mark(slot.after, true));
        }
        strip = strip.child(boxed);
    }

    let mut root: E = row().key(root_key).items_center().gap(GAP).flex1().child(measurer).child(strip);

    // ---- the overflow menu
    if hidden_any {
        let toggle = menu_open.clone();
        let mut trigger: E = row().key(trigger_key).center().size(TRIGGER, TRIGGER).radius(CONTROL_RADIUS).shrink0().hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).tooltip(more_label.clone()).aria_haspopup(HasPopup::Menu).aria_expanded(menu_open.get()).child(icon("ChevronDown", 13.0).color(t_.text_secondary)).on_click(move |_| toggle.update(|v| *v = !*v));
        if menu_open.get() {
            let close_menu = menu_open.clone();
            let mut panel = flyout_panel::<App>().key(panel_key).min_w(180.0).max_w(340.0).p(3.0).role(Role::Menu).aria_label(more_label.clone());
            for id in &fit.hidden {
                let Some(tab) = by_id.get_mut(id) else { continue };
                let row_key = hash_key(("page-tab-row", key.as_str(), id.as_str()));
                let mut r: E = row().key(row_key).items_center().gap(5.0).h(24.0).px(7.0).radius(5.0).hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).full_text(tab.title.clone().unwrap_or_else(|| tab.label.clone())).role(Role::MenuItem);
                if source_now.as_deref() == Some(id.as_str()) {
                    r = r.opacity(0.45);
                }
                match tab.leading.take() {
                    Some(leading) => r = r.child(leading),
                    None => {
                        if let Some(i) = tab.icon {
                            r = r.child(icon(i, 12.0).color(t_.text_tertiary));
                        }
                    }
                }
                if let Some(badge) = &tab.badge {
                    r = r.child(badge_mark(badge));
                }
                r = r.child(text(tab.label.clone()).ty(TypeStyle { italic: tab.italic, ..LABEL_TY }).ellipsis().flex1());
                if let Some(hint) = &tab.hint {
                    r = r.child(text(hint.clone()).ty(FINE).color(t_.text_tertiary).nowrap().ml(8.0));
                }
                let (select, id_owned, closer, swallow) = (on_select.clone(), id.clone(), close_menu.clone(), drag.clone());
                r = r.on_click(move |app: &mut App| {
                    if swallow.update(|d| std::mem::take(&mut d.suppress_click)) {
                        return;
                    }
                    closer.set(false);
                    select(app, &id_owned);
                });
                if let Some(h) = drag_handler(id) {
                    r = r.on_pointer(move |app: &mut App, e| h(app, e));
                }
                if let (true, false, Some(f)) = (tab_has_close(tab, on_close.is_some()), tab.close_disabled, on_close.clone()) {
                    let id_owned = id.clone();
                    r = r.on_middle_click(move |app: &mut App| f(app, &id_owned));
                }
                if tab_has_close(tab, on_close.is_some())
                    && let Some(f) = on_close.clone()
                {
                    let id_owned = id.clone();
                    let mut x: E = row().key(("page-tab-row-close", key.as_str(), id.as_str())).center().size(18.0, 18.0).radius(4.0).shrink0().tooltip(close_name(tab)).child(icon("X", 12.0).color(t_.text_secondary));
                    if tab.close_disabled {
                        x = x.disabled(true);
                    } else {
                        x = x.hover_bg(t_.subtle_hover).on_click(move |app: &mut App| f(app, &id_owned));
                    }
                    r = r.child(x);
                }
                if let Some((slot, in_menu)) = &slot_now
                    && *in_menu
                    && slot.id == *id
                {
                    r = r.child(drop_mark(slot.after, false));
                }
                panel = panel.child(r);
            }
            // A press on a tab keeps the menu open while tabs can be dragged into it.
            let dismiss = menu_open.clone();
            let strip_rect_key = strip_key;
            panel = panel.popover(trigger_key, Side::Below, Align::End, 4.0, move |_| {
                let on_strip = reorderable && rect_of(strip_rect_key).is_some() && dragging_now;
                if !on_strip {
                    dismiss.set(false);
                }
            });
            trigger = trigger.child(panel);
        }
        root = root.child(trigger);
    }
    if !hidden_any && menu_open.get() {
        menu_open.set(false);
    }
    if let Some(t) = trailing {
        root = root.child(spacer()).child(t);
    }
    if dragging_now {
        // Escape ends a drag before it can reach the menu, whose own Escape would close the menu and leave the drag live.
        let cancel = drag.clone();
        root = root.on_key_global(move |_, press| {
            if press.key == Key::Escape {
                cancel.set(Drag::default());
                return true;
            }
            false
        });
    }
    root
}

fn tab_has_close(tab: &PageTab, has_on_close: bool) -> bool {
    has_on_close && tab.closable
}

fn badge_mark(badge: &str) -> E {
    let t_ = theme();
    row().center().min_w(14.0).h(14.0).px(3.0).radius(4.0).shrink0().bg(t_.subtle_hover).child(text(badge.to_owned()).ty(FINE_STRONG).color(t_.text_secondary).nowrap().aria_hidden(true))
}

/// The landing spot of a drag: a line at one edge of the row it would land beside.
fn drop_mark(after: bool, horizontal: bool) -> E {
    let t_ = theme();
    let mark: E = div().bg(t_.accent).radius(1.0).pass_through();
    if horizontal {
        let m = mark.w(2.0).absolute().top(2.0).bottom(2.0);
        if after { m.right(-2.0) } else { m.left(-2.0) }
    } else {
        let m = mark.h(2.0).absolute().left(2.0).right(2.0);
        if after { m.bottom(-1.0) } else { m.top(-1.0) }
    }
}

#[allow(clippy::too_many_arguments)]
fn tab_key_press(app: &mut App, press: &KeyPress, ids: &[String], id: &str, pinned: bool, pinned_count: usize, delete_closes: bool, select: &Rc<dyn Fn(&mut App, &str)>, on_reorder: Option<&Rc<dyn Fn(&mut App, Vec<String>)>>, on_close: Option<&Rc<dyn Fn(&mut App, &str)>>, strip: &str) -> bool {
    let Some(index) = ids.iter().position(|i| i == id) else { return false };
    let go = |app: &mut App, next: &str| {
        select(app, next);
        focus_request(("page-tab-label", strip, next));
    };
    match press.key {
        Key::Left | Key::Right => {
            let delta: i64 = if press.key == Key::Left { -1 } else { 1 };
            if press.ctrl && press.shift && !press.alt && !press.cmd {
                let Some(reorder) = on_reorder else { return false };
                if pinned {
                    return true;
                }
                let destination = index as i64 + delta;
                if destination < pinned_count as i64 || destination >= ids.len() as i64 {
                    return true;
                }
                let mut next: Vec<String> = ids.iter().filter(|i| *i != id).cloned().collect();
                next.insert(destination as usize, id.to_owned());
                reorder(app, next);
                return true;
            }
            if press.ctrl || press.shift || press.alt || press.cmd {
                return false;
            }
            let next = &ids[(index as i64 + delta).rem_euclid(ids.len() as i64) as usize];
            go(app, next);
            true
        }
        Key::Home | Key::End => {
            let next = if press.key == Key::Home { &ids[0] } else { &ids[ids.len() - 1] };
            go(app, next);
            true
        }
        Key::Delete | Key::Backspace if delete_closes && !press.alt => {
            if let Some(f) = on_close {
                f(app, id);
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|i| (*i).to_owned()).collect()
    }

    fn metrics(widths: &[(&str, f32)], available: f32) -> PageTabsMetrics {
        PageTabsMetrics { widths: widths.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(), available, gap: 2.0, trigger: 24.0 }
    }

    #[test]
    fn with_nothing_measured_every_tab_is_shown() {
        let ids = s(&["a", "b", "c"]);
        assert_eq!(fit_page_tabs(&ids, None, None).shown, ids);
        assert_eq!(fit_page_tabs(&ids, None, Some(&metrics(&[], 0.0))).hidden, s(&[]));
    }

    #[test]
    fn shows_the_longest_run_that_fits_beside_the_trigger_and_hides_the_rest_in_order() {
        let ids = s(&["a", "b", "c", "d"]);
        let m = metrics(&[("a", 100.0), ("b", 100.0), ("c", 100.0), ("d", 100.0)], 410.0);
        // 100*4 + 2*3 = 406 fits exactly.
        assert!(fit_page_tabs(&ids, None, Some(&m)).hidden.is_empty());
        let fit = fit_page_tabs(&ids, None, Some(&metrics(&[("a", 100.0), ("b", 100.0), ("c", 100.0), ("d", 100.0)], 300.0)));
        assert_eq!(fit.shown, s(&["a", "b"]));
        assert_eq!(fit.hidden, s(&["c", "d"]));
    }

    #[test]
    fn the_active_tab_takes_the_runs_last_slot_and_more_when_it_is_the_wider() {
        let ids = s(&["a", "b", "c", "d"]);
        let m = metrics(&[("a", 100.0), ("b", 100.0), ("c", 100.0), ("d", 100.0)], 300.0);
        assert_eq!(fit_page_tabs(&ids, Some("d"), Some(&m)).shown, s(&["a", "d"]));
        let wide = metrics(&[("a", 100.0), ("b", 100.0), ("c", 100.0), ("d", 200.0)], 300.0);
        assert_eq!(fit_page_tabs(&ids, Some("d"), Some(&wide)).shown, s(&["d"]));
    }

    #[test]
    fn keeps_one_tab_on_screen_however_narrow_the_bar() {
        let ids = s(&["a", "b"]);
        let fit = fit_page_tabs(&ids, None, Some(&metrics(&[("a", 100.0), ("b", 100.0)], 10.0)));
        assert_eq!(fit.shown, s(&["a"]));
        assert_eq!(fit.hidden, s(&["b"]));
    }

    #[test]
    fn reorder_moves_a_tab_before_or_after_another() {
        let ids = s(&["a", "b", "c"]);
        assert_eq!(reorder_ids(&ids, "a", "c", true), s(&["b", "c", "a"]));
        assert_eq!(reorder_ids(&ids, "c", "a", false), s(&["c", "a", "b"]));
        assert_eq!(reorder_ids(&ids, "a", "a", true), ids);
        assert_eq!(reorder_ids(&ids, "a", "nope", true), ids);
    }

    #[test]
    fn a_slot_is_the_nearest_row_and_the_pointer_says_which_side() {
        let rows = vec![("a".to_owned(), rect(0.0, 0.0, 100.0, 24.0)), ("b".to_owned(), rect(102.0, 0.0, 100.0, 24.0)), ("c".to_owned(), rect(204.0, 0.0, 100.0, 24.0))];
        let list = Some(rect(0.0, 0.0, 304.0, 24.0));
        assert_eq!(slot_at(list, &rows, "a", (140.0, 10.0), true, 24.0), Some(DropSlot { id: "b".into(), after: false }));
        assert_eq!(slot_at(list, &rows, "a", (170.0, 10.0), true, 24.0), Some(DropSlot { id: "b".into(), after: true }));
        // The dragged one is not a landing spot.
        assert_eq!(slot_at(list, &rows, "b", (150.0, 10.0), true, 24.0).unwrap().id, "a");
        assert_eq!(slot_at(list, &rows, "a", (140.0, 100.0), true, 24.0), None, "too far outside the list");
    }
}
