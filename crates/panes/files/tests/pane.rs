//! Upstream's `FilesPane.test.tsx`, on the real host and a real folder rather than a mocked `invoke`. Where a case depends
//! on an answer arriving late (a fenced read), it is in `fences` at the end, which drives the state directly.

#[path = "rig/mod.rs"]
mod rig;

use mewrk_ui_common::context_menu::{menu_panel_key, menu_row_key};
use mewrk_ui_common::popover_menu::{panel_key, trigger_key};
use fluent::el::*;
use fluent::ui::Key;
use mewrk_pane_files::browser::*;
use mewrk_pane_files::pane::*;
use mewrk_pane_files::previews;
use rig::*;
use std::time::Duration;

fn sample() -> Rig {
    let rig = Rig::new();
    rig.write("README.md", "# Title\n\nHello.\n");
    rig.write("src/App.tsx", "export const a = 1;\nexport const b = 2;\n");
    rig.write("src/lib/deep.ts", "x\n");
    rig.write("notes.txt", "plain\n");
    rig
}

fn loaded(rig: &mut Rig) {
    rig.until("the root", |r| r.tree().iter().any(|n| n.contains("README.md")));
}

/// The folder opens only once the double-click window has passed.
fn open_folder(rig: &mut Rig, relative: &str) {
    let r = rig.row(relative);
    rig.click(r);
    let key = rig.key(relative);
    rig.until("the folder to open", |x| x.state().active().is_some_and(|p| p.expanded.contains(&key)));
}

#[test]
fn lists_the_workspace_root_with_directories_first() {
    let mut rig = sample();
    loaded(&mut rig);
    assert_eq!(rig.tree(), ["src", "notes.txt", "README.md"]);
}

#[test]
fn shows_a_loading_row_until_the_root_answers() {
    let mut rig = sample();
    rig.frame();
    assert_eq!(rig.tree(), ["~Loading…"]);
    loaded(&mut rig);
}

#[test]
fn says_so_when_a_folder_holds_nothing() {
    let mut rig = Rig::new();
    rig.until("the empty note", |r| r.tree() == ["~Folder is empty"]);
}

#[test]
fn waits_for_the_pane_to_be_shown_before_touching_the_host() {
    let mut rig = sample();
    rig.props.active = false;
    rig.settle(100);
    assert!(rig.state().listings.is_empty());
    rig.props.active = true;
    loaded(&mut rig);
}

#[test]
fn replaces_the_tree_when_the_root_itself_cannot_be_read() {
    let mut rig = Rig::new();
    rig.props.workspaces[0].path = rig.path("no-such-dir");
    let key = rig.key("no-such-dir");
    rig.until("the error", |r| matches!(r.state().listings.get(&key), Some(DirectoryState::Error(_))));
}

#[test]
fn lazy_loads_a_directory_on_expansion_and_keeps_the_listing() {
    let mut rig = sample();
    loaded(&mut rig);
    let src = rig.key("src");
    assert!(!rig.state().listings.contains_key(&src));
    open_folder(&mut rig, "src");
    assert_eq!(rig.tree(), ["src", "  lib", "  App.tsx", "notes.txt", "README.md"]);
    // Closing and opening again uses what was read.
    let requests_before = rig.state().listings.len();
    let r = rig.row("src");
    rig.click(r);
    rig.until("the folder to close", |x| x.tree().len() == 3);
    let r = rig.row("src");
    rig.click(r);
    rig.until("the folder to reopen", |x| x.tree().len() == 5);
    assert_eq!(rig.state().listings.len(), requests_before);
}

fn rig_key(rig: &mut Rig, relative: &str) -> String {
    rig.key(relative)
}

#[test]
fn expands_a_directory_whose_name_is_also_an_object_member() {
    let mut rig = Rig::new();
    rig.write("constructor/inner.txt", "x");
    rig.until("the root", |r| r.tree() == ["constructor"]);
    open_folder(&mut rig, "constructor");
    assert_eq!(rig.tree(), ["constructor", "  inner.txt"]);
}

#[test]
fn opens_a_file_in_a_line_numbered_viewer_on_the_page_and_keeps_the_tree_beside_it() {
    let mut rig = sample();
    loaded(&mut rig);
    let r = rig.row("notes.txt");
    rig.click(r);
    let key = rig.key("notes.txt");
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { .. })));
    assert_eq!(rig.file_key().as_deref(), Some(key.as_str()));
    // The tree stays beside the file as a 240 px column.
    assert_eq!(rig.row("notes.txt").w.round(), (TREE_COLUMN_WIDTH - 8.0 - 12.0).round().max(0.0).min(9999.0).max(rig.row("notes.txt").w.round()));
    assert!(rig.row("notes.txt").right() <= TREE_COLUMN_WIDTH + 1.0);
    assert!(rect_of(("numbered-code", key)).is_some());
}

#[test]
fn says_a_binary_file_cannot_be_shown_and_warns_of_a_truncated_one() {
    let mut rig = sample();
    let mut binary = vec![0u8; 16];
    binary[3] = 7;
    rig.write("blob.dat", &binary);
    rig.write("big.log", "line\n".repeat(2_000_000));
    rig.frame();
    loaded(&mut rig);
    let r = rig.row("blob.dat");
    rig.click(r);
    let key = rig.key("blob.dat");
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { binary: true, .. })));
    let r = rig.row("big.log");
    rig.click(r);
    let key = rig.key("big.log");
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { truncated: true, .. })));
}

#[test]
fn shows_the_hosts_message_when_a_file_cannot_be_read() {
    let mut rig = sample();
    loaded(&mut rig);
    let page = rig.state().active_page.clone().unwrap();
    let file = Location::new(&None, rig.path("gone.txt"));
    show_file(&mut rig.app, "files", &page, file.clone(), false);
    rig.until("the error", |x| matches!(x.state().viewers.get(&file.key()), Some(ViewerState::Error(_))));
}

#[test]
fn rereads_open_listings_and_the_open_file_from_refresh() {
    let mut rig = sample();
    loaded(&mut rig);
    open_folder(&mut rig, "src");
    let r = rig.row("notes.txt");
    rig.click(r);
    let key = rig.key("notes.txt");
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { .. })));
    rig.write("src/fresh.ts", "y");
    rig.write("notes.txt", "changed\n");
    refresh(&mut rig.app, "files");
    rig.until("the new entry", |x| x.tree().iter().any(|n| n.contains("fresh.ts")));
    rig.until("the new text", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { content, .. }) if &**content == "changed\n"));
}

#[test]
fn goes_into_a_folder_on_a_double_click() {
    let mut rig = sample();
    loaded(&mut rig);
    let r = rig.row("src");
    rig.click(r);
    rig.click(r);
    let want = rig.path("src");
    rig.until("the page to move", |x| x.state().active().and_then(|p| p.directory.as_ref()).is_some_and(|d| d.path == want));
    assert!(rig.state().active().unwrap().expanded.is_empty(), "it never opened on the way in");
    rig.until("the listing", |x| x.tree().contains(&"App.tsx".to_owned()));
}

