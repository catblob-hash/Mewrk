//! What the test files share: fixtures in upstream's JSON shape, and a headless rig that drives views on a real `App`.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use chrono::{SecondsFormat, TimeZone, Utc};
use fluent::el::*;
use fluent::ui::{Clipboard, Input, Key, KeyPress, Rect, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::{App, StartOptions};
use mewrk_pane_tasks::context::Ctx;
use serde_json::Value;

pub type E = El<App>;

pub fn ctx(value: Value) -> Ctx {
    serde_json::from_value(value.clone()).unwrap_or_else(|e| panic!("{e}: {value}"))
}

pub fn iso(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms).single().unwrap().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Default)]
pub struct MemClipboard(pub Rc<std::cell::RefCell<String>>);

impl Clipboard for MemClipboard {
    fn get(&mut self) -> Option<String> {
        Some(self.0.borrow().clone())
    }
    fn set(&mut self, text: &str) {
        *self.0.borrow_mut() = text.to_owned();
    }
}

/// A real application on a temporary data folder: handlers get what they get in the app.
pub fn start_app(dir: &std::path::Path) -> App {
    let dir = dir.to_owned();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Upstream's fixtures say what they expect in Chinese.
    unsafe { std::env::set_var("MEWRK_LANG", "zh-CN") };
    let app = App::start(StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: dir, resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
    mewrk_client::i18n::set_english(false);
    app
}

pub struct Rig {
    pub scene: Scene,
    pub text: TextCtx,
    pub mem: UiMemory,
    pub input: Input,
    pub clipboard: MemClipboard,
    pub time: f32,
    pub size: (f32, f32),
    pub last_wake: Option<f32>,
    pub app: App,
    dir: std::path::PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mewrk-pane-tasks-{tag}-{}", std::process::id()));
        Self { scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: MemClipboard::default(), time: 0.0, size: (900.0, 800.0), last_wake: None, app: start_app(&dir), dir }
    }

    pub fn frame(&mut self, view: &mut dyn FnMut(&mut App) -> E) {
        self.app.tick();
        self.time += 0.016;
        set_window_size(self.size.0, self.size.1);
        let root = view(&mut self.app);
        self.scene.clear();
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, &mut self.clipboard, 1.0, self.size, 0.016, self.time);
        render(root, &mut self.app, &mut ui);
        self.last_wake = ui.wake_in;
        self.input.end_frame();
    }

    /// Enough frames for layout that depends on last frame's rectangles to settle.
    pub fn settle(&mut self, view: &mut dyn FnMut(&mut App) -> E) {
        for _ in 0..3 {
            self.frame(view);
        }
    }

    pub fn click_at(&mut self, view: &mut dyn FnMut(&mut App) -> E, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame(view);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(view);
        self.input.down = false;
        self.input.released = true;
        self.frame(view);
        self.frame(view);
    }

    pub fn click(&mut self, view: &mut dyn FnMut(&mut App) -> E, key: impl std::hash::Hash) {
        let r = rect_of(key).expect("the element is on screen");
        self.click_at(view, r.x + r.w * 0.5, r.y + r.h * 0.5);
    }

    pub fn hover(&mut self, view: &mut dyn FnMut(&mut App) -> E, key: impl std::hash::Hash) {
        let r = rect_of(key).expect("the element is on screen");
        self.input.pointer = Some((r.x + r.w * 0.5, r.y + r.h * 0.5));
        self.frame(view);
        self.frame(view);
    }

    pub fn press_key(&mut self, view: &mut dyn FnMut(&mut App) -> E, key: Key, cmd: bool) {
        self.input.keys.push(KeyPress { key, shift: false, cmd, alt: false, ctrl: false });
        self.frame(view);
    }

    /// Runs frames until `done`, for what comes back from the host's threads.
    pub fn until(&mut self, view: &mut dyn FnMut(&mut App) -> E, what: &str, done: &mut dyn FnMut(&mut App) -> bool) {
        let start = std::time::Instant::now();
        while !done(&mut self.app) {
            assert!(start.elapsed().as_secs() < 20, "timed out waiting for {what}");
            self.frame(view);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.frame(view);
    }
}

pub fn on_screen(key: impl std::hash::Hash) -> bool {
    rect_of(key).is_some()
}

pub fn near(r: Rect, x: f32, y: f32, w: f32, h: f32) {
    assert!((r.x - x).abs() < 1.0 && (r.y - y).abs() < 1.0 && (r.w - w).abs() < 1.0 && (r.h - h).abs() < 1.0, "{r:?} is not ({x},{y},{w},{h})");
}

pub type Calls = Rc<std::cell::RefCell<Vec<String>>>;

pub fn calls() -> Calls {
    Rc::default()
}

pub fn counter() -> Rc<Cell<usize>> {
    Rc::default()
}

pub fn map<V>(entries: Vec<(&str, V)>) -> HashMap<String, V> {
    entries.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

// ---------------------------------------------------------------- the accessibility tree

pub type Tree = Vec<(accesskit::NodeId, accesskit::Node)>;

impl Rig {
    pub fn a11y_tree(&mut self, view: &mut dyn FnMut(&mut App) -> E) -> Tree {
        set_a11y_active(true);
        self.settle(view);
        take_a11y_update().expect("a client gets the tree").nodes
    }
}

pub fn said<'a>(tree: &'a Tree, words: &str) -> &'a accesskit::Node {
    tree.iter().map(|(_, node)| node).find(|node| node.label() == Some(words) || (node.role() == Role::Label && node.value() == Some(words))).unwrap_or_else(|| panic!("nothing says {words:?}: {:?}", tree.iter().filter_map(|(_, n)| n.label().or(n.value())).collect::<Vec<_>>()))
}

/// Everything a reader can press or type into with no name to call it by.
pub fn nameless(tree: &Tree) -> Vec<String> {
    tree.iter()
        .filter(|(_, node)| matches!(node.role(), Role::Button | Role::Link | Role::TextInput | Role::MultilineTextInput | Role::Tab | Role::Switch | Role::CheckBox | Role::MenuItem | Role::ComboBox) || node.supports_action(accesskit::Action::Click))
        .filter(|(_, node)| node.label().is_none_or(str::is_empty) && node.labelled_by().is_empty() && (matches!(node.role(), Role::TextInput | Role::MultilineTextInput) || node.value().is_none_or(str::is_empty)))
        .map(|(id, node)| format!("{:?} {id:?}", node.role()))
        .collect()
}
