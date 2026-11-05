//! `PopoverMenu.test.tsx`, by way of the real frame.

use super::rig::Rig;
use mewrk_client::prelude::*;
use mewrk_ui_common::popover_menu::*;
use std::cell::{Cell, RefCell};

const MENU: &str = "test-menu";

fn trigger(_open: bool) -> E {
    row().w(90.0).h(25.0).child(text("menu"))
}

fn rig_with(build: impl Fn() -> PopoverMenu + 'static) -> Rig {
    Rig::new().with_view(move |_| col().size(1100.0, 800.0).p(40.0).child(build().build()))
}

#[test]
fn puts_a_rows_and_a_headings_action_beside_them_and_closes_on_it() {
    let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
    let sink = log.clone();
    let mut rig = rig_with(move || {
        let sink = sink.clone();
        PopoverMenu::new(MENU, "machines", trigger).sections(move || {
            let (row_log, action_log, heading_log) = (sink.clone(), sink.clone(), sink.clone());
            vec![PopoverMenuSection::new("machines").label("here").action(PopoverMenuAction::new("settings of here", "Settings", move |_| heading_log.borrow_mut().push("heading"))).item(PopoverMenuItem::new("app", "app").on(move |_| row_log.borrow_mut().push("row")).action(PopoverMenuAction::new("variables of app", "Settings", move |_| action_log.borrow_mut().push("row action"))))]
        })
    });
    rig.click(trigger_key(MENU));
    rig.settle(30);
    // Nested buttons are invalid here too: the action does not lie inside the row.
    let row = rig.rect(row_key(MENU, "app"));
    let action = rig.rect(action_key(MENU, "row:app"));
    assert!(action.x >= row.right() - 0.5, "{action:?} beside {row:?}");
    rig.click(action_key(MENU, "row:app"));
    assert_eq!(*log.borrow(), ["row action"]);
    assert!(!rig.has(panel_key(MENU)), "choosing it closes the menu");

    rig.click(trigger_key(MENU));
    rig.settle(30);
    rig.click(action_key(MENU, "section:machines"));
    assert_eq!(*log.borrow(), ["row action", "heading"]);
    assert!(!rig.has(panel_key(MENU)));
}

#[test]
fn opens_above_when_asked_and_it_fits_there() {
    let mut low = Rig::new().with_view(|_| col().size(1100.0, 800.0).justify_end().p(40.0).child(PopoverMenu::new(MENU, "m", trigger).above().sections(|| vec![PopoverMenuSection::new("s").item(PopoverMenuItem::new("a", "A"))]).build()));
    low.click(trigger_key(MENU));
    low.settle(30);
    let (panel, chip) = (low.rect(panel_key(MENU)), low.rect(trigger_key(MENU)));
    assert!((panel.bottom() - (chip.y - 6.0)).abs() < 1.0, "its foot six pixels over the chip: {panel:?} {chip:?}");
}

#[test]
fn falls_back_below_when_there_is_no_room_above() {
    let mut high = Rig::new().with_view(|_| col().size(1100.0, 800.0).p(10.0).child(PopoverMenu::new(MENU, "m", trigger).above().sections(|| vec![PopoverMenuSection::new("s").item(PopoverMenuItem::new("a", "A"))]).build()));
    high.click(trigger_key(MENU));
    high.settle(30);
    let (panel, chip) = (high.rect(panel_key(MENU)), high.rect(trigger_key(MENU)));
    assert!((panel.y - (chip.bottom() + 6.0)).abs() < 1.0, "no room above: {panel:?} {chip:?}");
}

#[test]
fn opens_below_by_default_even_when_there_is_room_above() {
    let mut plain = Rig::new().with_view(|_| col().size(1100.0, 800.0).p(300.0).child(PopoverMenu::new(MENU, "m", trigger).sections(|| vec![PopoverMenuSection::new("s").item(PopoverMenuItem::new("a", "A"))]).build()));
    plain.click(trigger_key(MENU));
    plain.settle(30);
    let (panel, chip) = (plain.rect(panel_key(MENU)), plain.rect(trigger_key(MENU)));
    assert!((panel.y - (chip.bottom() + 6.0)).abs() < 1.0, "below, though there is room above: {panel:?} {chip:?}");
}

#[test]
fn stays_open_while_its_own_list_scrolls_and_closes_when_the_page_behind_it_scrolls() {
    let mut rig = rig_with(|| PopoverMenu::new(MENU, "m", trigger).sections(|| vec![PopoverMenuSection::new("s").items((0..80).map(|i| PopoverMenuItem::new(format!("r{i}"), format!("row {i}"))))]));
    rig.click(trigger_key(MENU));
    rig.settle(30);
    let panel = rig.rect(panel_key(MENU));
    assert!(panel.h <= 440.0, "the list scrolls rather than growing: {panel:?}");
    rig.input.pointer = Some((panel.x + 20.0, panel.y + 40.0));
    rig.frame();
    rig.input.scroll = -60.0;
    rig.input.scroll_is_pixels = true;
    rig.frame();
    rig.settle(20);
    assert!(rig.has(panel_key(MENU)), "its own list scrolled");

    rig.input.pointer = Some((1000.0, 700.0));
    rig.frame();
    rig.input.scroll = -60.0;
    rig.frame();
    rig.settle(20);
    assert!(!rig.has(panel_key(MENU)), "the measured position would go stale");
}