#[test]
fn opens_a_folder_on_a_single_click_only_once_a_double_clicks_window_has_passed() {
    let mut rig = sample();
    loaded(&mut rig);
    let r = rig.row("src");
    rig.click(r);
    let key = rig.key("src");
    assert!(!rig.state().active().unwrap().expanded.contains(&key));
    std::thread::sleep(Duration::from_millis(DOUBLE_CLICK_MS + 40));
    rig.until("the folder to open", |x| x.state().active().unwrap().expanded.contains(&key));
}

#[test]
fn reads_a_folder_ahead_while_the_pointer_rests_on_it() {
    let mut rig = sample();
    loaded(&mut rig);
    let r = rig.row("src");
    rig.hover(r);
    let key = rig.key("src");
    rig.until("the read ahead", |x| x.state().listings.contains_key(&key));
    assert!(!rig.state().active().unwrap().expanded.contains(&key));
}

#[test]
fn scrolls_a_revealed_row_into_the_tree_once_and_leaves_the_tree_where_it_is_scrolled_after() {
    let mut rig = Rig::new();
    for i in 0..200 {
        rig.write(&format!("f{i:03}.txt"), "x");
    }
    rig.size = (900.0, 300.0);
    rig.until("the root", |r| r.tree().len() == 200);
    let target = Location::new(&None, rig.path("f150.txt"));
    reveal_in_tree(&mut rig.app, "files", target.clone());
    let tree = ("files-tree", "files".to_owned());
    let probe = tree.clone();
    rig.until("the row to scroll into view", move |_| scroll_state(probe.clone()).is_some_and(|(offset, _, _)| offset > 3000.0));
    assert_eq!(rig.state().revealed_key.as_deref(), Some(target.key().as_str()));
    scroll_request(tree.clone(), ScrollRequest::To(0.0));
    let probe = tree.clone();
    rig.until("the tree to move back", move |_| scroll_state(probe.clone()).is_some_and(|(offset, _, _)| offset < 1.0));
    rig.settle(50);
    assert!(scroll_state(tree.clone()).unwrap().0 < 1.0, "the tree stays where it was put");
    // The row lets go of its light.
    rig.until("the flash to end", |x| x.state().revealed_key.is_none());
}

#[test]
fn searches_the_pages_directory_from_the_filter_and_opens_a_match() {
    let mut rig = sample();
    loaded(&mut rig);
    set_filter(&mut rig.app, "files", "  deep ".to_owned());
    rig.until("the results", |x| matches!(&x.state().search, SearchState::Ready { matches, .. } if !matches.is_empty()));
    let key = rig.key("src/lib/deep.ts");
    let m = rig.rect(("files-row", "files", key.as_str()));
    rig.click(m);
    rig.until("the file", |x| x.file_key().as_deref() == Some(key.as_str()));
    // A new directory clears what was typed.
    let src = rig.path("src");
    navigate(&mut rig.app, "files", Some(Location::new(&None, src)));
    rig.until("the filter to clear", |x| x.state().filter.is_empty() && matches!(x.state().search, SearchState::Idle));
}

#[test]
fn does_not_search_while_the_filter_is_empty() {
    let mut rig = sample();
    loaded(&mut rig);
    set_filter(&mut rig.app, "files", "   ".to_owned());
    rig.settle(200);
    assert!(matches!(rig.state().search, SearchState::Idle));
    assert_eq!(rig.state().debounced_filter, "");
}

#[test]
fn says_when_nothing_matches() {
    let mut rig = sample();
    loaded(&mut rig);
    set_filter(&mut rig.app, "files", "zzzzzqqq".to_owned());
    rig.until("the empty answer", |x| matches!(&x.state().search, SearchState::Ready { matches, .. } if matches.is_empty()));
}

#[test]
fn walks_the_tree_with_the_arrow_keys_and_opens_with_enter() {
    let mut rig = sample();
    loaded(&mut rig);
    let r = rig.row("notes.txt");
    rig.click(r);
    let readme = rig.key("README.md");
    rig.press_key(Key::Down);
    assert_eq!(rig.state().focused_path.as_deref(), Some(readme.as_str()));
    rig.press_key(Key::Enter);
    rig.until("the file", |x| x.file_key().as_deref() == Some(readme.as_str()));
    assert!(!rig.state().active().unwrap().preview, "Enter keeps the page");
    rig.press_key(Key::Up);
    let want = Some(rig.key("notes.txt"));
    assert_eq!(rig.state().focused_path, want);
}

#[test]
fn opens_with_arrow_right_collapses_with_arrow_left_and_climbs_to_the_parent() {
    let mut rig = sample();
    loaded(&mut rig);
    open_folder(&mut rig, "src");
    let r = rig.row("src/App.tsx");
    rig.click(r);
    rig.press_key(Key::Left);
    let want = Some(rig.key("src"));
    assert_eq!(rig.state().focused_path, want, "a child climbs to its parent");
    rig.press_key(Key::Left);
    rig.until("the folder to close", |x| x.tree().len() == 3);
    rig.press_key(Key::Right);
    rig.until("the folder to open", |x| x.tree().len() == 5);
    rig.press_key(Key::Right);
    let want = Some(rig.key("src/lib"));
    assert_eq!(rig.state().focused_path, want, "an open folder steps into its first child");
}

#[test]
fn opens_a_workspace_recorded_under_home_where_its_machine_says_it_is() {
    let mut rig = sample();
    let home = browse_path(&std::env::home_dir().unwrap().to_string_lossy());
    rig.props.workspaces[0].path = "~".to_owned();
    rig.until("the page to take the machine's spelling", |x| x.state().active().and_then(|p| p.directory.as_ref()).is_some_and(|d| d.path == home));
}

// ---- address bar

#[test]
fn goes_up_to_the_top_of_the_machine_and_from_there_to_the_machines() {
    let mut rig = Rig::new();
    rig.props.workspaces[0].path = "/".to_owned();
    rig.until("the root", |x| x.state().listings.contains_key(&location_key(&None, "/")));
    go_up(&mut rig.app, "files");
    rig.frame();
    assert!(rig.state().active().unwrap().directory.is_none());
    assert_eq!(rig.tree(), ["@This machine"]);
    assert_eq!(rig.state().revealed_key.as_deref(), Some("local"));
    // From the machines a machine's row goes to its home.
    let r = rig.rect(("files-row", "files", "local"));
    rig.click(r);
    let home = browse_path(&std::env::home_dir().unwrap().to_string_lossy());
    rig.until("home", |x| x.state().active().and_then(|p| p.directory.as_ref()).is_some_and(|d| d.path == home));
}

