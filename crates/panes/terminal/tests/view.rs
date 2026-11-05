//! The tab strip and its menus, driven the way a person drives them, with nothing behind them but the pane's own state.
//! `TerminalTabBar.test.tsx` and `PageTabs.test.tsx`, as far as they are about what a click does.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use fluent::el::{hash_key, rect_of, render, set_theme, set_window_size, theme};
use fluent::ui::{Input, Key, KeyPress, NoClipboard, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::*;
use mewrk_client::{i18n, App, StartOptions};
use mewrk_pane_terminal::*;
use mewrk_ui_common::popover_menu as popover;

const WIN: (f32, f32) = (800.0, 400.0);
static SERIAL: Mutex<()> = Mutex::new(());

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: NoClipboard,
    _serial: MutexGuard<'static, ()>,
}

fn start() -> (Rig, App) {
    let serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("mewrk-terminal-view-{}-{n:x}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    set_theme(Theme::dark());
    set_window_size(WIN.0, WIN.1);
    let app = App::start(StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: dir, resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
    i18n::set_english(false);
    (Rig { scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: NoClipboard, _serial: serial }, app)
}

fn frame(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) {
    app.tick();
    rig.scene.clear();
    let root = view(app);
    let mut ui = Ui::new(&mut rig.scene, &mut rig.text, theme(), &rig.input, &mut rig.mem, &mut rig.clipboard, 1.0, WIN, 0.016, 0.0);
    render(root, app, &mut ui);
    rig.input.end_frame();
}

fn settle(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) {
    for _ in 0..4 {
        frame(rig, app, view);
    }
}

fn press(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, p: (f32, f32)) {
    rig.input.pointer = Some(p);
    frame(rig, app, view);
    rig.input.pressed = true;
    rig.input.down = true;
    frame(rig, app, view);
}

fn release(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) {
    rig.input.down = false;
    rig.input.released = true;
    frame(rig, app, view);
    frame(rig, app, view);
}

fn center(key: impl std::hash::Hash) -> (f32, f32) {
    let r = rect_of(key).expect("on screen");
    (r.x + r.w * 0.5, r.y + r.h * 0.5)
}

fn click(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, key: impl std::hash::Hash) {
    let p = center(key);
    press(rig, app, view, p);
    release(rig, app, view);
}

#[derive(Default, Clone)]
struct Log {
    selected: Rc<RefCell<Vec<String>>>,
    closed: Rc<RefCell<Vec<String>>>,
    renamed: Rc<RefCell<Vec<(String, String)>>>,
    reordered: Rc<RefCell<Vec<Vec<String>>>>,
}

struct Bar {
    tabs: Vec<(&'static str, &'static str, bool)>,
    active: Option<&'static str>,
    closing: Vec<&'static str>,
}

fn bar(log: &Log, spec: &Bar, add: Vec<MenuSection>) -> E {
    let (a, b, c, d) = (log.selected.clone(), log.closed.clone(), log.renamed.clone(), log.reordered.clone());
    terminal_tab_bar(TerminalTabBarProps {
        key: "bar".into(),
        tabs: spec.tabs.iter().map(|(id, label, read_only)| TerminalTabDescriptor { id: (*id).into(), label: (*label).into(), title: None, content: None, read_only: *read_only }).collect(),
        active_id: spec.active.map(str::to_owned),
        closing_ids: spec.closing.iter().map(|c| (*c).to_owned()).collect::<HashSet<_>>(),
        on_select: Rc::new(move |_, id| a.borrow_mut().push(id.to_owned())),
        on_close: Rc::new(move |_, id| b.borrow_mut().push(id.to_owned())),
        on_rename: Rc::new(move |_, id, name| c.borrow_mut().push((id.to_owned(), name))),
        on_reorder: Some(Rc::new(move |_, ids| d.borrow_mut().push(ids))),
        add,
    })
}

fn three() -> Bar {
    Bar { tabs: vec![("t1", "zsh", false), ("t2", "bash", false), ("t3", "fish", false)], active: Some("t1"), closing: Vec::new() }
}

fn label_key<'a>(id: &'a str) -> (&'static str, &'static str, &'a str) {
    ("page-tab-label", "bar", id)
}

#[test]
fn selects_on_click_and_opens_another_terminal_from_the_plus_menu() {
    let (mut rig, mut app) = start();
    let log = Log::default();
    let chosen = Rc::new(Cell::new(0));
    let mut view = {
        let (log, chosen) = (log.clone(), chosen.clone());
        move |_: &mut App| -> E {
            let chosen = chosen.clone();
            let shells = terminal_shell_menu_items(&[TerminalShell::Zsh, TerminalShell::Bash], false, Rc::new(move |_, shell| chosen.set(if shell == TerminalShell::Bash { 2 } else { 1 })));
            col().child(row().h(32.0).child(bar(&log, &three(), vec![MenuSection { id: "shells".into(), label: None, items: shells }])))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, label_key("t2"));
    assert_eq!(log.selected.borrow().as_slice(), ["t2"]);

    let menu = ("terminal-add", "bar");
    click(&mut rig, &mut app, &mut view, popover::trigger_key(menu));
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, popover::row_key(menu, "bash"));
    assert_eq!(chosen.get(), 2);
    assert!(rect_of(popover::panel_key(menu)).is_none(), "the menu closes on a choice");

    // The keyboard reaches a choice too: the arrows walk the rows and Enter takes the one they are on.
    chosen.set(0);
    click(&mut rig, &mut app, &mut view, popover::trigger_key(menu));
    settle(&mut rig, &mut app, &mut view);
    for key in [Key::Down, Key::Down, Key::Enter] {
        rig.input.keys.push(KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false });
        frame(&mut rig, &mut app, &mut view);
        frame(&mut rig, &mut app, &mut view);
    }
    assert_eq!(chosen.get(), 2, "the second row, by the arrows");
    assert!(rect_of(popover::panel_key(menu)).is_none());
}

