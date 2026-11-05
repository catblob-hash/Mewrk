//! A pane on the real host, driven with scripted input and no GPU: `App::start` with a temp data dir, a temp folder for
//! files, and frames run by hand. Every read goes through the host's own `browse_*` commands.
#![allow(dead_code)]

use fluent::el::*;
use fluent::ui::{Clipboard, Input, Key, KeyPress, Rect, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::{App, StartOptions};
use mewrk_pane_files::*;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct TestClipboard(pub Option<String>);

impl Clipboard for TestClipboard {
    fn get(&mut self) -> Option<String> {
        self.0.clone()
    }
    fn set(&mut self, text: &str) {
        self.0 = Some(text.to_owned());
    }
}

pub struct Rig {
    /// What `Ui::time` counts from: a pointer resting on a menu row opens its submenu by it.
    epoch: Instant,
    pub app: App,
    pub root: PathBuf,
    pub props: FilesPaneProps,
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    pub input: Input,
    pub clipboard: TestClipboard,
    pub size: (f32, f32),
    _data: TempDir,
    _files: TempDir,
}

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("mewrk-files-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn start_app(data: &std::path::Path) -> App {
    // Before anything reads it: `App::start` resolves the language from this, and it is one flag for the whole process.
    static LANGUAGE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the first thing any test does, before a host thread exists to read the environment.
    LANGUAGE.call_once(|| unsafe { std::env::set_var("MEWRK_LANG", "en") });
    App::start(StartOptions {
        identifier: "dev.mewrk.tests".to_owned(),
        version: "0.0.0".to_owned(),
        data_dir: data.to_owned(),
        resource_dir: None,
        wake: Arc::new(|| {}),
        exit: Arc::new(|_| {}),
        show_window: Arc::new(|| {}),
    })
    .map_err(|e| format!("{e:?}"))
    .expect("the host starts")
}

impl Rig {
    pub fn new() -> Self {
        let data = TempDir::new("data");
        let files = TempDir::new("root");
        let app = start_app(&data.0);
        let root = files.0.clone();
        let props = FilesPaneProps {
            pane_id: "files".to_owned(),
            workspaces: vec![FilesPaneWorkspace { number: 1, machine: None, path: root.to_string_lossy().into_owned() }],
            ssh_machines: Vec::new(),
            host_windows: false,
            active: true,
            open_request: None,
            on_open_request_handled: None,
            on_mention: None,
        };
        Self { epoch: Instant::now(), app, root, props, scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: TestClipboard::default(), size: (900.0, 700.0), _data: data, _files: files }
    }

    pub fn write(&self, relative: &str, content: impl AsRef<[u8]>) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    pub fn path(&self, relative: &str) -> String {
        self.root.join(relative).to_string_lossy().into_owned()
    }

    pub fn frame(&mut self) {
        self.app.tick();
        self.scene.clear();
        set_window_size(self.size.0, self.size.1);
        let view = files_pane(&mut self.app, self.props.clone());
        // As the shell's `side_pane` lays it out.
        let root = col::<App>().size(self.size.0, self.size.1).child(row::<App>().h(32.0).shrink0().items_center().child(view.header)).child(col::<App>().flex1().min_w(0.0).min_h(0.0).clip().child(view.body));
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, &mut self.clipboard, 1.0, self.size, 0.016, self.epoch.elapsed().as_secs_f32());
        render(root, &mut self.app, &mut ui);
        self.input.end_frame();
    }

    /// Runs frames until `done` says so: answers from the host and timers arrive when they arrive.
    pub fn until(&mut self, what: &str, done: impl Fn(&mut Self) -> bool) {
        let started = Instant::now();
        loop {
            self.frame();
            if done(self) {
                self.frame();
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "timed out waiting for {what}\ntext on screen: {}", self.state_summary());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn settle(&mut self, ms: u64) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(ms) {
            self.frame();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn state(&mut self) -> &mut pane::PaneState {
        pane::pane(&mut self.app, "files").expect("the pane is mounted")
    }

    /// Names in the tree, as a reader would read them top to bottom.
    pub fn tree(&mut self) -> Vec<String> {
        self.state().rows().iter().map(|r| match r {
            pane::TreeRow::Entry { entry, depth, .. } => format!("{}{}", "  ".repeat(*depth), entry.name),
            pane::TreeRow::Machine { name, .. } => format!("@{name}"),
            pane::TreeRow::Message { text, depth, .. } => format!("{}~{text}", "  ".repeat(*depth)),
        }).collect()
    }

    pub fn state_summary(&mut self) -> String {
        format!("{:?}", self.tree())
    }

    pub fn key(&self, relative: &str) -> String {
        browser::location_key(&None, &self.path(relative))
    }

    pub fn file_key(&mut self) -> Option<String> {
        self.state().active().and_then(|p| p.file.as_ref()).map(pane::Location::key)
    }

    pub fn rect(&self, key: impl std::hash::Hash) -> Rect {
        rect_of(key).expect("the element is on screen")
    }

    pub fn row(&self, relative: &str) -> Rect {
        self.rect(("files-row", "files", self.key(relative).as_str()))
    }

    pub fn click_at(&mut self, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame();
        self.input.pressed = true;
        self.input.down = true;
        self.frame();
        self.input.down = false;
        self.input.released = true;
        self.frame();
    }

    pub fn click(&mut self, r: Rect) {
        self.click_at(r.x + r.w * 0.5, r.y + r.h * 0.5);
    }

    pub fn right_click(&mut self, r: Rect) {
        self.input.pointer = Some((r.x + r.w * 0.5, r.y + r.h * 0.5));
        self.frame();
        self.input.right_pressed = true;
        self.frame();
        self.frame();
    }

    pub fn hover(&mut self, r: Rect) {
        self.input.pointer = Some((r.x + r.w * 0.5, r.y + r.h * 0.5));
        self.frame();
        self.frame();
    }

    pub fn press_key(&mut self, key: Key) {
        self.input.keys.push(KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false });
        self.frame();
        self.frame();
    }

    pub fn type_text(&mut self, text: &str) {
        self.input.text.push_str(text);
        self.frame();
        self.frame();
    }

    /// Selects everything in the focused field and types over it.
    pub fn replace_text(&mut self, text: &str) {
        self.input.keys.push(KeyPress { key: Key::Char('a'), shift: false, cmd: true, alt: false, ctrl: false });
        self.frame();
        self.type_text(text);
    }

    pub fn menu_labels(&mut self) -> Vec<String> {
        let Some(menu) = self.state().menu.clone() else { return Vec::new() };
        menu.sections.iter().flat_map(|s| s.items.iter().map(|i| i.label.clone())).collect()
    }

    /// What a menu row does, without a pointer: a menu's position is the portal's business and tested in fluent.
    pub fn choose(&mut self, label: &str) {
        let menu = self.state().menu.clone().expect("a menu is open");
        let item = menu.sections.iter().flat_map(|s| s.items.iter()).find(|i| i.label == label).unwrap_or_else(|| panic!("no menu row {label}")).clone();
        assert!(!item.disabled, "{label} is disabled");
        self.state().menu = None;
        (item.on_select.expect("a row with an action"))(&mut self.app);
        self.frame();
    }

    pub fn open_row_menu(&mut self, relative: &str) {
        let r = self.row(relative);
        self.right_click(r);
        assert!(self.state().menu.is_some(), "a menu opened on {relative}");
    }

    /// Glyph quads on screen, to tell whether something was painted at all.
    pub fn images_on_screen(&self) -> usize {
        self.scene.layers.iter().map(|l| l.images.len()).sum()
    }

    pub fn request(&mut self, path: &str, line: Option<u32>, nonce: u64) -> pane::FilesPaneOpenRequest {
        pane::FilesPaneOpenRequest { machine: None, path: path.to_owned(), line, nonce, collapse_tree: false, new_page: false, folder: false }
    }
}

pub fn handled_counter() -> (Rc<std::cell::Cell<u64>>, Rc<dyn Fn(&mut App, u64)>) {
    let seen = Rc::new(std::cell::Cell::new(0));
    let inner = seen.clone();
    (seen, Rc::new(move |_, nonce| inner.set(nonce)))
}

// ---------------------------------------------------------------- accessibility

pub type AxNodes = Vec<(accesskit::NodeId, accesskit::Node)>;

/// What a node is called: a label reads its value, everything else its label.
pub fn a11y_name(node: &accesskit::Node) -> &str {
    let words = if node.role() == accesskit::Role::Label { node.value() } else { node.label() };
    words.unwrap_or("")
}

/// The node of this role that says this: the words beside a field say what it does too.
pub fn a11y_is<'a>(tree: &'a AxNodes, role: accesskit::Role, name: &str) -> &'a accesskit::Node {
    tree.iter().map(|(_, node)| node).find(|node| node.role() == role && a11y_name(node) == name).unwrap_or_else(|| panic!("no {role:?} says {name:?} in {:#?}", tree.iter().map(|(_, node)| format!("{:?} {:?}", node.role(), a11y_name(node))).collect::<Vec<_>>()))
}