#[test]
fn opens_a_typed_file_beside_its_folder_and_says_when_nothing_is_there() {
    let mut rig = sample();
    loaded(&mut rig);
    let d = rig.rect(("files-address-display", "files"));
    rig.click(d);
    rig.frame();
    assert!(rig.state().address.is_some());
    rig.replace_text(&format!("{}/src/App.tsx", rig.root.display()));
    assert!(rig.state().address.as_ref().unwrap().text.ends_with("src/App.tsx"));
    submit_address(&mut rig.app, "files");
    let key = rig.key("src/App.tsx");
    rig.until("the file", |x| x.file_key().as_deref() == Some(key.as_str()));
    assert!(rig.state().address.is_none());
    rig.state().address = Some(AddressEdit { text: rig_path(&rig_root(&mut rig), "nope"), error: None, busy: false });
    submit_address(&mut rig.app, "files");
    rig.until("the complaint", |x| x.state().address.as_ref().is_some_and(|a| a.error.as_deref() == Some("Nothing is there")));
}

fn rig_root(rig: &mut Rig) -> String {
    rig.root.display().to_string()
}

fn rig_path(root: &str, name: &str) -> String {
    format!("{root}/{name}")
}

#[test]
fn says_when_a_typed_machine_is_not_registered_and_keeps_what_was_typed() {
    let mut rig = sample();
    loaded(&mut rig);
    rig.state().address = Some(AddressEdit { text: "nowhere:/srv".to_owned(), error: None, busy: false });
    submit_address(&mut rig.app, "files");
    let a = rig.state().address.clone().unwrap();
    assert_eq!(a.text, "nowhere:/srv");
    assert_eq!(a.error.as_deref(), Some("No SSH machine named nowhere is registered"));
}

// ---- menus

#[test]
fn offers_a_files_actions_on_a_right_click() {
    let mut rig = sample();
    loaded(&mut rig);
    rig.open_row_menu("notes.txt");
    assert_eq!(rig.menu_labels(), ["Open in File Manager", "Open File", "Open With", "Open in New Page", "Rename…", "Delete…", "Copy Absolute Path", "Copy Relative Path"]);
    rig.choose("Copy Relative Path");
    rig.frame();
    rig.frame();
    assert_eq!(rig.clipboard.0.as_deref(), Some("notes.txt"));
    rig.open_row_menu("notes.txt");
    rig.choose("Copy Absolute Path");
    rig.frame();
    rig.frame();
    assert_eq!(rig.clipboard.0, Some(native_spelling(&rig.path("notes.txt"))));
}

#[test]
fn opens_a_file_or_a_folder_in_a_page_of_its_own() {
    let mut rig = sample();
    loaded(&mut rig);
    rig.open_row_menu("notes.txt");
    rig.choose("Open in New Page");
    assert_eq!(rig.state().pages.len(), 2);
    assert_eq!(rig.file_key(), Some(rig.key("notes.txt")));
    assert!(!rig.state().active().unwrap().preview);
    let first = rig.state().pages[0].id.clone();
    rig.state().active_page = Some(first);
    rig.frame();
    rig.open_row_menu("src");
    rig.choose("Open in New Page");
    assert_eq!(rig.state().pages.len(), 3);
    let src = rig.path("src");
    assert_eq!(rig.state().active().unwrap().directory.as_ref().unwrap().path, src);
}

#[test]
fn lists_the_programs_that_open_a_file_when_open_with_is_pointed_at() {
    let mut rig = sample();
    loaded(&mut rig);
    rig.open_row_menu("notes.txt");
    let serial = rig.state().menu.as_ref().unwrap().serial;
    let menu = ("files", "files", serial);
    let item = rig.state().menu.clone().unwrap().sections.iter().flat_map(|s| s.items.iter()).find(|i| i.id == "open-with").unwrap().clone();
    assert!(item.load_children.is_some(), "rows on demand");
    rig.frame();
    rig.frame();
    let open_with = rig.rect(menu_row_key(menu, &[], "open-with"));
    rig.hover(open_with);
    let programs = menu_panel_key(menu, &["open-with"]);
    rig.until("the programs", move |_| rect_of(programs).is_some());
    rig.settle(50);
    // Some program opens a .txt, so the panel is taller than the one line a failure or a wait is.
    assert!(rig.rect(programs).h > 30.0, "{:?}", rig.rect(programs));
}

use std::rc::Rc;

#[test]
fn keeps_this_computers_desktop_out_of_reach_for_another_machines_files() {
    let mut rig = sample();
    loaded(&mut rig);
    let machine = Some(mewrk_client::model::RunTarget::Ssh { machine_id: "id".to_owned() });
    let location = Location::new(&machine, "/srv/x");
    let sections = entry_menu(rig.state(), &location, BrowseEntryKind::File);
    let find = |id: &str| sections.iter().flat_map(|s| s.items.iter()).find(|i| i.id == id).unwrap().clone();
    assert!(find("reveal").disabled && find("reveal").title.is_some());
    assert!(find("open-with").disabled && find("open-with").title.is_some());
}

#[test]
fn renames_a_row_in_place() {
    let mut rig = sample();
    loaded(&mut rig);
    open_folder(&mut rig, "src");
    let r = rig.row("src/App.tsx");
    rig.click(r);
    rig.until("the viewer", |x| x.file_key().is_some());
    rig.open_row_menu("src");
    rig.choose("Rename…");
    rig.frame();
    rig.frame();
    assert_eq!(rig.state().renaming.as_ref().unwrap().draft, "src");
    rig.replace_text("code");
    assert_eq!(rig.state().renaming.as_ref().unwrap().draft, "code");
    rig.press_key(Key::Enter);
    let moved_file = rig.key("code/App.tsx");
    rig.until("the rename", |x| x.state().renaming.is_none() && x.tree().iter().any(|n| n == "code"));
    assert!(rig.root.join("code/App.tsx").exists());
    assert_eq!(rig.file_key(), Some(moved_file), "the open file follows");
    let code = rig.key("code");
    assert!(rig.state().active().unwrap().expanded.contains(&code), "and so does the opened folder");
}

#[test]
fn escape_drops_a_rename_and_reports_a_refused_one() {
    let mut rig = sample();
    loaded(&mut rig);
    rig.open_row_menu("notes.txt");
    rig.choose("Rename…");
    rig.frame();
    rig.frame();
    rig.press_key(Key::Escape);
    assert!(rig.state().renaming.is_none());
    rig.state().renaming = Some(Renaming { machine: None, path: rig_path(&rig_root(&mut rig), "notes.txt"), draft: "bad/name".to_owned(), busy: false, tab: None });
    commit_rename(&mut rig.app, "files");
    rig.until("the refusal", |x| x.state().action_error.is_some());
    assert!(rig.state().renaming.as_ref().is_some_and(|r| !r.busy));
    assert!(rig.root.join("notes.txt").exists());
}