#[test]
fn opens_a_nested_list_beside_its_row_in_flyout_mode_and_closes_after_a_choice_in_it() {
    let chosen = Rc::new(Cell::new(false));
    let mark = chosen.clone();
    let mut rig = rig_with(move || {
        let mark = mark.clone();
        PopoverMenu::new(MENU, "backends", trigger).submenu(Submenu::Flyout).sections(move || {
            let mark = mark.clone();
            vec![PopoverMenuSection::new("b").item(PopoverMenuItem::new("native", "Native").checked(true).children(vec![PopoverMenuItem::new("v1", "web_search").checked(true).on(move |_| mark.set(true))])).item(PopoverMenuItem::new("provider", "Provider").checked(false))]
        })
    });
    rig.click(trigger_key(MENU));
    rig.settle(30);
    assert!(!rig.has(row_key(MENU, "v1")));
    rig.click(row_key(MENU, "native"));
    rig.settle(30);
    let (panel, nested) = (rig.rect(panel_key(MENU)), rig.rect(row_key(MENU, "v1")));
    assert!(nested.x >= panel.right() - 1.0, "a panel of its own beside the menu: {nested:?} {panel:?}");
    // A press in the submenu is a press in the menu: the row was chosen, and the menu closed after.
    rig.click(row_key(MENU, "v1"));
    assert!(chosen.get());
    assert!(!rig.has(panel_key(MENU)));
}

#[test]
fn a_press_in_the_menu_leaves_a_submenu_alone_and_one_outside_closes_everything() {
    let mut rig = rig_with(|| PopoverMenu::new(MENU, "m", trigger).submenu(Submenu::Flyout).sections(|| vec![PopoverMenuSection::new("b").label("Heading").item(PopoverMenuItem::new("native", "Native").children(vec![PopoverMenuItem::new("v1", "One")]))]));
    rig.click(trigger_key(MENU));
    rig.settle(30);
    rig.click(row_key(MENU, "native"));
    rig.settle(30);
    assert!(rig.has(row_key(MENU, "v1")));
    let panel = rig.rect(panel_key(MENU));
    rig.click_at(panel.x + 30.0, panel.y + 12.0);
    rig.settle(20);
    assert!(rig.has(row_key(MENU, "v1")), "the press landed on the heading");
    rig.click_at(1000.0, 700.0);
    rig.settle(20);
    assert!(!rig.has(panel_key(MENU)));
    assert!(!rig.has(row_key(MENU, "v1")));
}

#[test]
fn walks_into_a_submenu_with_the_right_arrow_and_back_out_with_the_left() {
    let chosen: Rc<RefCell<Vec<&'static str>>> = Rc::default();
    let sink = chosen.clone();
    let mut rig = rig_with(move || {
        let sink = sink.clone();
        PopoverMenu::new(MENU, "m", trigger).submenu(Submenu::Flyout).sections(move || {
            let (one, two) = (sink.clone(), sink.clone());
            vec![PopoverMenuSection::new("b").item(PopoverMenuItem::new("native", "Native").children(vec![PopoverMenuItem::new("v1", "One").on(move |_| one.borrow_mut().push("one")), PopoverMenuItem::new("v2", "Two").on(move |_| two.borrow_mut().push("two"))])).item(PopoverMenuItem::new("provider", "Provider"))]
        })
    });
    rig.click(trigger_key(MENU));
    rig.settle(30);
    rig.press(Key::Down);
    rig.press(Key::Right);
    rig.settle(30);
    assert!(rig.has(row_key(MENU, "v1")), "the right arrow opens it");
    rig.press(Key::Left);
    rig.settle(30);
    assert!(!rig.has(row_key(MENU, "v1")), "the left arrow goes back to its row");
    rig.press(Key::Right);
    rig.settle(30);
    rig.press(Key::Down);
    rig.press(Key::Enter);
    rig.settle(20);
    assert_eq!(*chosen.borrow(), ["two"]);
    assert!(!rig.has(panel_key(MENU)));
}

#[test]
fn nests_inline_by_default_inside_the_one_scrolling_list() {
    let mut rig = rig_with(|| PopoverMenu::new(MENU, "m", trigger).sections(|| vec![PopoverMenuSection::new("g").item(PopoverMenuItem::new("g", "Group").children(vec![PopoverMenuItem::new("x", "Member")]))]));
    rig.click(trigger_key(MENU));
    rig.settle(30);
    rig.click(row_key(MENU, "g"));
    rig.settle(30);
    let (panel, nested) = (rig.rect(panel_key(MENU)), rig.rect(row_key(MENU, "x")));
    assert!(nested.bottom() <= panel.bottom() && nested.x < panel.right(), "inside the panel: {nested:?} {panel:?}");
}

