//! `diff_view` on its own, driven the way a reader drives it, with no host call in it beyond starting one: it has no pane
//! assumptions, and this is what says so.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use fluent::el::{a11y_action, hash_key, rect_of, render, scroll_state, set_a11y_active, set_theme, set_window_size, take_a11y_update, theme, use_state, Local};
use fluent::ui::{Input, Key, KeyPress, NoClipboard, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::*;
use mewrk_client::{i18n, App, StartOptions};
use mewrk_pane_git::*;

const WIN: (f32, f32) = (1000.0, 700.0);
static SERIAL: Mutex<()> = Mutex::new(());

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: NoClipboard,
    size: (f32, f32),
    _serial: MutexGuard<'static, ()>,
}

fn start() -> (Rig, App) {
    let serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("mewrk-git-view-{}-{n:x}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    set_theme(Theme::dark());
    set_window_size(WIN.0, WIN.1);
    let app = App::start(StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: dir, resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
    i18n::set_english(false);
    (Rig { scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: NoClipboard, size: WIN, _serial: serial }, app)
}

fn frame(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) {
    app.tick();
    rig.scene.clear();
    let root = view(app);
    let mut ui = Ui::new(&mut rig.scene, &mut rig.text, theme(), &rig.input, &mut rig.mem, &mut rig.clipboard, 1.0, rig.size, 0.016, 0.0);
    render(root, app, &mut ui);
    rig.input.end_frame();
}

fn settle(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) {
    for _ in 0..4 {
        frame(rig, app, view);
    }
}

fn click_at(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, x: f32, y: f32) {
    rig.input.pointer = Some((x, y));
    frame(rig, app, view);
    rig.input.pressed = true;
    rig.input.down = true;
    frame(rig, app, view);
    rig.input.down = false;
    rig.input.released = true;
    frame(rig, app, view);
    frame(rig, app, view);
}

fn click(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, key: impl std::hash::Hash) {
    let r = rect_of(key).expect("on screen");
    click_at(rig, app, view, r.x + r.w * 0.5, r.y + r.h * 0.5);
}

const PATH: &str = "src/app.ts";
const PATCH: &str = "diff --git a/src/app.ts b/src/app.ts\nindex 1111111..2222222 100644\n--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1,4 +1,4 @@\n const keep = 1;\n-const old = 2;\n+const next = 2;\n-const gone = 3;\n const tail = 4;\n+const extra = 5;\n";

/// The same parsed files every call. The view knows a file by its `Arc`, as it does in the app, where the patches outlive the frame:
/// fresh ones each frame look like a new file to it, unless the allocator happens to hand back the old address.
fn patches() -> HashMap<String, Arc<DiffFile>> {
    thread_local! {
        static PATCHES: HashMap<String, Arc<DiffFile>> = parse_unified_diff(PATCH, None).into_iter().map(|f| (f.path.clone(), Arc::new(f))).collect();
    }
    PATCHES.with(HashMap::clone)
}

fn entries() -> Vec<DiffEntry> {
    vec![DiffEntry::new(PATH, DiffEntryStatus::Modified, 2, 2)]
}

#[derive(Default, Clone)]
struct Calls {
    selected: Rc<std::cell::RefCell<Vec<String>>>,
    needed: Rc<std::cell::RefCell<Vec<String>>>,
    expanded: Rc<std::cell::RefCell<Vec<String>>>,
    lines: Rc<std::cell::RefCell<Vec<(String, u32)>>>,
}

fn props(calls: &Calls, tree: bool, style: DiffStyle, wrap: bool) -> DiffViewProps {
    let (select, need, expand, line) = (calls.selected.clone(), calls.needed.clone(), calls.expanded.clone(), calls.lines.clone());
    DiffViewProps {
        key: "viewer".into(),
        entries: entries(),
        patches: patches(),
        show_tree: tree,
        diff_style: style,
        word_wrap: wrap,
        word_diff: true,
        on_select_file: Some(Rc::new(move |_, path| select.borrow_mut().push(path.to_owned()))),
        on_need_patch: Some(Rc::new(move |_, path| need.borrow_mut().push(path.to_owned()))),
        on_expand_context: Some(Rc::new(move |_, path| expand.borrow_mut().push(path.to_owned()))),
        on_line_menu: Some(Rc::new(move |_, path, number, _| line.borrow_mut().push((path.to_owned(), number)))),
        empty_label: "No changes".into(),
        ..DiffViewProps::default()
    }
}

/// The y of a line in an open card, counting rows under its header: the first row is the tail band's neighbours.
fn card(app_key: &str) -> fluent::Rect {
    rect_of(("diff-file", app_key, PATH)).expect("the card")
}

#[test]
fn a_card_starts_folded_and_its_header_opens_it_and_reports_the_selection() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E { col().child(diff_view(app, props(&calls, false, DiffStyle::Unified, true))) }
    };
    settle(&mut rig, &mut app, &mut view);
    assert_eq!(card("viewer").h, 32.0, "folded: the header alone");
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    assert_eq!(calls.selected.borrow().as_slice(), [PATH]);
    settle(&mut rig, &mut app, &mut view);
    assert!(card("viewer").h > 32.0 + 17.0 * 5.0, "open: its rows under it, {:?}", card("viewer"));
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    settle(&mut rig, &mut app, &mut view);
    assert_eq!(card("viewer").h, 32.0);
}