#[test]
fn asks_before_a_delete_saying_where_it_goes() {
    let mut rig = sample();
    loaded(&mut rig);
    let r = rig.row("notes.txt");
    rig.click(r);
    rig.until("the viewer", |x| x.file_key().is_some());
    rig.open_row_menu("notes.txt");
    rig.choose("Delete…");
    assert!(rig.root.join("notes.txt").exists(), "nothing happens before the answer");
    let cancel = rig.rect(("files-delete-cancel", "files"));
    rig.click(cancel);
    assert!(rig.state().confirm_delete.is_none());
    // The rest puts a file in this computer's Trash, which nobody should get from running the tests: `MEWRK_TEST_TRASH=1`.
    if std::env::var_os("MEWRK_TEST_TRASH").is_none() {
        return;
    }
    rig.open_row_menu("notes.txt");
    rig.choose("Delete…");
    let confirm = rig.rect(("files-delete-confirm", "files"));
    rig.click(confirm);
    rig.until("the delete", |x| x.state().confirm_delete.is_none() && !x.tree().iter().any(|n| n == "notes.txt"));
    assert!(!rig.root.join("notes.txt").exists());
    assert_eq!(rig.file_key(), None, "the page lets go of a deleted file");
}

#[test]
fn a_delete_the_host_refuses_says_why_and_keeps_the_pages() {
    let mut rig = sample();
    loaded(&mut rig);
    rig.state().confirm_delete = Some(ConfirmDelete { machine: None, path: rig_path(&rig_root(&mut rig), "no-such-file"), busy: false });
    commit_delete(&mut rig.app, "files");
    rig.until("the refusal", |x| x.state().confirm_delete.is_none() && x.state().action_error.is_some());
    let dismiss = rig.rect(("files-error-dismiss", "files"));
    rig.click(dismiss);
    assert!(rig.state().action_error.is_none());
}

#[test]
fn opens_a_rows_menu_under_its_three_dots_and_acts_on_a_real_click() {
    let mut rig = sample();
    loaded(&mut rig);
    let row = rig.row("notes.txt");
    rig.hover(row);
    let key = rig.key("notes.txt");
    let button_key = ("files-row-menu", "files".to_owned(), key.clone());
    let button = rig.rect(button_key.clone());
    rig.click(button);
    rig.frame();
    let serial = rig.state().menu.as_ref().expect("a menu").serial;
    let menu = ("files", "files", serial);
    rig.frame();
    rig.frame();
    let panel = rig.rect(menu_panel_key(menu, &[]));
    assert!(panel.y >= button.bottom() - 1.0 && panel.y < button.bottom() + 10.0 || panel.bottom() <= button.y + 1.0, "against its trigger: {panel:?} {button:?}");
    assert!((panel.right() - button.right()).abs() < 2.0, "right edges flush: {panel:?} {button:?}");
    // A real press on a real row.
    let copy = rig.rect(menu_row_key(menu, &[], "copy-relative"));
    rig.click(copy);
    rig.frame();
    rig.frame();
    assert_eq!(rig.clipboard.0.as_deref(), Some("notes.txt"));
    assert!(rig.state().menu.is_none());
}

#[test]
fn shows_the_programs_beside_open_with_when_the_pointer_reaches_it() {
    let mut rig = sample();
    loaded(&mut rig);
    rig.open_row_menu("notes.txt");
    let serial = rig.state().menu.as_ref().unwrap().serial;
    let menu = ("files", "files", serial);
    rig.frame();
    rig.frame();
    let open_with = rig.rect(menu_row_key(menu, &[], "open-with"));
    rig.hover(open_with);
    let programs = menu_panel_key(menu, &["open-with"]);
    rig.until("the programs", move |_| rect_of(programs).is_some());
    assert!(rig.rect(programs).x >= rig.rect(menu_panel_key(menu, &[])).right() - 2.0);
    // Moving to a row with no rows of its own lets them go, once the pointer has rested there.
    let plain = rig.rect(menu_row_key(menu, &[], "rename"));
    rig.hover(plain);
    rig.until("the programs to go", move |_| rect_of(programs).is_none());
}

#[test]
fn opens_the_workspaces_menu_above_the_footer() {
    let mut rig = sample();
    loaded(&mut rig);
    let trigger = rig.rect(trigger_key(("files-workspaces", "files")));
    rig.click(trigger);
    rig.frame();
    rig.frame();
    rig.frame();
    let panel = rig.rect(panel_key(("files-workspaces", "files")));
    assert!(panel.bottom() <= trigger.y + 1.0, "{panel:?} above {trigger:?}");
}

#[test]
fn colours_a_file_it_has_a_grammar_for_and_keeps_its_indentation_on_the_grid() {
    let lines = mewrk_pane_files::code::split_code_lines("fn a() {\n    let x = 1;\n}\n");
    assert_eq!(lines.len(), 3);
    let lang = mewrk_panes::highlight::lang_of("rust").expect("a grammar");
    let mut tokens = Vec::new();
    mewrk_panes::highlight::line(lang, &mut mewrk_panes::highlight::State::default(), "fn a() {", &mut tokens);
    assert!(tokens.iter().any(|(_, t)| *t == mewrk_panes::highlight::Token::Keyword));
}

#[test]
fn makes_a_folder_from_the_menu_of_the_trees_empty_space() {
    let mut rig = sample();
    loaded(&mut rig);
    let tree = rig.rect(("files-tree", "files".to_owned()));
    rig.right_click(fluent::rect(tree.x, tree.bottom() - 30.0, tree.w, 20.0));
    assert_eq!(rig.menu_labels(), ["Open in File Manager", "Open in New Page", "New Folder", "Copy Absolute Path", "Copy Relative Path"]);
    rig.choose("New Folder");
    rig.frame();
    rig.frame();
    rig.type_text("made");
    assert_eq!(rig.state().creating.as_ref().unwrap().draft, "made");
    rig.press_key(Key::Enter);
    rig.until("the folder", |x| x.state().creating.is_none() && x.tree().iter().any(|n| n == "made"));
    assert!(rig.root.join("made").is_dir());
    let want = Some(rig.key("made"));
    assert_eq!(rig.state().focused_path, want);
}

#[test]
fn opens_a_page_at_any_workspace_from_the_tab_strips_plus() {
    let mut rig = sample();
    let second = TempDir::new("second");
    std::fs::write(second.0.join("other.txt"), "x").unwrap();
    rig.props.workspaces.push(FilesPaneWorkspace { number: 2, machine: None, path: second.0.to_string_lossy().into_owned() });
    loaded(&mut rig);
    let sections = {
        let pick: Rc<dyn Fn(&mut mewrk_client::App, Location)> = Rc::new(|app, location| {
            let page = make_page(app, Some(location));
            open_page(app, "files", page);
        });
        workspace_sections(rig.state(), pick)
    };
    assert_eq!(sections.len(), 1, "both are on this machine");
    assert_eq!(sections[0].items.len(), 2);
    (sections[0].items[1].on_select.clone().unwrap())(&mut rig.app);
    rig.until("the second workspace", |x| x.tree() == ["other.txt"]);
    assert_eq!(rig.state().pages.len(), 2);
}

#[test]
fn opens_the_only_workspace_straight_from_the_plus() {
    let mut rig = sample();
    loaded(&mut rig);
    let plus = rig.rect(("files-add-page", "files"));
    rig.click(plus);
    assert_eq!(rig.state().pages.len(), 2);
    let root = rig.path("");
    assert_eq!(rig.state().active().unwrap().directory.as_ref().unwrap().path, root);
}