#[test]
fn search_filters_says_when_nothing_is_left_and_starts_clean_on_reopening() {
    let mut rig = rig_with(|| PopoverMenu::new(MENU, "m", trigger).search("Search").empty_label("Nothing here").sections(|| vec![PopoverMenuSection::new("s").item(PopoverMenuItem::new("a", "Alpha")).item(PopoverMenuItem::new("b", "Beta"))]));
    rig.click(trigger_key(MENU));
    rig.settle(30);
    assert!(rig.has(row_key(MENU, "a")));
    let panel = rig.rect(panel_key(MENU));
    // The field is at the panel's head, since it opened below.
    rig.click_at(panel.x + 60.0, panel.y + 16.0);
    rig.type_text("bet");
    rig.settle(10);
    assert!(!rig.has(row_key(MENU, "a")));
    assert!(rig.has(row_key(MENU, "b")));
    rig.type_text("zzz");
    assert!(rig.says("Nothing here"));
    rig.click(trigger_key(MENU));
    rig.settle(20);
    rig.click(trigger_key(MENU));
    rig.settle(30);
    assert!(rig.has(row_key(MENU, "a")), "a reopened menu starts clean");
}

#[test]
fn the_search_field_moves_to_the_far_edge_when_the_panel_opens_above() {
    let mut rig = Rig::new().with_view(|_| col().size(1100.0, 800.0).justify_end().p(40.0).child(PopoverMenu::new(MENU, "m", trigger).above().search("Search").sections(|| vec![PopoverMenuSection::new("s").item(PopoverMenuItem::new("a", "Alpha"))]).build()));
    rig.click(trigger_key(MENU));
    rig.settle(30);
    let (panel, row) = (rig.rect(panel_key(MENU)), rig.rect(row_key(MENU, "a")));
    assert!(row.y - panel.y < 12.0, "the rows lead, and the field sits next to the trigger: {row:?} {panel:?}");
}

#[test]
fn a_disabled_row_does_not_fire() {
    let fired = Rc::new(Cell::new(0));
    let mark = fired.clone();
    let mut rig = rig_with(move || {
        let mark = mark.clone();
        PopoverMenu::new(MENU, "m", trigger).sections(move || {
            let mark = mark.clone();
            vec![PopoverMenuSection::new("s").item(PopoverMenuItem::new("a", "Alpha").disabled(true).title("why not").on(move |_| mark.set(mark.get() + 1)))]
        })
    });
    rig.click(trigger_key(MENU));
    rig.settle(30);
    rig.click(row_key(MENU, "a"));
    assert_eq!(fired.get(), 0);
    assert!(rig.has(panel_key(MENU)), "a press on a held row leaves the menu up");
}

#[test]
fn a_disabled_menu_does_not_open() {
    let mut held = rig_with(|| PopoverMenu::new(MENU, "m", trigger).disabled(true).sections(|| vec![PopoverMenuSection::new("s").item(PopoverMenuItem::new("a", "Alpha"))]));
    held.click(trigger_key(MENU));
    held.settle(20);
    assert!(!held.has(panel_key(MENU)));
}

#[test]
fn asks_for_its_list_once_per_opening_and_reports_where_the_panel_is() {
    let opened = Rc::new(Cell::new(0));
    let rects: Rc<RefCell<Vec<bool>>> = Rc::default();
    let (open_mark, rect_sink) = (opened.clone(), rects.clone());
    let mut rig = rig_with(move || {
        let (open_mark, rect_sink) = (open_mark.clone(), rect_sink.clone());
        PopoverMenu::new(MENU, "m", trigger).on_open(move |_| open_mark.set(open_mark.get() + 1)).on_panel_rect(move |_, rect| rect_sink.borrow_mut().push(rect.is_some())).sections(|| vec![PopoverMenuSection::new("s").item(PopoverMenuItem::new("a", "Alpha"))])
    });
    rig.click(trigger_key(MENU));
    rig.settle(30);
    assert_eq!(opened.get(), 1);
    assert_eq!(rects.borrow().last(), Some(&true));
    rig.click(trigger_key(MENU));
    rig.settle(30);
    assert_eq!(opened.get(), 1, "closing is no opening");
    assert_eq!(rects.borrow().last(), Some(&false));
    rig.press(Key::Escape);
    rig.click(trigger_key(MENU));
    rig.settle(30);
    assert_eq!(opened.get(), 2);
    rig.press(Key::Escape);
    rig.settle(20);
    assert!(!rig.has(panel_key(MENU)), "Escape closes it");
}