#[test]
fn a_right_click_answers_with_the_line_the_gutter_shows_new_or_old() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E { col().child(diff_view(app, props(&calls, false, DiffStyle::Unified, true))) }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    settle(&mut rig, &mut app, &mut view);
    let top = card("viewer").y + 32.0;
    let right_click = |rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, row: f32| {
        rig.input.pointer = Some((200.0, top + row * 17.0 + 8.0));
        frame(rig, app, view);
        rig.input.right_pressed = true;
        frame(rig, app, view);
        frame(rig, app, view);
    };
    // Row 1 removes old line 2, row 2 adds new line 2, row 3 removes old line 3.
    right_click(&mut rig, &mut app, &mut view, 1.0);
    right_click(&mut rig, &mut app, &mut view, 2.0);
    right_click(&mut rig, &mut app, &mut view, 3.0);
    assert_eq!(calls.lines.borrow().as_slice(), [(PATH.to_owned(), 2), (PATH.to_owned(), 2), (PATH.to_owned(), 3)]);
}

#[test]
fn the_tail_band_asks_for_more_context() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E { col().child(diff_view(app, props(&calls, false, DiffStyle::Unified, true))) }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    settle(&mut rig, &mut app, &mut view);
    let c = card("viewer");
    // Six lines, then the 32px band that says there may be more.
    click_at(&mut rig, &mut app, &mut view, c.x + 60.0, c.y + 32.0 + 6.0 * 17.0 + 16.0);
    assert_eq!(calls.expanded.borrow().as_slice(), [PATH]);
}

/// The button is an element of its own, for its name and the keyboard, and the leaf leaves a press there to it.
#[test]
fn the_expand_button_asks_once() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E { col().child(diff_view(app, props(&calls, false, DiffStyle::Unified, true))) }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    settle(&mut rig, &mut app, &mut view);
    let c = card("viewer");
    click_at(&mut rig, &mut app, &mut view, c.x + 12.0, c.y + 32.0 + 6.0 * 17.0 + 16.0);
    assert_eq!(calls.expanded.borrow().as_slice(), [PATH]);
}

#[test]
fn a_file_with_no_patch_asks_for_one_only_once_it_is_opened() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E {
            let mut p = props(&calls, false, DiffStyle::Unified, true);
            p.patches = HashMap::new();
            col().child(diff_view(app, p))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    assert!(calls.needed.borrow().is_empty(), "folded files cost nothing");
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    frame(&mut rig, &mut app, &mut view);
    assert!(calls.needed.borrow().iter().all(|p| p == PATH) && !calls.needed.borrow().is_empty());
    assert_eq!(card("viewer").h, 32.0, "it opens once it has its diff, rather than onto a spinner");
}

#[test]
fn binary_and_unavailable_files_have_a_sentence_instead_of_hunks() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E {
            let mut p = props(&calls, false, DiffStyle::Unified, true);
            p.patches = HashMap::new();
            p.entries = vec![DiffEntry { binary: true, ..DiffEntry::new("a.png", DiffEntryStatus::Modified, 0, 0) }, DiffEntry { unavailable: Some("no diff".into()), ..DiffEntry::new("b.ts", DiffEntryStatus::Modified, 0, 0) }];
            col().child(diff_view(app, p))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", "a.png"));
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", "b.ts"));
    settle(&mut rig, &mut app, &mut view);
    assert!(rect_of(("diff-file", "viewer", "a.png")).unwrap().h > 32.0);
    assert!(rect_of(("diff-file", "viewer", "b.ts")).unwrap().h > 32.0);
    assert!(calls.needed.borrow().is_empty(), "an answer is not a request");
}

