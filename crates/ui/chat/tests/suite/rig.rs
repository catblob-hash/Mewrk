//! A view on the real host, driven with scripted input and no GPU: `App::start` over a temp data dir, frames run by hand.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use fluent::el::*;
use fluent::ui::{Clipboard, Input, Key, KeyPress, Rect, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::E;
use mewrk_client::{App, StartOptions};

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

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("mewrk-chat-{tag}-{}-{n}", std::process::id()));
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

pub struct Rig {
    pub app: App,
    pub scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    pub input: Input,
    pub clipboard: TestClipboard,
    pub size: (f32, f32),
    _data: TempDir,
}

impl Rig {
    pub fn new() -> Self {
        let data = TempDir::new("data");
        let app = start_app(&data.0);
        Self { app, scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: TestClipboard::default(), size: (1000.0, 800.0), _data: data }
    }

    /// One frame: posted jobs, the view, its handlers.
    pub fn frame(&mut self, view: &mut dyn FnMut(&mut App) -> E) {
        self.app.tick();
        self.scene.clear();
        set_window_size(self.size.0, self.size.1);
        set_theme(Theme::dark());
        let root = view(&mut self.app);
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, &mut self.clipboard, 1.0, self.size, 0.016, 0.0);
        render(root, &mut self.app, &mut ui);
        self.input.end_frame();
    }

    /// Enough frames for last frame's rectangles and state to settle.
    pub fn settle(&mut self, view: &mut dyn FnMut(&mut App) -> E) {
        for _ in 0..4 {
            self.frame(view);
        }
    }

    pub fn point(&mut self, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
    }

    pub fn press(&mut self, view: &mut dyn FnMut(&mut App) -> E, x: f32, y: f32) {
        self.point(x, y);
        self.frame(view);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(view);
    }

    pub fn release(&mut self, view: &mut dyn FnMut(&mut App) -> E, x: f32, y: f32) {
        self.point(x, y);
        self.input.down = false;
        self.input.released = true;
        self.frame(view);
    }

    pub fn click(&mut self, view: &mut dyn FnMut(&mut App) -> E, x: f32, y: f32) {
        self.press(view, x, y);
        self.release(view, x, y);
        self.settle(view);
    }

    pub fn click_key(&mut self, view: &mut dyn FnMut(&mut App) -> E, key: impl std::hash::Hash) {
        let r = rect_of(key).expect("the element was on screen last frame");
        self.click(view, r.x + r.w * 0.5, r.y + r.h * 0.5);
    }

    pub fn right_click(&mut self, view: &mut dyn FnMut(&mut App) -> E, x: f32, y: f32) {
        self.point(x, y);
        self.frame(view);
        self.input.right_pressed = true;
        self.frame(view);
        self.settle(view);
    }

    pub fn wheel(&mut self, view: &mut dyn FnMut(&mut App) -> E, dy: f32) {
        self.input.scroll = dy;
        self.input.scroll_is_pixels = true;
        self.frame(view);
    }

    pub fn key(&mut self, view: &mut dyn FnMut(&mut App) -> E, key: Key, cmd: bool, shift: bool) {
        self.input.keys.push(KeyPress { key, shift, cmd, alt: false, ctrl: false });
        self.frame(view);
    }

    pub fn type_text(&mut self, view: &mut dyn FnMut(&mut App) -> E, text: &str) {
        self.input.text.push_str(text);
        self.frame(view);
    }

    pub fn rect(&self, key: impl std::hash::Hash) -> Option<Rect> {
        rect_of(key)
    }
}

// ---------------------------------------------------------------- the accessibility tree

pub type Tree = Vec<(accesskit::NodeId, accesskit::Node)>;

impl Rig {
    /// The whole tree, as a client that has just arrived gets it.
    pub fn a11y_tree(&mut self, view: &mut dyn FnMut(&mut App) -> E) -> Tree {
        set_a11y_active(true);
        self.settle(view);
        take_a11y_update().expect("a client gets the tree").nodes
    }

    /// What a screen reader's press does: the same handlers a pointer reaches.
    pub fn a11y_press(&mut self, view: &mut dyn FnMut(&mut App) -> E, id: accesskit::NodeId) {
        a11y_action(accesskit::ActionRequest { action: accesskit::Action::Click, target_tree: accesskit::TreeId::ROOT, target_node: id, data: None });
        self.settle(view);
    }
}

pub fn says<'a>(tree: &'a Tree, words: &str) -> Option<&'a accesskit::Node> {
    tree.iter().map(|(_, node)| node).find(|node| node.label() == Some(words) || (node.role() == Role::Label && node.value() == Some(words)))
}

pub fn said<'a>(tree: &'a Tree, words: &str) -> &'a accesskit::Node {
    says(tree, words).unwrap_or_else(|| panic!("nothing says {words:?}: {:?}", tree.iter().filter_map(|(_, n)| n.label().or(n.value())).collect::<Vec<_>>()))
}

pub fn id_of(tree: &Tree, words: &str) -> accesskit::NodeId {
    tree.iter().find(|(_, node)| node.label() == Some(words)).unwrap_or_else(|| panic!("nothing is called {words:?}")).0
}

/// Everything a reader can press or type into with no name to call it by. A task list's box in a message is neither, and has no name upstream either.
pub fn nameless(tree: &Tree) -> Vec<String> {
    tree.iter()
        .filter(|(_, node)| !(node.role() == Role::CheckBox && node.is_disabled()))
        .filter(|(_, node)| matches!(node.role(), Role::Button | Role::Link | Role::TextInput | Role::MultilineTextInput | Role::Tab | Role::Switch | Role::CheckBox | Role::MenuItem | Role::ComboBox) || node.supports_action(accesskit::Action::Click))
        .filter(|(_, node)| node.label().is_none_or(str::is_empty) && node.labelled_by().is_empty() && (matches!(node.role(), Role::TextInput | Role::MultilineTextInput) || node.value().is_none_or(str::is_empty)))
        .map(|(id, node)| format!("{:?} {id:?}", node.role()))
        .collect()
}
