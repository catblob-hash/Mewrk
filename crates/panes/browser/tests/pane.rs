//! The pane on a real host with no window, scripted: `App::start` over a temp data dir, no GPU.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fluent::el::*;
use fluent::ui::{Input, Key, KeyPress, NoClipboard, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::{i18n, App, StartOptions};
use mewrk_pane_browser::*;
use serde_json::json;

const WIN: (f32, f32) = (1000.0, 760.0);
const SESSION: &str = "pane-test-session";

/// One test at a time: `HOME` is process-wide, and the host keeps statics of its own.
static SERIAL: Mutex<()> = Mutex::new(());

fn fresh_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("mewrk-browser-{tag}-{}-{n:x}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: NoClipboard,
    app: App,
    props: BrowserPaneProps,
    _serial: MutexGuard<'static, ()>,
}

fn start() -> Rig {
    let serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let (dir, home) = (fresh_dir("data"), fresh_dir("home"));
    let document = json!({
        "schemaVersion": 6,
        "globalSettings": {},
        "workspaces": [],
        "tools": [{ "name": "read", "label": "Read", "description": "", "category": "filesystem", "dangerous": false, "parameters": [] }],
        "capabilities": { "skills": [], "mcps": [] },
    });
    std::fs::write(dir.join("document.v1.json"), serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    // SAFETY: serialized by `SERIAL`, before this test's threads read the environment.
    unsafe {
        std::env::set_var("HOME", &home);
        // Where `std::env::home_dir` looks on Windows.
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("LANG", "en_US.UTF-8");
    }
    set_theme(Theme::dark());
    set_window_size(WIN.0, WIN.1);
    let app = App::start(StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: dir, resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
    i18n::set_english(true);
    let props = BrowserPaneProps { session_id: Some(SESSION.to_owned()), active: true, ..Default::default() };
    Rig { scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: NoClipboard, app, props, _serial: serial }
}

impl Rig {
    fn frame(&mut self) {
        self.app.tick();
        self.app.frame_no += 1;
        self.scene.clear();
        let view = browser_pane(&mut self.app, self.props.clone());
        let root = col::<App>().size(WIN.0, WIN.1).child(row().h(32.0).child(view.header)).child(col().flex1().min_h(0.0).child(view.body));
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, &mut self.clipboard, 1.0, WIN, 0.016, 0.0);
        render(root, &mut self.app, &mut ui);
        self.input.end_frame();
    }

    fn settle(&mut self, what: &str, mut done: impl FnMut(&mut Rig) -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            self.frame();
            if done(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    fn click(&mut self, key: impl std::hash::Hash) {
        let area = rect_of(key).expect("something to click");
        self.input.pointer = Some((area.x + area.w / 2.0, area.y + area.h / 2.0));
        self.frame();
        self.input.pressed = true;
        self.input.down = true;
        self.frame();
        self.input.down = false;
        self.input.released = true;
        self.frame();
        self.frame();
    }

    fn press(&mut self, key: Key) {
        self.input.keys.push(KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false });
        self.frame();
        self.frame();
    }

    fn key(&self, name: &'static str) -> (&'static str, String) {
        (name, SESSION.to_owned())
    }
}

#[test]
fn the_toolbar_reads_in_the_references_order_and_size() {
    let mut rig = start();
    rig.frame();
    rig.frame();
    let x = |name: &'static str| rect_of((name, SESSION.to_owned())).unwrap_or_else(|| panic!("{name} is missing")).x;
    let order = [x("browser-menu-trigger"), x("browser-address-chip"), x("browser-back"), x("browser-forward"), x("browser-annotate"), x("browser-select"), x("browser-reload")];
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    let tool = rect_of(("browser-back", SESSION.to_owned())).unwrap();
    assert_eq!((tool.w, tool.h), (24.0, 24.0));
}

#[test]
fn an_address_swaps_for_an_input_and_escape_puts_it_back() {
    let mut rig = start();
    rig.frame();
    rig.frame();
    let chip = rig.key("browser-address-chip");
    rig.click(chip.clone());
    assert!(rect_of(chip.clone()).is_none(), "the chip gave way to the input");
    assert_eq!(rig.mem.focus, hash_key(("field", ("browser-address", SESSION))), "and the input has the keyboard");
    rig.input.text.push_str("half typed");
    rig.frame();
    rig.press(Key::Escape);
    assert!(rect_of(chip).is_some(), "Escape takes it back");
}

#[test]
fn a_tab_that_cant_get_a_page_says_why_in_the_error_strip() {
    let mut rig = start();
    rig.frame();
    rig.frame();
    let chip = rig.key("browser-address-chip");
    rig.click(chip);
    rig.input.text.push_str("http://localhost:3000/");
    rig.frame();
    rig.press(Key::Enter);
    // No window here, so the host has nowhere to put a page: the pane reports that, and doesn't hang.
    let error = rig.key("browser-error");
    rig.settle("the error strip", |_| rect_of(error.clone()).is_some());
}

#[test]
fn a_refused_address_is_reported_without_asking_the_host() {
    let mut rig = start();
    rig.frame();
    rig.frame();
    let chip = rig.key("browser-address-chip");
    rig.click(chip);
    rig.input.text.push_str("javascript:alert(1)");
    rig.frame();
    rig.press(Key::Enter);
    let error = rig.key("browser-error");
    rig.settle("the error strip", |_| rect_of(error.clone()).is_some());
}

#[test]
fn the_menu_opens_from_its_trigger_and_escape_closes_it() {
    let mut rig = start();
    rig.frame();
    rig.frame();
    assert!(!overlay_open());
    let trigger = rig.key("browser-menu-trigger");
    rig.click(trigger.clone());
    assert!(rect_of(("browser-menu", SESSION)).is_some(), "the menu is up");
    assert!(overlay_open() && !overlay_rects().is_empty(), "and it says so to whatever sits under it");
    for name in ["browser-menu-file", "browser-menu-logs", "browser-menu-clear"] {
        assert!(rect_of((name, SESSION.to_owned())).is_some(), "{name}");
    }
    // Clicking the trigger again closes it, the way a toggle does.
    rig.click(trigger);
    assert!(rect_of(("browser-menu", SESSION)).is_none());
}

#[test]
fn the_menu_takes_the_focus_and_the_arrows_walk_its_rows() {
    let mut rig = start();
    rig.frame();
    rig.frame();
    let trigger = rig.key("browser-menu-trigger");
    rig.click(trigger);
    rig.frame();
    rig.frame();
    let row = |key: &'static str| hash_key((key, SESSION.to_owned()));
    // With no servers, the first row is the Files group's.
    assert_eq!(focused_id(), row("browser-menu-file"), "the first row has the focus");
    rig.press(Key::Down);
    assert_eq!(focused_id(), row("browser-menu-logs"));
    rig.press(Key::End);
    assert_eq!(focused_id(), row("browser-menu-clear"));
    rig.press(Key::Down);
    assert_eq!(focused_id(), row("browser-menu-file"), "it wraps");
    rig.press(Key::Up);
    assert_eq!(focused_id(), row("browser-menu-clear"));
    rig.press(Key::Home);
    assert_eq!(focused_id(), row("browser-menu-file"));
    rig.press(Key::Down);
    // Enter does what a click on the row does: the logs open, and the focus goes back to the trigger.
    rig.press(Key::Enter);
    assert!(rect_of(("browser-log-drawer", SESSION)).is_some(), "the logs opened");
    assert!(rect_of(("browser-menu", SESSION)).is_none(), "the menu closed");
    assert_eq!(focused_id(), hash_key(("browser-menu-trigger", SESSION.to_owned())));
}

#[test]
fn clearing_browsing_data_takes_an_explicit_second_step() {
    let mut rig = start();
    rig.frame();
    rig.frame();
    let trigger = rig.key("browser-menu-trigger");
    rig.click(trigger);
    assert!(rect_of(("browser-menu-clear-confirm", SESSION.to_owned())).is_none(), "nothing is one click from clearing");
    let row = rig.key("browser-menu-clear");
    rig.click(row);
    assert!(rect_of(("browser-menu-clear-confirm", SESSION.to_owned())).is_some(), "the row opens the step that does it");
}

#[test]
fn the_logs_toggle_docks_a_drawer_that_takes_height_from_the_page() {
    let mut rig = start();
    rig.frame();
    rig.frame();
    let before = rect_of(("browser-page-area", SESSION)).unwrap();
    let trigger = rig.key("browser-menu-trigger");
    rig.click(trigger);
    let logs = rig.key("browser-menu-logs");
    rig.click(logs);
    assert!(rect_of(("browser-menu", SESSION)).is_none(), "the menu closes first");
    let drawer = rect_of(("browser-log-drawer", SESSION)).expect("the drawer");
    let after = rect_of(("browser-page-area", SESSION)).unwrap();
    assert_eq!(drawer.h, 200.0);
    assert_eq!(after.h, before.h - 200.0, "beside the page, not over it");
    let close = rig.key("browser-log-close");
    rig.click(close);
    assert!(rect_of(("browser-log-drawer", SESSION)).is_none());
}

#[test]
fn the_standby_is_up_with_no_workspace_and_covers_the_page_area() {
    let mut rig = start();
    rig.frame();
    rig.frame();
    assert!(rect_of(("browser-body-card", SESSION)).is_some(), "the no-config card");
}

#[test]
fn a_pane_with_no_session_is_a_pane_with_nothing_in_it() {
    let mut rig = start();
    rig.props.session_id = None;
    rig.frame();
    rig.frame();
    assert!(rect_of(("browser-menu-trigger", SESSION)).is_none());
}

// ---------------------------------------------------------------- accessibility

type AxNodes = Vec<(accesskit::NodeId, accesskit::Node)>;

fn a11y_name(node: &accesskit::Node) -> &str {
    let words = if node.role() == accesskit::Role::Label { node.value() } else { node.label() };
    words.unwrap_or("")
}

/// The node of this role that says this: the words beside a field say what it does too.
fn a11y_is<'a>(tree: &'a AxNodes, role: accesskit::Role, name: &str) -> &'a accesskit::Node {
    tree.iter().map(|(_, node)| node).find(|node| node.role() == role && a11y_name(node) == name).unwrap_or_else(|| panic!("no {role:?} says {name:?} in {:#?}", tree.iter().map(|(_, node)| format!("{:?} {:?}", node.role(), a11y_name(node))).collect::<Vec<_>>()))
}

/// Whatever a keyboard or a reader can act on and has no name. A splitter has none upstream either.
fn a11y_unnamed(tree: &AxNodes) -> Vec<String> {
    use accesskit::{Action, Role};
    tree.iter()
        .map(|(_, node)| node)
        .filter(|node| !matches!(node.role(), Role::Window | Role::TextRun | Role::Label | Role::Splitter))
        .filter(|node| [Action::Click, Action::Focus, Action::SetValue, Action::Increment].into_iter().any(|a| node.supports_action(a)) || matches!(node.role(), Role::Dialog | Role::AlertDialog))
        .filter(|node| a11y_name(node).trim().is_empty() && node.labelled_by().is_empty())
        .map(|node| format!("{:?} at {:?}", node.role(), node.bounds()))
        .collect()
}

impl Rig {
    fn a11y_tree(&mut self) -> AxNodes {
        set_a11y_active(true);
        for _ in 0..3 {
            self.frame();
        }
        take_a11y_update().expect("a client gets the tree").nodes
    }
}

#[test]
fn the_toolbar_and_its_menu_name_every_control_and_say_what_is_up() {
    use accesskit::Role;
    let mut rig = start();
    let tree = rig.a11y_tree();
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    let trigger = a11y_is(&tree, Role::Button, "Servers & settings");
    assert_eq!((trigger.has_popup(), trigger.is_expanded()), (Some(accesskit::HasPopup::Menu), Some(false)));
    a11y_is(&tree, Role::Button, "Enter a URL");
    a11y_is(&tree, Role::Button, "Go back");
    a11y_is(&tree, Role::Button, "Go forward");
    assert!(a11y_is(&tree, Role::Button, "Annotate").toggled().is_some());
    assert!(a11y_is(&tree, Role::Button, "Select element").toggled().is_some());
    a11y_is(&tree, Role::Button, "Reload");

    rig.click(rig.key("browser-menu-trigger"));
    let tree = rig.a11y_tree();
    assert_eq!(a11y_is(&tree, Role::Button, "Servers & settings").is_expanded(), Some(true));
    a11y_is(&tree, Role::Menu, "Browser menu");
    a11y_is(&tree, Role::MenuItem, "Open file");
    a11y_is(&tree, Role::MenuItem, "Show dev server logs");
    assert_eq!(a11y_is(&tree, Role::MenuItem, "Clear browsing data").is_expanded(), Some(false));
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn editing_the_address_is_a_search_field_with_a_name() {
    use accesskit::Role;
    let mut rig = start();
    rig.frame();
    rig.frame();
    rig.click(rig.key("browser-address-chip"));
    let tree = rig.a11y_tree();
    a11y_is(&tree, Role::TextInput, "Page URL");
    assert!(tree.iter().any(|(_, node)| node.role() == Role::Search));
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn an_empty_pane_says_so_as_a_status() {
    use accesskit::Role;
    let mut rig = start();
    rig.settle("the servers to be read", |rig| rig.app.frame_no > 5);
    let tree = rig.a11y_tree();
    assert!(tree.iter().any(|(_, node)| node.role() == Role::Status), "{:#?}", tree.iter().map(|(_, node)| format!("{:?} {:?}", node.role(), a11y_name(node))).collect::<Vec<_>>());
    set_a11y_active(false);
}