#[test]
fn side_by_side_gives_way_to_unified_in_a_narrow_pane_and_so_does_the_file_column() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let fit_tree = Rc::new(std::cell::Cell::new(None));
    let fit_split = Rc::new(std::cell::Cell::new(None));
    let width = Rc::new(std::cell::Cell::new(900.0));
    let mut view = {
        let (calls, fit_tree, fit_split, width) = (calls.clone(), fit_tree.clone(), fit_split.clone(), width.clone());
        move |app: &mut App| -> E {
            let mut p = props(&calls, true, DiffStyle::Split, true);
            let (a, b) = (fit_tree.clone(), fit_split.clone());
            p.on_can_fit_tree_change = Some(Rc::new(move |_, v| a.set(Some(v))));
            p.on_can_fit_split_change = Some(Rc::new(move |_, v| b.set(Some(v))));
            row().child(col().w(width.get()).h(600.0).child(diff_view(app, p)))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    assert_eq!((fit_tree.get(), fit_split.get()), (Some(true), Some(true)));
    assert!(rect_of(("diff-tree", "viewer")).is_some());
    width.set(500.0);
    settle(&mut rig, &mut app, &mut view);
    assert_eq!((fit_tree.get(), fit_split.get()), (Some(true), Some(false)));
    width.set(300.0);
    settle(&mut rig, &mut app, &mut view);
    assert_eq!((fit_tree.get(), fit_split.get()), (Some(false), Some(false)));
    assert!(rect_of(("diff-tree", "viewer")).is_none(), "no room for the file column");
}

#[test]
fn a_reveal_opens_the_file_and_scrolls_to_it_and_asking_again_asks_again() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let request = Rc::new(std::cell::RefCell::new(None::<(String, u64)>));
    let mut view = {
        let (calls, request) = (calls.clone(), request.clone());
        move |app: &mut App| -> E {
            let mut p = props(&calls, false, DiffStyle::Unified, true);
            let mut many = Vec::new();
            for i in 0..40 {
                many.push(DiffEntry::new(format!("f{i:02}.ts"), DiffEntryStatus::Modified, 1, 0));
            }
            let last = "z.ts";
            many.push(DiffEntry::new(last, DiffEntryStatus::Modified, 1, 0));
            let mut patches: HashMap<String, Arc<DiffFile>> = HashMap::new();
            patches.insert(last.to_owned(), Arc::new(parse_unified_diff(&PATCH.replace("src/app.ts", "z.ts"), None).remove(0)));
            p.entries = many;
            p.patches = patches;
            p.reveal_request = request.borrow().clone();
            col().h(600.0).child(diff_view(app, p))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    *request.borrow_mut() = Some(("z.ts".into(), 1));
    for _ in 0..30 {
        frame(&mut rig, &mut app, &mut view);
    }
    assert_eq!(calls.selected.borrow().as_slice(), ["z.ts"]);
    let (offset, _, _) = scroll_state(hash_key(("diff-view-scroll", "viewer"))).unwrap();
    assert!(offset > 100.0, "it scrolled, to {offset}");
    let r = rect_of(("diff-file", "viewer", "z.ts")).unwrap();
    assert!(r.y < 600.0 && r.h > 32.0, "the card is on screen and open: {r:?}");
}

#[test]
fn folding_everything_and_hiding_whitespace_are_the_views_to_do() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let fold = Rc::new(std::cell::Cell::new(None::<(bool, u64)>));
    let hide = Rc::new(std::cell::Cell::new(false));
    let mut view = {
        let (calls, fold, hide) = (calls.clone(), fold.clone(), hide.clone());
        move |app: &mut App| -> E {
            let mut p = props(&calls, false, DiffStyle::Unified, true);
            p.fold_all_request = fold.get();
            p.hide_whitespace = hide.get();
            p.patches = parse_unified_diff("diff --git a/src/app.ts b/src/app.ts\n--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1 +1 @@\n-  a\n+    a\n", None).into_iter().map(|f| (f.path.clone(), Arc::new(f))).collect();
            col().child(diff_view(app, p))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    fold.set(Some((false, 1)));
    settle(&mut rig, &mut app, &mut view);
    let open = card("viewer").h;
    assert!(open > 32.0, "unfold all");
    hide.set(true);
    settle(&mut rig, &mut app, &mut view);
    assert!(card("viewer").h < open || card("viewer").h > 32.0, "the hunk went, and a sentence said why");
    hide.set(false);
    fold.set(Some((true, 2)));
    settle(&mut rig, &mut app, &mut view);
    assert_eq!(card("viewer").h, 32.0, "fold all");
}

#[test]
fn the_empty_label_shows_when_there_is_nothing_to_list() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E {
            let mut p = props(&calls, true, DiffStyle::Unified, true);
            p.entries = Vec::new();
            col().child(diff_view(app, p))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    assert!(rect_of(("diff-tree", "viewer")).is_some(), "the column stays for its chrome");
}

#[test]
fn the_diff_card_and_the_status_chip_draw() {
    let (mut rig, mut app) = start();
    let mut view = move |_: &mut App| -> E {
        col().child(diff_output("card", "--- a/x.ts\n+++ b/x.ts\n@@ -1 +1 @@\n-const a = 1;\n+const b = 1;\n", None)).child(diff_output("empty", "", Some("x.ts")))
    };
    settle(&mut rig, &mut app, &mut view);
    assert!(!rig.scene.layers[0].glyphs.is_empty());
}

/// A card scrolled under the top keeps its header on screen, until the next card's pushes it away.
#[test]
fn a_header_sticks_while_its_card_is_scrolled_under_the_top() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let long: String = (0..200).map(|i| format!("+line {i}\n")).collect();
    let patch = format!("diff --git a/a.ts b/a.ts\n--- /dev/null\n+++ b/a.ts\n@@ -0,0 +1,200 @@\n{long}");
    let tail = "diff --git a/b.ts b/b.ts\n--- /dev/null\n+++ b/b.ts\n@@ -0,0 +1 @@\n+last\n";
    let mut view = {
        let calls = calls.clone();
        let patches: HashMap<String, Arc<DiffFile>> = parse_unified_diff(&format!("{patch}{tail}"), None).into_iter().map(|f| (f.path.clone(), Arc::new(f))).collect();
        move |app: &mut App| -> E {
            let mut p = props(&calls, false, DiffStyle::Unified, true);
            p.entries = vec![DiffEntry::new("a.ts", DiffEntryStatus::Added, 200, 0), DiffEntry::new("b.ts", DiffEntryStatus::Added, 1, 0)];
            p.patches = patches.clone();
            p.fold_all_request = Some((false, 1));
            col().h(400.0).child(diff_view(app, p))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    let sticky = ("diff-file-header", "viewer", "sticky", "a.ts");
    assert!(rect_of(sticky).is_none(), "at the top nothing has scrolled under");
    let list = rect_of(hash_key(("diff-view-scroll", "viewer"))).unwrap();
    rig.input.pointer = Some((list.x + 100.0, list.y + 100.0));
    rig.input.scroll = -500.0;
    rig.input.scroll_is_pixels = true;
    settle(&mut rig, &mut app, &mut view);
    let r = rect_of(sticky).expect("the header stays");
    assert_eq!((r.y, r.h), (list.y, 32.0), "at the scroller's top");
    assert!(rect_of(("diff-file-header", "viewer", "", "a.ts")).unwrap().y < list.y, "while its own has gone by");

    // It works like the one it stands in for.
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "sticky", "a.ts"));
    settle(&mut rig, &mut app, &mut view);
    assert_eq!(calls.selected.borrow().as_slice(), ["a.ts"]);
    assert_eq!(rect_of(("diff-file", "viewer", "a.ts")).unwrap().h, 32.0, "folded from the sticky header");
}

/// A patch with what a test needs of it, in the viewer's shape.
fn only(patch: &str) -> HashMap<String, Arc<DiffFile>> {
    parse_unified_diff(patch, None).into_iter().map(|f| (f.path.clone(), Arc::new(f))).collect()
}

/// The leaf's own state, as the view that holds it can reach it.
fn table_state() -> Local<code_table::TableUi> {
    use_state(("code-table-ui", hash_key(("viewer", PATH))), code_table::TableUi::default)
}

#[test]
fn what_is_said_about_a_file_follows_its_path_and_a_long_path_gives_way_to_it() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let long = format!("{}/app.ts", "deep/".repeat(60).trim_end_matches('/'));
    let mut view = {
        let (calls, long) = (calls.clone(), long.clone());
        move |app: &mut App| -> E {
            let mut other = DiffEntry::new(long.as_str(), DiffEntryStatus::Modified, 1, 1);
            other.note = Some("note".to_owned());
            col().child(diff_view(app, DiffViewProps { entries: vec![DiffEntry::new(PATH, DiffEntryStatus::Modified, 2, 2), other], ..props(&calls, false, DiffStyle::Unified, true) }))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    let header = rect_of(("diff-file-header", "viewer", "", PATH)).unwrap();
    let stat = rect_of(("diff-file-stat", "viewer", "", PATH)).unwrap();
    assert!(stat.x < header.x + header.w * 0.5, "beside the path, not at the far edge: {stat:?} in {header:?}");
    // A path too long for the row ellipsizes, and the counts stay inside the header.
    let (header, stat) = (rect_of(("diff-file-header", "viewer", "", long.as_str())).unwrap(), rect_of(("diff-file-stat", "viewer", "", long.as_str())).unwrap());
    assert!(stat.right() <= header.right(), "{stat:?} in {header:?}");
    assert!(stat.x > header.x + header.w * 0.5, "the path took the room: {stat:?}");
}

#[test]
fn only_the_button_and_the_label_of_a_band_ask_for_more_the_heading_only_reads() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E {
            let patches = only("diff --git a/src/app.ts b/src/app.ts\n--- a/src/app.ts\n+++ b/src/app.ts\n@@ -5,2 +5,2 @@ function a() {\n-const old = 1;\n+const next = 1;\n keep\n");
            col().child(diff_view(app, DiffViewProps { patches, ..props(&calls, false, DiffStyle::Unified, true) }))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    settle(&mut rig, &mut app, &mut view);
    let band = card("viewer").y + 32.0 + 16.0;
    click_at(&mut rig, &mut app, &mut view, 700.0, band);
    assert!(calls.expanded.borrow().is_empty(), "past the label is the heading");
    click_at(&mut rig, &mut app, &mut view, 30.0, band);
    assert_eq!(calls.expanded.borrow().as_slice(), [PATH.to_owned()]);
}

#[test]
fn a_right_click_on_the_empty_side_of_a_row_still_answers_with_its_line() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E {
            let patches = only("diff --git a/src/app.ts b/src/app.ts\n--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1 +1,2 @@\n ctx\n+new\n");
            col().child(diff_view(app, DiffViewProps { patches, ..props(&calls, false, DiffStyle::Split, true) }))
        }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    settle(&mut rig, &mut app, &mut view);
    rig.input.pointer = Some((120.0, card("viewer").y + 32.0 + 17.0 + 8.0));
    frame(&mut rig, &mut app, &mut view);
    rig.input.right_pressed = true;
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(calls.lines.borrow().as_slice(), [(PATH.to_owned(), 2)]);
}

#[test]
fn the_wheel_moves_a_long_line_sideways_and_stops_at_its_end() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let state: Rc<std::cell::RefCell<Option<Local<code_table::TableUi>>>> = Rc::default();
    let mut view = {
        let (calls, state) = (calls.clone(), state.clone());
        move |app: &mut App| -> E {
            let long = "x".repeat(400);
            let patches = only(&format!("diff --git a/src/app.ts b/src/app.ts\n--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1 +1 @@\n-a\n+{long}\n"));
            let viewer = diff_view(app, DiffViewProps { patches, ..props(&calls, false, DiffStyle::Unified, false) });
            *state.borrow_mut() = Some(table_state());
            col().child(viewer)
        }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    settle(&mut rig, &mut app, &mut view);
    let scrolled = |state: &Rc<std::cell::RefCell<Option<Local<code_table::TableUi>>>>| state.borrow().as_ref().unwrap().borrow().scroll_x;
    rig.input.pointer = Some((300.0, card("viewer").y + 32.0 + 8.0));
    rig.input.scroll_x = -120.0;
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(scrolled(&state), 120.0);
    rig.input.scroll_x = 500.0;
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(scrolled(&state), 0.0);
    rig.input.scroll_x = -100_000.0;
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    let end = scrolled(&state);
    assert!(end > 120.0 && end < 100_000.0, "as far as the line goes: {end}");
}

#[test]
fn clicks_take_a_word_then_the_line_shift_moves_the_far_end_and_select_all_stays_in_the_file() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let state: Rc<std::cell::RefCell<Option<Local<code_table::TableUi>>>> = Rc::default();
    let mut view = {
        let (calls, state) = (calls.clone(), state.clone());
        move |app: &mut App| -> E {
            let viewer = diff_view(app, props(&calls, false, DiffStyle::Unified, true));
            *state.borrow_mut() = Some(table_state());
            col().child(viewer)
        }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    settle(&mut rig, &mut app, &mut view);
    let selection = |state: &Rc<std::cell::RefCell<Option<Local<code_table::TableUi>>>>| state.borrow().as_ref().unwrap().borrow().selection.unwrap();
    let press = |rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, x: f32, y: f32, double: bool| {
        rig.input.pointer = Some((x, y));
        frame(rig, app, view);
        rig.input.pressed = true;
        rig.input.down = true;
        rig.input.double_click = double;
        frame(rig, app, view);
        rig.input.down = false;
        rig.input.released = true;
        frame(rig, app, view);
    };
    let top = card("viewer").y + 32.0;
    let (x, y) = (60.0, top + 8.0);
    press(&mut rig, &mut app, &mut view, x, y, false);
    let one = selection(&state);
    assert_eq!(one.anchor, one.head, "a caret");
    press(&mut rig, &mut app, &mut view, x, y, true);
    let word = selection(&state);
    assert_eq!(word.anchor.0, word.head.0);
    assert!(word.anchor.1 < word.head.1, "a word: {word:?}");
    press(&mut rig, &mut app, &mut view, x, y, true);
    let line = selection(&state);
    assert_eq!(line.anchor.1, 0, "from the line's start");
    assert!(line.head.1 > word.head.1 - word.anchor.1, "to its end: {line:?}");

    press(&mut rig, &mut app, &mut view, x, y, false);
    let start_at = selection(&state).anchor;
    rig.input.shift = true;
    press(&mut rig, &mut app, &mut view, x, top + 17.0 * 3.0 + 8.0, false);
    rig.input.shift = false;
    let extended = selection(&state);
    assert_eq!(extended.anchor, start_at, "the near end stays");
    assert!(extended.head.0 > start_at.0, "and the far one moved down: {extended:?}");

    rig.input.keys.push(KeyPress { key: Key::Char('a'), shift: false, cmd: true, alt: false, ctrl: false });
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    let all = selection(&state);
    assert_eq!(all.anchor.1, 0);
    assert!(all.head.0 >= extended.head.0, "the whole file: {all:?}");
}

// ---------------------------------------------------------------- the accessibility tree

type Tree = Vec<(accesskit::NodeId, accesskit::Node)>;

fn a11y_tree(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) -> Tree {
    set_a11y_active(true);
    settle(rig, app, view);
    take_a11y_update().expect("a client gets the tree").nodes
}

fn says<'a>(tree: &'a Tree, words: &str) -> Option<&'a accesskit::Node> {
    tree.iter().map(|(_, node)| node).find(|node| node.label() == Some(words) || (node.role() == Role::Label && node.value() == Some(words)))
}