/// Whatever a keyboard or a reader can act on and has no name. A sweep of a surface asserts this is empty. A splitter has none upstream either.
pub fn a11y_unnamed(tree: &AxNodes) -> Vec<String> {
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
    /// The whole tree as the frame left it, with a client that has just arrived.
    pub fn a11y_tree(&mut self) -> AxNodes {
        set_a11y_active(true);
        for _ in 0..3 {
            self.frame();
        }
        take_a11y_update().expect("a client gets the tree").nodes
    }

    /// Walks the Tab order all the way round, and says what each stop is called. A stop the tree doesn't know lands the focus on the
    /// window, and reads as what is around it.
    pub fn a11y_tab_stops(&mut self) -> Vec<String> {
        let mut seen = Vec::new();
        let mut stops = Vec::new();
        for _ in 0..200 {
            self.press_key(Key::Tab);
            let focus = focused_id();
            if focus == 0 {
                break;
            }
            if seen.contains(&focus) {
                assert_eq!(focus, seen[0], "Tab goes round the whole pane, and this one sends it back early after {stops:#?}");
                break;
            }
            seen.push(focus);
            set_a11y_active(true);
            self.frame();
            self.frame();
            let update = take_a11y_update().expect("a client gets the tree");
            let node = update.nodes.iter().find(|(id, _)| *id == update.focus).map(|(_, node)| node).expect("the focus is in the tree");
            stops.push(format!("{:?} {:?}", node.role(), a11y_name(node)));
        }
        stops
    }
}