#[test]
fn opens_a_pages_menu_on_its_tab_with_its_files_own_actions() {
    let mut rig = sample();
    loaded(&mut rig);
    let request = rig.request(&rig.path("notes.txt"), None, 1);
    rig.props.open_request = Some(request);
    rig.until("the file", |x| x.file_key().is_some());
    let page = rig.state().active_page.clone().unwrap();
    let labels: Vec<String> = tab_menu(rig.state(), &page).iter().flat_map(|s| s.items.iter().map(|i| i.label.clone())).collect();
    assert_eq!(labels, ["Keep open", "Reveal in file tree", "Open in File Manager", "Open With", "Open in New Page", "Rename…", "Delete…", "Copy Absolute Path", "Copy Relative Path", "Close page", "Close other pages", "Close all pages"]);
}

#[test]
fn closes_a_page_onto_its_neighbour_and_says_so_when_none_is_left() {
    let mut rig = sample();
    loaded(&mut rig);
    for _ in 0..2 {
        let page = make_page(&mut rig.app, Some(Location::new(&None, rig.root.to_string_lossy().into_owned())));
        open_page(&mut rig.app, "files", page);
    }
    let ids: Vec<String> = rig.state().pages.iter().map(|p| p.id.clone()).collect();
    rig.state().active_page = Some(ids[1].clone());
    let doomed = ids[1].clone();
    close_pages(&mut rig.app, "files", |p| p.id == doomed);
    assert_eq!(rig.state().active_page.as_deref(), Some(ids[2].as_str()));
    close_pages(&mut rig.app, "files", |_| true);
    assert_eq!(rig.state().active_page, None);
    rig.frame();
    assert!(rig.state().pages.is_empty());
}

// ---- viewer

#[test]
fn renders_a_markdown_file_and_gives_it_a_way_back_to_its_source() {
    let mut rig = sample();
    loaded(&mut rig);
    let r = rig.row("README.md");
    rig.click(r);
    let key = rig.key("README.md");
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { .. })));
    let scroller = format!("doc-scroll/{key}");
    assert!(rect_of(scroller.as_str()).is_some(), "rendered");
    let toggle = rig.rect(("files-source", "files"));
    rig.click(toggle);
    rig.frame();
    assert!(rig.state().source_keys.contains(&key));
    assert!(rect_of(("numbered-code", key.clone())).is_some(), "the source, numbered");
}

#[test]
fn offers_no_source_toggle_for_a_file_that_has_only_one_form() {
    let mut rig = sample();
    loaded(&mut rig);
    let r = rig.row("notes.txt");
    rig.click(r);
    let key = rig.key("notes.txt");
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { .. })));
    assert!(rect_of(("files-source", "files")).is_none());
}

#[test]
fn offers_to_mention_a_right_clicked_line_of_a_file_in_its_workspace() {
    let mut rig = sample();
    let said = Rc::new(std::cell::RefCell::new(None));
    let slot = said.clone();
    rig.props.on_mention = Some(Rc::new(move |_, m| *slot.borrow_mut() = Some(m)));
    loaded(&mut rig);
    open_folder(&mut rig, "src");
    let r = rig.row("src/App.tsx");
    rig.click(r);
    let key = rig.key("src/App.tsx");
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { .. })));
    let code = rig.rect(("code-surface", key.as_str()));
    rig.right_click(fluent::rect(code.x + 40.0, code.y + 20.0, 10.0, 10.0));
    assert_eq!(rig.menu_labels(), ["Mention in Conversation"]);
    rig.choose("Mention in Conversation");
    assert_eq!(*said.borrow(), Some(FileMention { path: "src/App.tsx".to_owned(), workspace: Some(1), line: 2 }));
}

fn png_bytes() -> Vec<u8> {
    let mut png = Vec::new();
    image::RgbaImage::from_pixel(4, 3, image::Rgba([200, 10, 10, 255])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    png
}

#[test]
fn shows_a_picture_from_its_own_bytes_rather_than_calling_it_binary() {
    let mut rig = sample();
    rig.write("shot.png", png_bytes());
    rig.frame();
    loaded(&mut rig);
    let r = rig.row("shot.png");
    rig.click(r);
    rig.until("the picture on screen", |x| x.images_on_screen() > 0);
    let key = rig.key("shot.png");
    assert!(!rig.state().viewers.contains_key(&key), "never read as text");
    assert!(rect_of(("zoom-actual", key.as_str())).is_some());
}

#[test]
fn explains_a_picture_the_host_refused_to_send_whole() {
    let mut rig = sample();
    rig.write("huge.png", vec![0u8; 9 * 1024 * 1024]);
    rig.frame();
    loaded(&mut rig);
    let r = rig.row("huge.png");
    rig.click(r);
    let key = rig.key("huge.png");
    rig.until("the refusal", |x| matches!(x.state().media.get(&key), Some(MediaState::TooLarge)));
    assert!(rect_of(("reveal", key.as_str())).is_some(), "with a way to it in the file manager");
}

#[test]
fn reads_the_pictures_a_rendered_document_points_at() {
    let mut rig = sample();
    rig.write("docs/guide.md", "![a](../assets/a.png)\n\n![web](https://x.example/b.png)\n");
    rig.write("assets/a.png", png_bytes());
    rig.frame();
    loaded(&mut rig);
    let page = rig.state().active_page.clone().unwrap();
    let file = Location::new(&None, rig.path("docs/guide.md"));
    show_file(&mut rig.app, "files", &page, file, false);
    let picture = rig.key("assets/a.png");
    rig.until("the picture's bytes", |x| matches!(x.state().media.get(&picture), Some(MediaState::Ready(_))));
    rig.until("the picture on screen", |x| x.images_on_screen() > 0);
    assert_eq!(rig.state().media.len(), 1, "the web's picture is nobody's to read");
    // Closing the file gives the bytes back.
    let page = rig.state().active_page.clone().unwrap();
    rig.state().update_page(&page, |p| p.file = None);
    rig.frame();
    assert!(rig.state().media.is_empty());
}

#[test]
fn follows_a_relative_link_inside_a_document_instead_of_navigating() {
    let mut rig = sample();
    rig.write("docs/a.md", "x");
    rig.write("docs/b.md", "line1\nline2\nline3\n");
    rig.frame();
    loaded(&mut rig);
    let page = rig.state().active_page.clone().unwrap();
    let file = Location::new(&None, rig.path("docs/a.md"));
    show_file(&mut rig.app, "files", &page, file, false);
    on_document_link(&mut rig.app, "files", "b.md#L2");
    let target = rig.key("docs/b.md");
    rig.until("the file", |x| x.file_key().as_deref() == Some(target.as_str()));
    rig.until("the line to light", |x| x.state().lit_line == Some((target.clone(), 2)));
    assert!(rig.state().source_keys.contains(&target), "a line names a place in the source");
    // A climb out of the workspace goes nowhere.
    on_document_link(&mut rig.app, "files", "../../../outside.md");
    rig.frame();
    assert_eq!(rig.file_key().as_deref(), Some(target.as_str()));
}

#[test]
fn shows_a_linked_directory_in_the_tree_instead_of_reading_it_as_a_file() {
    let mut rig = sample();
    rig.write("docs/a.md", "x");
    rig.write("docs/sub/c.txt", "y");
    rig.frame();
    loaded(&mut rig);
    let page = rig.state().active_page.clone().unwrap();
    let file = Location::new(&None, rig.path("docs/a.md"));
    show_file(&mut rig.app, "files", &page, file, false);
    on_document_link(&mut rig.app, "files", "sub/");
    let folder = rig.key("docs/sub");
    rig.until("the folder to open in the tree", |x| x.state().active().unwrap().expanded.contains(&folder));
    assert_eq!(rig.file_key(), Some(rig.key("docs/a.md")), "the document stays");
    rig.until("the row", |x| x.tree().iter().any(|n| n.ends_with("c.txt")));
}

#[test]
fn reads_a_file_outside_the_workspace_against_the_top_of_its_filesystem() {
    let mut rig = sample();
    let elsewhere = TempDir::new("else");
    let path = elsewhere.0.join("x.txt");
    std::fs::write(&path, "x\n").unwrap();
    let path = browse_path(&path.to_string_lossy());
    rig.props.open_request = Some(rig.request(&path, None, 1));
    let key = location_key(&None, &path);
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { .. })));
    let top = filesystem_root(&path);
    let file = Location::new(&None, path);
    assert_eq!(rig.state().document_root(&file), top);
}