#[test]
fn a_machine_with_no_shells_says_so_and_a_lifecycle_operation_disables_every_row() {
    let empty = terminal_shell_menu_items(&[], false, Rc::new(|_, _| {}));
    assert_eq!(empty.len(), 1);
    assert_eq!(&*empty[0].label, "未探测到可用的 shell");
    assert!(empty[0].disabled);
    let busy = terminal_shell_menu_items(&[TerminalShell::Zsh], true, Rc::new(|_, _| {}));
    assert!(busy[0].disabled);
}

#[test]
fn several_workspaces_offer_a_flyout_of_shells_and_one_offers_its_shells_directly() {
    let (mut rig, mut app) = start();
    let workspace = |path: &str, shells: Vec<TerminalShell>| TerminalWorkspace { path: path.to_owned(), title: None, machine_icon: "Folder", shells };
    let one = new_terminal_sections(NewTerminalMenu { workspaces: vec![workspace("/w/a", vec![TerminalShell::Zsh])], selected: 1, disabled: false }, "c", Rc::new(|_| {}));
    assert_eq!(one[0].id, "shells");
    assert_eq!(one[0].label.as_deref(), Some("在 /w/a 打开"));
    assert_eq!(&*one[0].items[0].label, "zsh");

    let opened = Rc::new(Cell::new(false));
    let mut view = {
        let opened = opened.clone();
        move |_: &mut App| -> E {
            let opened = opened.clone();
            let two = new_terminal_sections(NewTerminalMenu { workspaces: vec![workspace("/w/a", vec![TerminalShell::Zsh]), workspace("/w/b", vec![TerminalShell::Bash])], selected: 1, disabled: false }, "c", Rc::new(move |_| opened.set(true)));
            let log = Log::default();
            col().child(row().h(32.0).child(bar(&log, &Bar { tabs: Vec::new(), active: None, closing: Vec::new() }, two)))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    let menu = ("terminal-add", "bar");
    click(&mut rig, &mut app, &mut view, popover::trigger_key(menu));
    settle(&mut rig, &mut app, &mut view);
    let row_key = popover::row_key(menu, "workspace-2");
    click(&mut rig, &mut app, &mut view, row_key);
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, popover::row_key(menu, "bash"));
    assert!(opened.get(), "the pane opens with the new terminal");
    let layout = store(&mut app).tabs.layout("c");
    assert_eq!(layout.tabs[0].launch, Some(TerminalLaunchChoice { workspace: Some(2), shell: Some(TerminalShell::Bash) }), "the workspace's number and the shell picked beside it");
}

#[test]
fn closes_from_its_own_tab_and_spends_the_control_while_it_is_going() {
    let (mut rig, mut app) = start();
    let log = Log::default();
    let closing = Rc::new(Cell::new(false));
    let mut view = {
        let (log, closing) = (log.clone(), closing.clone());
        move |_: &mut App| -> E {
            let mut spec = three();
            if closing.get() {
                spec.closing = vec!["t1"];
            }
            col().child(row().h(32.0).child(bar(&log, &spec, Vec::new())))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("page-tab-close", "bar", "t1"));
    assert_eq!(log.closed.borrow().as_slice(), ["t1"]);
    assert!(log.selected.borrow().is_empty(), "closing is not selecting");
    closing.set(true);
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("page-tab-close", "bar", "t1"));
    assert_eq!(log.closed.borrow().len(), 1, "spent");
}

#[test]
fn the_middle_button_closes_a_tab_unless_its_close_is_spent() {
    let (mut rig, mut app) = start();
    let log = Log::default();
    let mut view = {
        let log = log.clone();
        move |_: &mut App| -> E { col().child(row().h(32.0).child(bar(&log, &Bar { closing: vec!["t3"], ..three() }, Vec::new()))) }
    };
    settle(&mut rig, &mut app, &mut view);
    for tab in ["t2", "t3"] {
        rig.input.pointer = Some(center(label_key(tab)));
        frame(&mut rig, &mut app, &mut view);
        rig.input.middle_pressed = true;
        frame(&mut rig, &mut app, &mut view);
        frame(&mut rig, &mut app, &mut view);
    }
    assert_eq!(log.closed.borrow().as_slice(), ["t2"]);
}

#[test]
fn moves_between_tabs_with_the_arrows_wrapping_and_hands_over_the_order_with_ctrl_shift() {
    let (mut rig, mut app) = start();
    let log = Log::default();
    let mut view = {
        let log = log.clone();
        move |_: &mut App| -> E { col().child(row().h(32.0).child(bar(&log, &three(), Vec::new()))) }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, label_key("t1"));
    log.selected.borrow_mut().clear();
    let key = |rig: &mut Rig, k: Key, ctrl: bool, shift: bool| rig.input.keys.push(KeyPress { key: k, shift, cmd: false, alt: false, ctrl });
    key(&mut rig, Key::Left, false, false);
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(log.selected.borrow().as_slice(), ["t3"], "wraps at the ends");
    frame(&mut rig, &mut app, &mut view);
    key(&mut rig, Key::Right, false, false);
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(log.selected.borrow().as_slice(), ["t3", "t1"], "focus followed it there");
    frame(&mut rig, &mut app, &mut view);
    key(&mut rig, Key::Right, true, true);
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(log.reordered.borrow().as_slice(), [vec!["t2".to_owned(), "t1".to_owned(), "t3".to_owned()]]);
}

#[test]
fn a_drag_reorders_marks_the_landing_spot_and_swallows_the_click() {
    let (mut rig, mut app) = start();
    let log = Log::default();
    let mut view = {
        let log = log.clone();
        move |_: &mut App| -> E { col().child(row().h(32.0).child(bar(&log, &three(), Vec::new()))) }
    };
    settle(&mut rig, &mut app, &mut view);
    let from = center(label_key("t1"));
    let r = rect_of(hash_key(("page-tab", "bar", "t3"))).expect("the last tab");
    let to = (r.right() - 2.0, from.1);
    press(&mut rig, &mut app, &mut view, from);
    // Under the threshold it is still a click.
    rig.input.pointer = Some((from.0 + 2.0, from.1));
    frame(&mut rig, &mut app, &mut view);
    rig.input.pointer = Some(to);
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    release(&mut rig, &mut app, &mut view);
    assert_eq!(log.reordered.borrow().as_slice(), [vec!["t2".to_owned(), "t3".to_owned(), "t1".to_owned()]]);
    assert!(log.selected.borrow().is_empty(), "a drag is not a click");

    // A press that stays put is.
    let p = center(label_key("t2"));
    press(&mut rig, &mut app, &mut view, p);
    rig.input.pointer = Some((p.0 + 1.0, p.1));
    frame(&mut rig, &mut app, &mut view);
    release(&mut rig, &mut app, &mut view);
    assert_eq!(log.selected.borrow().as_slice(), ["t2"]);
}

#[test]
fn leads_with_the_read_only_page_which_keeps_its_place_and_takes_no_name() {
    let (mut rig, mut app) = start();
    let log = Log::default();
    let mut view = {
        let log = log.clone();
        move |_: &mut App| -> E {
            let spec = Bar { tabs: vec![(READ_ONLY_TERMINAL_TAB_ID, "bash:/w", true), ("t1", "zsh", false), ("t2", "fish", false)], active: Some("t1"), closing: Vec::new() };
            col().child(row().h(32.0).child(bar(&log, &spec, Vec::new())))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    // It does not drag.
    let from = center(label_key(READ_ONLY_TERMINAL_TAB_ID));
    let to = center(label_key("t2"));
    press(&mut rig, &mut app, &mut view, from);
    rig.input.pointer = Some(to);
    frame(&mut rig, &mut app, &mut view);
    release(&mut rig, &mut app, &mut view);
    assert!(log.reordered.borrow().is_empty());

    // Nothing lands ahead of it.
    let from = center(label_key("t2"));
    let to = center(label_key(READ_ONLY_TERMINAL_TAB_ID));
    press(&mut rig, &mut app, &mut view, from);
    rig.input.pointer = Some((to.0, to.1));
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    release(&mut rig, &mut app, &mut view);
    let order = log.reordered.borrow();
    assert_eq!(order.last().unwrap()[0], READ_ONLY_TERMINAL_TAB_ID);
    drop(order);

    // A double click on it opens no editor.
    let p = center(label_key(READ_ONLY_TERMINAL_TAB_ID));
    press(&mut rig, &mut app, &mut view, p);
    rig.input.double_click = true;
    frame(&mut rig, &mut app, &mut view);
    release(&mut rig, &mut app, &mut view);
    assert!(log.renamed.borrow().is_empty());
    assert!(rect_of(label_key(READ_ONLY_TERMINAL_TAB_ID)).is_some(), "still its label, not an editor");
}

fn double_click(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, key: impl std::hash::Hash + Clone) {
    let p = center(key);
    rig.input.pointer = Some(p);
    frame(rig, app, view);
    rig.input.pressed = true;
    rig.input.down = true;
    rig.input.double_click = true;
    frame(rig, app, view);
    rig.input.down = false;
    rig.input.released = true;
    frame(rig, app, view);
    frame(rig, app, view);
}

#[test]
fn renaming_commits_on_enter_and_keeps_the_old_name_on_escape() {
    let (mut rig, mut app) = start();
    let log = Log::default();
    let mut view = {
        let log = log.clone();
        move |_: &mut App| -> E { col().child(row().h(32.0).child(bar(&log, &three(), Vec::new()))) }
    };
    settle(&mut rig, &mut app, &mut view);
    double_click(&mut rig, &mut app, &mut view, label_key("t2"));
    settle(&mut rig, &mut app, &mut view);
    // The field holds what the tab showed, and has the focus: typing replaces nothing until it is selected, so a test
    // only needs it to end up with what was typed after.
    rig.input.text = "-x".into();
    frame(&mut rig, &mut app, &mut view);
    rig.input.keys.push(KeyPress { key: Key::Enter, shift: false, cmd: false, alt: false, ctrl: false });
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(log.renamed.borrow().as_slice(), [("t2".to_owned(), "bash-x".to_owned())]);

    double_click(&mut rig, &mut app, &mut view, label_key("t1"));
    settle(&mut rig, &mut app, &mut view);
    rig.input.text = "nope".into();
    frame(&mut rig, &mut app, &mut view);
    rig.input.keys.push(KeyPress { key: Key::Escape, shift: false, cmd: false, alt: false, ctrl: false });
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(log.renamed.borrow().len(), 1, "Escape keeps the old name");
    assert!(rect_of(label_key("t1")).is_some(), "the tab is back");
}

#[test]
fn the_overflow_menu_appears_once_a_tab_is_out_of_reach_and_reaches_it() {
    let (mut rig, mut app) = start();
    let log = Log::default();
    let narrow = Rc::new(Cell::new(false));
    let mut view = {
        let (log, narrow) = (log.clone(), narrow.clone());
        move |_: &mut App| -> E {
            let w = if narrow.get() { 180.0 } else { 700.0 };
            let spec = Bar { tabs: vec![("t1", "first terminal", false), ("t2", "second terminal", false), ("t3", "third terminal", false)], active: Some("t1"), closing: Vec::new() };
            col().child(row().w(w).h(32.0).child(bar(&log, &spec, Vec::new())))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    let trigger = hash_key(("page-tabs-trigger", hash_key(("page-tabs", "bar"))));
    assert!(rect_of(trigger).is_none(), "stays away while every tab fits");
    narrow.set(true);
    settle(&mut rig, &mut app, &mut view);
    assert!(rect_of(trigger).is_some(), "appears once a tab is out of reach");
    assert!(rect_of(label_key("t3")).is_none());
    click(&mut rig, &mut app, &mut view, trigger);
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, hash_key(("page-tab-row", "bar", "t3")));
    assert_eq!(log.selected.borrow().as_slice(), ["t3"]);
}