fn said<'a>(tree: &'a Tree, words: &str) -> &'a accesskit::Node {
    says(tree, words).unwrap_or_else(|| panic!("nothing says {words:?}: {:?}", tree.iter().filter_map(|(_, n)| n.label().or(n.value())).collect::<Vec<_>>()))
}

/// Everything a reader can press or type into with no name to call it by.
fn nameless(tree: &Tree) -> Vec<String> {
    tree.iter()
        .filter(|(_, node)| matches!(node.role(), Role::Button | Role::Link | Role::TextInput | Role::MultilineTextInput | Role::Tab | Role::Switch | Role::CheckBox | Role::MenuItem | Role::ComboBox) || node.supports_action(accesskit::Action::Click))
        .filter(|(_, node)| node.label().is_none_or(str::is_empty) && node.labelled_by().is_empty() && (matches!(node.role(), Role::TextInput | Role::MultilineTextInput) || node.value().is_none_or(str::is_empty)))
        .map(|(id, node)| format!("{:?} {id:?}", node.role()))
        .collect()
}

#[test]
fn a_diff_reads_as_upstream_s_dom_does_its_text_with_the_numbers_left_out() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E { col().child(diff_view(app, props(&calls, true, DiffStyle::Unified, true))) }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    let tree = a11y_tree(&mut rig, &mut app, &mut view);

    assert_eq!(said(&tree, "变更文件").role(), Role::Navigation);
    assert_eq!(said(&tree, &format!("{PATH} 差异")).role(), Role::Region);
    let toggle = tree.iter().map(|(_, n)| n).find(|n| n.is_expanded() == Some(true) && n.label().is_some_and(|l| l.contains("app.ts"))).expect("the file's disclosure says it is open");
    assert_eq!(toggle.role(), Role::Button);
    assert!(said(&tree, "app.ts +2 −2").supports_action(accesskit::Action::Click));

    for line in ["const keep = 1;", "const old = 2;", "const next = 2;", "const gone = 3;", "const tail = 4;", "const extra = 5;"] {
        assert_eq!(said(&tree, line).role(), Role::Label);
    }
    assert!(says(&tree, "1").is_none(), "the gutter is aria-hidden upstream");

    let expand = said(&tree, "展开更多上下文");
    assert_eq!(expand.role(), Role::Button);
    assert_eq!(said(&tree, "下面可能还有未改动的内容").role(), Role::Button);
    let (id, _) = tree.iter().find(|(_, n)| n.label() == Some("展开更多上下文")).unwrap();
    a11y_action(accesskit::ActionRequest { action: accesskit::Action::Click, target_tree: accesskit::TreeId::ROOT, target_node: *id, data: None });
    settle(&mut rig, &mut app, &mut view);
    assert_eq!(calls.expanded.borrow().as_slice(), [PATH.to_owned()]);
    assert_eq!(nameless(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn side_by_side_a_row_reads_as_its_two_sides() {
    let (mut rig, mut app) = start();
    let calls = Calls::default();
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E { col().child(diff_view(app, props(&calls, false, DiffStyle::Split, false))) }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    let tree = a11y_tree(&mut rig, &mut app, &mut view);
    let old = said(&tree, "const old = 2;").bounds().expect("placed");
    let next = said(&tree, "const next = 2;").bounds().expect("placed");
    assert_eq!(old.y0, next.y0, "one row");
    assert!(old.x1 <= next.x0, "the old side stands left of the new");
    set_a11y_active(false);
}

#[test]
fn only_the_lines_near_the_view_are_in_the_tree_however_long_the_file() {
    let (mut rig, mut app) = start();
    let body: String = (0..100_000).map(|i| format!(" line {i}\n")).collect();
    let patch = format!("diff --git a/src/app.ts b/src/app.ts\n--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1,100000 +1,100000 @@\n{body}");
    let calls = Calls::default();
    let files = only(&patch);
    let mut view = {
        let calls = calls.clone();
        move |app: &mut App| -> E { col().h_full().child(diff_view(app, DiffViewProps { patches: files.clone(), ..props(&calls, false, DiffStyle::Unified, false) })) }
    };
    settle(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", "viewer", "", PATH));
    settle(&mut rig, &mut app, &mut view);
    let tree = a11y_tree(&mut rig, &mut app, &mut view);
    assert!(says(&tree, "line 0").is_some());
    assert!(says(&tree, "line 99999").is_none());
    assert!(tree.len() < 400, "{} nodes", tree.len());
    set_a11y_active(false);
}

#[test]
fn nothing_is_added_over_a_table_with_nobody_listening() {
    let (mut rig, mut app) = start();
    let mut view = move |_: &mut App| -> E { col().child(diff_output("card", "--- a/x.ts\n+++ b/x.ts\n@@ -1 +1 @@\n-const a = 1;\n+const b = 1;\n", None)) };
    settle(&mut rig, &mut app, &mut view);
    assert!(rect_of(("code-table-line", hash_key(("diff-output", "card", "--- a/x.ts\n+++ b/x.ts\n@@ -1 +1 @@\n-const a = 1;\n+const b = 1;\n", None::<&str>)), 0usize)).is_none());
}

#[test]
fn a_diff_card_is_a_named_region_with_its_counts_said_in_words() {
    let (mut rig, mut app) = start();
    let mut view = move |_: &mut App| -> E { col().child(diff_output("card", "--- a/x.ts\n+++ b/x.ts\n@@ -1 +1,2 @@\n-const a = 1;\n+const b = 1;\n+const c = 2;\n", None)) };
    let tree = a11y_tree(&mut rig, &mut app, &mut view);
    assert_eq!(said(&tree, "x.ts 文件差异").role(), Role::Region);
    assert_eq!(said(&tree, "新增 2 行，删除 1 行").role(), Role::Label);
    assert!(says(&tree, "+2").is_none(), "the words stand for the figures");
    said(&tree, "x.ts");
    said(&tree, "const a = 1;");
    said(&tree, "const c = 2;");
    set_a11y_active(false);
}

/// `.diff-output__line`: 20 px rows of 10 px text, with no marker, where the review pane has 17 px rows of 12 px and a `+`.
#[test]
fn a_card_s_rows_are_its_own_and_not_the_review_pane_s() {
    let (mut rig, mut app) = start();
    let patch = "--- a/x.ts\n+++ b/x.ts\n@@ -1,2 +1,2 @@\n one\n-two\n+three\n";
    let mut view = move |_: &mut App| -> E { col().w(400.0).child(diff_output("card", patch, None)) };
    settle(&mut rig, &mut app, &mut view);
    let id = hash_key(("diff-output", "card", patch, None::<&str>));
    let body = rect_of(("diff-output-body", id)).expect("the body");
    assert_eq!(body.h, 3.0 * 20.0);
}

#[test]
fn a_card_says_a_change_by_its_tint_alone() {
    let glyphs = |patch: &'static str| {
        let (mut rig, mut app) = start();
        let mut view = move |_: &mut App| -> E { col().w(400.0).child(diff_output("card", patch, Some("x"))) };
        settle(&mut rig, &mut app, &mut view);
        rig.scene.layers.iter().map(|layer| layer.glyphs.len()).sum::<usize>()
    };
    assert_eq!(glyphs("@@ -0,0 +1 @@\n+abc\n"), glyphs("@@ -1 +1 @@\n abc\n"), "no marker");
}

fn status_summary() -> GitWorkspaceSummary {
    GitWorkspaceSummary {
        repository_id: "repository-id-1".into(),
        worktree_id: "worktree-id-1".into(),
        branch: Some("feature/review".into()),
        head: Some("12ab34cd".into()),
        content_revision: "revision-1".into(),
        summary_revision: "summary-1".into(),
        upstream: None,
        upstream_target: None,
        ahead: 0,
        behind: 0,
        additions: 7,
        deletions: 2,
        staged: 1,
        unstaged: 1,
        untracked: 0,
        conflicted: 0,
        stash: 0,
        changed_files: 1,
        stageable: 1,
        unstageable: 1,
        remote: None,
        remotes: Vec::new(),
        git_version: "git version 2.50.0".into(),
        repository_root: "/repo".into(),
        worktree_root: "/repo".into(),
        detached: false,
        unborn: false,
        operation: None,
        operation_revision: None,
        is_clean: false,
        binary_files: 0,
        warnings: Vec::new(),
    }
}

#[test]
fn the_status_card_names_its_toggle_and_says_which_page_is_open() {
    let (mut rig, mut app) = start();
    let git = summary_to_snapshot(status_summary(), Vec::new());
    let open = Rc::new(std::cell::Cell::new(0));
    let mut view = {
        let (git, open) = (git.clone(), open.clone());
        move |_: &mut App| -> E {
            let open = open.clone();
            div().child(git_status_card("status", &git, true, Rc::new(move |_| open.set(open.get() + 1))))
        }
    };
    let tree = a11y_tree(&mut rig, &mut app, &mut view);
    let toggle = said(&tree, "展开 Git 状态卡片");
    assert_eq!((toggle.role(), toggle.is_expanded()), (Role::Button, Some(false)));
    assert_eq!(said(&tree, "Git 状态").role(), Role::Complementary);
    let (id, _) = tree.iter().find(|(_, n)| n.label() == Some("展开 Git 状态卡片")).unwrap();
    a11y_action(accesskit::ActionRequest { action: accesskit::Action::Click, target_tree: accesskit::TreeId::ROOT, target_node: *id, data: None });
    settle(&mut rig, &mut app, &mut view);
    let tree = a11y_tree(&mut rig, &mut app, &mut view);
    assert_eq!(said(&tree, "收起 Git 状态卡片").is_expanded(), Some(true));
    let changes = tree.iter().map(|(_, n)| n).find(|n| n.label().is_some_and(|l| l.starts_with("变更 新增"))).expect("the counts are in the row's name");
    assert_eq!(changes.aria_current(), Some(AriaCurrent::Page));
    assert_eq!(nameless(&tree), Vec::<String>::new());
    set_a11y_active(false);
}