// ---- open requests

#[test]
fn opens_a_folder_a_request_names_as_a_page_browsing_that_folder() {
    let mut rig = sample();
    let (seen, handled) = handled_counter();
    rig.props.on_open_request_handled = Some(handled);
    let mut request = rig.request(&rig.path("src"), None, 7);
    request.folder = true;
    rig.props.open_request = Some(request);
    rig.frame();
    assert_eq!(seen.get(), 7);
    let src = rig.path("src");
    assert_eq!(rig.state().active().unwrap().directory.as_ref().unwrap().path, src);
    assert_eq!(rig.state().pages.len(), 2);
}

#[test]
fn opens_the_file_a_request_names_on_a_preview_page_in_its_workspace() {
    let mut rig = sample();
    rig.frame();
    rig.props.open_request = Some(rig.request(&rig.path("src/App.tsx"), None, 1));
    rig.frame();
    assert_eq!(rig.state().pages.len(), 1, "the untouched first page is where it goes");
    let page = rig.state().active().unwrap().clone();
    assert!(page.preview);
    assert_eq!(page.directory.unwrap().path, rig.path(""));
    assert!(page.expanded.contains(&rig.key("src")), "with its folder open");
}

#[test]
fn reuses_the_preview_page_for_the_next_request_and_keeps_a_kept_one() {
    let mut rig = sample();
    rig.frame();
    rig.props.open_request = Some(rig.request(&rig.path("src/App.tsx"), None, 1));
    rig.frame();
    rig.props.open_request = Some(rig.request(&rig.path("notes.txt"), None, 2));
    rig.frame();
    assert_eq!(rig.state().pages.len(), 1);
    assert_eq!(rig.file_key(), Some(rig.key("notes.txt")));
    let page = rig.state().active_page.clone().unwrap();
    keep_page(&mut rig.app, "files", &page);
    rig.props.open_request = Some(rig.request(&rig.path("README.md"), None, 3));
    rig.frame();
    assert_eq!(rig.state().pages.len(), 2, "a kept page is not taken over");
    // A file already showing is shown where it is.
    rig.props.open_request = Some(rig.request(&rig.path("notes.txt"), None, 4));
    rig.frame();
    assert_eq!(rig.state().pages.len(), 2);
    assert_eq!(rig.state().active_page.as_deref(), Some(page.as_str()));
}

#[test]
fn gives_a_file_asked_for_on_a_page_of_its_own_a_kept_page_beside_the_preview_page() {
    let mut rig = sample();
    rig.frame();
    rig.props.open_request = Some(rig.request(&rig.path("notes.txt"), None, 1));
    rig.frame();
    let mut request = rig.request(&rig.path("README.md"), None, 2);
    request.new_page = true;
    rig.props.open_request = Some(request);
    rig.frame();
    assert_eq!(rig.state().pages.len(), 2);
    assert!(rig.state().pages[0].preview);
    assert!(!rig.state().pages[1].preview);
}

#[test]
fn lights_the_line_a_reference_named_and_shows_a_documents_source_for_it() {
    let mut rig = sample();
    rig.frame();
    rig.props.open_request = Some(rig.request(&rig.path("README.md"), Some(3), 1));
    let key = rig.key("README.md");
    rig.until("the line to light", |x| x.state().lit_line == Some((key.clone(), 3)));
    assert!(rig.state().source_keys.contains(&key));
    rig.until("the light to go", |x| x.state().lit_line.is_none());
}

#[test]
fn a_line_past_the_end_lights_nothing() {
    let mut rig = sample();
    rig.frame();
    rig.props.open_request = Some(rig.request(&rig.path("notes.txt"), Some(99), 1));
    rig.until("the jump to land", |x| x.state().jump_line.is_none() && x.state().pending_jump.is_none() && x.file_key().is_some());
    assert_eq!(rig.state().lit_line, None);
}

#[test]
fn asks_again_when_the_same_file_is_requested_twice() {
    let mut rig = sample();
    rig.frame();
    rig.props.open_request = Some(rig.request(&rig.path("notes.txt"), Some(1), 1));
    let key = rig.key("notes.txt");
    rig.until("the light", |x| x.state().lit_line == Some((key.clone(), 1)));
    rig.until("it to go", |x| x.state().lit_line.is_none());
    rig.props.open_request = Some(rig.request(&rig.path("notes.txt"), Some(1), 2));
    rig.until("the light again", |x| x.state().lit_line == Some((key.clone(), 1)));
}

// ---- tree drawer

#[test]
fn folds_the_tree_away_with_or_without_a_file_open_and_remembers_the_choice() {
    let mut rig = sample();
    loaded(&mut rig);
    let toggle = rig.rect(("files-tree-toggle", "files"));
    rig.click(toggle);
    rig.frame();
    assert!(!rig.state().show_tree);
    assert!(rect_of(("files-tree", "files".to_owned())).is_none());
    assert!(!rig.app.ui_store().read_flag(SHOW_TREE_STORAGE_KEY, true), "stored for the next pane");
    drop_files_pane(&mut rig.app, "files");
    rig.frame();
    assert!(!rig.state().show_tree, "a new pane takes the reader's choice");
    set_show_tree_preference(&mut rig.app, "files", true);
    rig.frame();
    assert!(rect_of(("files-tree", "files".to_owned())).is_some());
}

#[test]
fn folds_the_tree_for_a_request_that_opened_the_pane_without_changing_the_stored_choice() {
    let mut rig = sample();
    let mut request = rig.request(&rig.path("notes.txt"), None, 1);
    request.collapse_tree = true;
    rig.props.open_request = Some(request);
    rig.frame();
    assert!(!rig.state().show_tree);
    assert!(rig.app.ui_store().read_flag(SHOW_TREE_STORAGE_KEY, true));
}

#[test]
fn keeps_the_tree_as_it_was_for_a_request_made_while_the_pane_was_open() {
    let mut rig = sample();
    loaded(&mut rig);
    rig.props.open_request = Some(rig.request(&rig.path("notes.txt"), None, 1));
    rig.frame();
    assert!(rig.state().show_tree);
}

#[test]
fn the_tree_steps_aside_when_the_pane_is_too_narrow_for_the_two() {
    let mut rig = sample();
    rig.size = (320.0, 500.0);
    loaded(&mut rig);
    rig.settle(20);
    assert!(!rig.state().can_fit_tree);
    assert!(rect_of(("files-tree", "files".to_owned())).is_some(), "with no file the tree has the pane");
    let r = rig.row("notes.txt");
    rig.click(r);
    rig.until("the file", |x| x.file_key().is_some());
    assert!(rect_of(("files-tree", "files".to_owned())).is_none(), "with one, the file does");
}

// ---- previewers

#[test]
fn draws_an_html_page_from_its_markup_without_running_it() {
    let mut rig = sample();
    rig.write("site/index.html", "<!doctype html><html><head><title>T</title><script>boom()</script></head><body><h1 id=top>Hello</h1><a href=\"next.html#s\">next</a></body></html>");
    rig.write("site/next.html", "<p>next</p>");
    rig.frame();
    loaded(&mut rig);
    let page = rig.state().active_page.clone().unwrap();
    let file = Location::new(&None, rig.path("site/index.html"));
    show_file(&mut rig.app, "files", &page, file.clone(), false);
    let key = file.key();
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { .. })));
    // Headless, the same markup is drawn as a document.
    assert!(rect_of(format!("html-scroll/{key}").as_str()).is_some());
    mewrk_pane_files::viewer::open_html_link(&mut rig.app, "files", "site/index.html", "next.html#s");
    let target = rig.key("site/next.html");
    rig.until("the linked page", |x| x.file_key().as_deref() == Some(target.as_str()));
}

#[test]
fn tabulates_delimited_text() {
    let mut rig = sample();
    rig.write("t.csv", "name,count\na,1\nb,2\n");
    rig.frame();
    loaded(&mut rig);
    let r = rig.row("t.csv");
    rig.click(r);
    let key = rig.key("t.csv");
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { .. })));
    assert!(rect_of(("csv-scroll", key)).is_some());
}

#[test]
fn reads_a_notebook_the_way_jupyter_shows_it() {
    let mut rig = sample();
    rig.write("n.ipynb", r##"{"nbformat":4,"cells":[{"cell_type":"markdown","source":["# T"]},{"cell_type":"code","execution_count":1,"source":["1+1"],"outputs":[{"output_type":"execute_result","execution_count":1,"data":{"text/plain":["2"]}}]}]}"##);
    rig.frame();
    loaded(&mut rig);
    let r = rig.row("n.ipynb");
    rig.click(r);
    let key = rig.key("n.ipynb");
    rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { .. })));
    assert!(rect_of(format!("doc-scroll/{key}").as_str()).is_some());
}

#[test]
fn reads_a_pdf_a_recording_and_a_font_as_bytes_never_as_text() {
    let mut rig = sample();
    rig.write("a.pdf", b"%PDF-1.4 nope");
    let wav = mewrk_pane_files::audio::encode_wav(&mewrk_pane_files::audio::DecodedAudio { samples: vec![0.0; 800], channels: 1, rate: 8000 });
    rig.write("a.wav", wav);
    rig.write("a.ttf", b"not a font");
    rig.frame();
    loaded(&mut rig);
    for name in ["a.pdf", "a.wav", "a.ttf"] {
        let r = rig.row(name);
        rig.click(r);
        let key = rig.key(name);
        rig.until("the bytes", |x| matches!(x.state().media.get(&key), Some(MediaState::Ready(_))));
        assert!(!rig.state().viewers.contains_key(&key), "{name} was not read as text");
    }
}

#[test]
fn sets_a_real_woff2_font_from_its_own_glyphs() {
    let mut rig = sample();
    rig.write("Fraunces.woff2", include_bytes!("fixtures/fraunces-latin-600-normal.woff2"));
    rig.frame();
    loaded(&mut rig);
    let r = rig.row("Fraunces.woff2");
    rig.click(r);
    let key = rig.key("Fraunces.woff2");
    rig.until("the preview", |_| rect_of(("font-scroll", key.as_str())).is_some());
    rig.until("the samples on screen", |x| x.images_on_screen() > 0);
    assert!(!rig.state().viewers.contains_key(&key), "never read as text");
}

#[test]
fn says_so_when_a_woff2_cannot_be_read() {
    let mut rig = sample();
    rig.write("broken.woff2", b"wOF2 this is not a font at all");
    rig.frame();
    loaded(&mut rig);
    let r = rig.row("broken.woff2");
    rig.click(r);
    let key = rig.key("broken.woff2");
    rig.until("the bytes", |x| matches!(x.state().media.get(&key), Some(MediaState::Ready(_))));
    rig.settle(100);
    assert!(rect_of(("font-scroll", key.as_str())).is_none());
}

/// A real animated GIF, encoded here, of `frames` frames that loop forever.
fn animated_gif(frames: u8) -> Vec<u8> {
    use image::codecs::gif::{GifEncoder, Repeat};
    let mut out = Vec::new();
    {
        let mut encoder = GifEncoder::new(&mut out);
        encoder.set_repeat(Repeat::Infinite).unwrap();
        for i in 0..frames {
            let pixels = image::RgbaImage::from_pixel(6, 6, image::Rgba([i * 40, 0, 200, 255]));
            encoder.encode_frame(image::Frame::from_parts(pixels, 0, 0, image::Delay::from_numer_denom_ms(30, 1))).unwrap();
        }
    }
    out
}

#[test]
fn animates_a_real_gif_while_it_is_on_screen_and_lets_go_of_it_with_the_cache() {
    let mut rig = sample();
    rig.write("spin.gif", animated_gif(4));
    rig.write("still.gif", {
        let mut out = Vec::new();
        image::RgbaImage::from_pixel(6, 6, image::Rgba([9, 9, 9, 255])).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Gif).unwrap();
        out
    });
    rig.frame();
    loaded(&mut rig);
    for (name, moving) in [("spin.gif", true), ("still.gif", false)] {
        let r = rig.row(name);
        rig.click(r);
        let key = rig.key(name);
        rig.until("the picture on screen", |x| x.images_on_screen() > 0);
        let source = match rig.state().media.get(&key) {
            Some(MediaState::Ready(source)) => source.clone(),
            _ => panic!("the bytes of {name}"),
        };
        rig.until("the decode to settle", |x| {
            x.frame();
            mewrk_pane_files::images::is_animating(&source) == moving
        });
        rig.settle(30);
        assert_eq!(mewrk_pane_files::images::is_animating(&source), moving, "{name}");
    }
}

#[test]
fn closes_the_active_tab_with_the_delete_key_as_upstream_does() {
    let mut rig = sample();
    loaded(&mut rig);
    let plus = rig.rect(("files-add-page", "files"));
    rig.click(plus);
    rig.frame();
    assert_eq!(rig.state().pages.len(), 2);
    let active = rig.state().active_page.clone().unwrap();
    let tab = rig.rect(hash_key(("page-tab", "files/files", active.as_str())));
    rig.click(tab);
    rig.frame();
    rig.press_key(Key::Delete);
    rig.frame();
    assert_eq!(rig.state().pages.len(), 1);
    assert_ne!(rig.state().active_page.as_deref(), Some(active.as_str()), "it went with its page");
}

#[test]
fn points_at_a_video_it_cannot_play_instead_of_calling_it_binary() {
    let mut rig = sample();
    rig.write("clip.mp4", b"\0\0\0");
    rig.frame();
    loaded(&mut rig);
    let r = rig.row("clip.mp4");
    rig.click(r);
    let key = rig.key("clip.mp4");
    rig.until("the page", |x| x.file_key().as_deref() == Some(key.as_str()));
    rig.frame();
    assert!(!rig.state().viewers.contains_key(&key));
    assert!(rect_of(("reveal", key.as_str())).is_some());
    let _ = previews::notice("");
}

// ---- fences: answers landing out of order

mod fences {
    use super::*;

    /// A listing asked for again while the first read is out is answered once: whichever lands last for the newest
    /// request. Nothing is left in flight, and the opening that waited is honoured.
    #[test]
    fn a_refresh_rereads_what_it_threw_away() {
        let mut rig = sample();
        loaded(&mut rig);
        let src = rig_key(&mut rig, "src");
        let src_path = rig.path("src");
        load_directory(&mut rig.app, "files", &None, &src_path);
        assert!(matches!(rig.state().listings.get(&src), Some(DirectoryState::Loading)));
        refresh(&mut rig.app, "files");
        assert!(!rig.state().listings.contains_key(&src), "a cached Loading nobody is driving is gone");
        let page = rig.state().active_page.clone().unwrap();
        set_directory_open(&mut rig.app, "files", &page, &None, &src_path, true);
        rig.until("the listing", |x| matches!(x.state().listings.get(&src), Some(DirectoryState::Ready(_))));
        assert_eq!(rig.tree().len(), 5);
    }

    #[test]
    fn keeps_the_newer_read_when_two_reads_of_a_file_are_out() {
        let mut rig = sample();
        loaded(&mut rig);
        let path = rig.path("notes.txt");
        load_file(&mut rig.app, "files", &None, &path);
        rig.write("notes.txt", "newer\n");
        load_file(&mut rig.app, "files", &None, &path);
        let key = rig.key("notes.txt");
        rig.until("the read", |x| matches!(x.state().viewers.get(&key), Some(ViewerState::Ready { .. })));
        rig.settle(100);
        assert!(matches!(rig.state().viewers.get(&key), Some(ViewerState::Ready { content, .. }) if &**content == "newer\n"));
    }
}

// ---------------------------------------------------------------- accessibility

#[test]
fn the_tree_says_what_each_row_is_and_how_deep_and_every_control_has_a_name() {
    use accesskit::Role;
    let mut rig = sample();
    loaded(&mut rig);
    open_folder(&mut rig, "src");
    rig.until("the folder's rows", |r| r.tree().iter().any(|n| n.contains("App.tsx")));
    let tree = rig.a11y_tree();
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    a11y_is(&tree, Role::Tree, "Files");
    let folder = a11y_is(&tree, Role::TreeItem, "src");
    assert_eq!((folder.level(), folder.is_expanded()), (Some(1), Some(true)));
    let file = a11y_is(&tree, Role::TreeItem, "App.tsx");
    assert_eq!((file.level(), file.is_expanded()), (Some(2), None));
    a11y_is(&tree, Role::TextInput, "Filter files");
    let address = tree.iter().map(|(_, node)| node).find(|node| a11y_name(node).starts_with("Location: ")).expect("the address says it can be edited");
    assert_eq!(address.role(), Role::Button);
    assert_eq!(a11y_is(&tree, Role::Button, "Collapse file tree").is_expanded(), Some(true));
    assert_eq!(a11y_is(&tree, Role::Button, "File actions").has_popup(), Some(accesskit::HasPopup::Menu));
    let stops = rig.a11y_tab_stops();
    assert!(stops.len() > 5, "{stops:#?}");
    assert!(stops.iter().all(|stop| !stop.ends_with("\"\"") && !stop.starts_with("Window")), "{stops:#?}");
    set_a11y_active(false);
}

#[test]
fn an_open_file_names_its_tools_and_a_file_tree_row_that_is_open_is_the_current_one() {
    use accesskit::Role;
    let mut rig = sample();
    loaded(&mut rig);
    let r = rig.row("notes.txt");
    rig.click(r);
    rig.until("the file", |x| x.file_key().is_some());
    rig.settle(50);
    let tree = rig.a11y_tree();
    assert!(a11y_is(&tree, Role::TreeItem, "notes.txt").aria_current().is_some());
    a11y_is(&tree, Role::Button, "Reload file");
    a11y_is(&tree, Role::Button, "Reveal in file tree");
    a11y_is(&tree, Role::Button, "Copy file contents");
    a11y_is(&tree, Role::Button, "Close file");
    assert_eq!(a11y_is(&tree, Role::ScrollView, "notes.txt").children().len(), 1, "the file's text is under its name");
    assert!(tree.iter().any(|(_, node)| node.role() == Role::Label && node.value() == Some("plain\n")), "and a reader gets it");
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn a_delete_asks_in_an_alert_dialog_and_a_failed_action_is_an_alert() {
    use accesskit::Role;
    let mut rig = sample();
    loaded(&mut rig);
    let location = Location::new(&None, rig.path("notes.txt"));
    rig.state().confirm_delete = Some(ConfirmDelete { machine: None, path: location.path.clone(), busy: false });
    rig.state().action_error = Some("no".to_owned());
    let tree = rig.a11y_tree();
    a11y_is(&tree, Role::AlertDialog, "Confirm delete");
    assert!(tree.iter().any(|(_, node)| node.role() == Role::Alert));
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn a_search_is_a_list_of_options() {
    use accesskit::Role;
    let mut rig = sample();
    loaded(&mut rig);
    let filter = rect_of(hash_key(("field", ("files-filter", "files")))).expect("the filter");
    rig.click(filter);
    rig.type_text("App");
    rig.until("the matches", |r| matches!(r.state().search, SearchState::Ready { .. }));
    let tree = rig.a11y_tree();
    a11y_is(&tree, Role::ListBox, "Matching files");
    assert!(tree.iter().any(|(_, node)| node.role() == Role::ListBoxOption && a11y_name(node).contains("App.tsx")), "{tree:#?}");
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    set_a11y_active(false);
}
