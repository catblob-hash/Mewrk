//! A real host behind a scripted window.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fluent::el::{rect_of, render, set_theme, set_window_size};
use fluent::ui::{Clipboard, Input, Key, KeyPress, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::*;
use mewrk_client::{i18n, App, StartOptions};
use mewrk_shell::root;
use serde_json::Value;

/// `model.rs`'s current document version.
const SCHEMA_VERSION: u32 = 6;

pub const WIN: (f32, f32) = (1400.0, 900.0);

/// One test at a time: the language and `HOME` are process-wide, and the host keeps statics of its own.
pub static SERIAL: Mutex<()> = Mutex::new(());

#[derive(Default)]
pub struct MemClipboard(pub Option<String>);

impl Clipboard for MemClipboard {
    fn get(&mut self) -> Option<String> {
        self.0.clone()
    }
    fn set(&mut self, text: &str) {
        self.0 = Some(text.to_owned());
    }
}

pub struct Rig {
    pub dir: PathBuf,
    pub home: PathBuf,
    pub project: PathBuf,
    pub project_two: PathBuf,
    /// The data folders `restart` started on.
    copies: Vec<PathBuf>,
    pub win: (f32, f32),
    pub scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    pub input: Input,
    pub clipboard: MemClipboard,
    pub time: f32,
    _serial: MutexGuard<'static, ()>,
}

fn fresh_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("mewrk-shell-{tag}-{}-{n:x}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // macOS answers with the /private spelling, which is what the host compares against.
    dir.canonicalize().unwrap()
}

/// The host's own tools, from the document it writes when it starts on nothing: a run offers the model what the document lists.
fn catalogue() -> Value {
    static TOOLS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    TOOLS
        .get_or_init(|| {
            let dir = fresh_dir("catalogue");
            let app = App::start(StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: dir.clone(), resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|_| "App::start failed").unwrap();
            let tools = serde_json::to_value(&app.doc.tools).unwrap();
            app.host.finalize();
            drop(app);
            let _ = std::fs::remove_dir_all(dir);
            tools
        })
        .clone()
}

/// Two projects in the document before the host starts: a directory a stored document already holds is one it has already
/// been granted, and the picker that grants new ones needs a person.
fn seed_document(dir: &Path, one: &Path, two: &Path) {
    let project = |id: &str, name: &str, path: &Path| json!({ "id": id, "name": name, "kind": "directory", "path": path.to_str().unwrap(), "createdAt": "2026-01-01T00:00:00.000Z", "defaultConversationPresetId": "", "conversations": [] });
    let document = json!({
        "schemaVersion": SCHEMA_VERSION,
        "globalSettings": {},
        "workspaces": [project("ws1", "alpha", one), project("ws2", "beta", two)],
        "tools": catalogue(),
        "capabilities": { "skills": [], "mcps": [] },
    });
    std::fs::write(dir.join("document.v1.json"), serde_json::to_vec_pretty(&document).unwrap()).unwrap();
}

impl Drop for Rig {
    fn drop(&mut self) {
        for dir in [&self.dir, &self.home, &self.project, &self.project_two].into_iter().chain(&self.copies) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl Rig {
    pub fn new(tag: &str) -> (Self, App) {
        let serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = fresh_dir(&format!("{tag}-data"));
        let home = fresh_dir(&format!("{tag}-home"));
        let project = fresh_dir(&format!("{tag}-one"));
        let project_two = fresh_dir(&format!("{tag}-two"));
        seed_document(&dir, &project, &project_two);
        // The global level is read from the home folder, and a test must not see the machine's own.
        // SAFETY: tests here run one at a time, under `SERIAL`.
        unsafe {
            std::env::set_var("HOME", &home);
            // Where `std::env::home_dir` looks on Windows.
            std::env::set_var("USERPROFILE", &home);
        }
        set_theme(Theme::dark());
        let mut app = App::start(StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: dir.clone(), resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
        i18n::set_english(true);
        app.system_dark = true;
        root::init(&mut app);
        let rig = Self { dir, home, project, project_two, copies: Vec::new(), win: WIN, scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: MemClipboard::default(), time: 0.0, _serial: serial };
        (rig, app)
    }

    /// Quits and starts again on the same data, as a person closing and reopening the app would.
    pub fn restart(&mut self, mut app: App) -> App {
        app.save_conversation_turns();
        app.flush_ui_store();
        app.host.finalize();
        drop(app);
        self.scene = Scene::default();
        self.mem = UiMemory::default();
        self.input = Input::default();
        // On a copy: the host holds its data folder for the life of the process, and this process goes on.
        let copy = fresh_dir("restart");
        let status = std::process::Command::new("cp").arg("-R").arg(format!("{}/.", self.dir.display())).arg(&copy).status().unwrap();
        assert!(status.success());
        self.copies.push(copy.clone());
        let mut app = App::start(StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: copy, resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|_| "App::start failed").unwrap();
        app.system_dark = true;
        root::init(&mut app);
        app
    }

    /// A conversation with something in it, so the sidebar lists it: created through the host as a stored one would be, with
    /// the built-in preset's settings.
    pub fn add_conversation(&mut self, app: &mut App, workspace: &str, title: &str) -> String {
        let id = format!("conv-{}", title.replace(' ', "-"));
        let preset = app.doc.presets.conversation_presets.first().expect("the built-in preset").clone();
        let mut settings: mewrk_client::model::ConversationSettings = serde_json::from_value(json!({ "enabledTools": [] })).unwrap();
        settings = mewrk_client::presets::apply_conversation_preset_settings(&settings, &preset.settings, None);
        let contexts = json!([{ "kind": "user", "id": format!("{id}-u"), "content": "hello", "createdAt": "2026-01-01T00:00:00.000Z" }]);
        let conversation = json!({ "id": id, "title": title, "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z", "settings": settings, "contexts": contexts, "presetId": preset.id });
        let created = app.host.invoke_blocking("create_conversation", json!({ "workspaceId": workspace, "conversation": conversation })).expect("create_conversation");
        let created: mewrk_client::model::Conversation = serde_json::from_value(created).unwrap();
        let target = workspace.to_owned();
        app.update_document(move |doc| doc.workspaces.iter_mut().find(|w| w.id == target).expect("seeded project").conversations.insert(0, created));
        id
    }

    pub fn frame(&mut self, app: &mut App) {
        app.tick();
        self.scene.clear();
        app.frame_no += 1;
        set_window_size(self.win.0, self.win.1);
        let theme = root::theme(app, None);
        set_theme(theme);
        let mut ui = Ui::new(&mut self.scene, &mut self.text, theme, &self.input, &mut self.mem, &mut self.clipboard, 1.0, self.win, 0.016, self.time);
        let view = root::root(app);
        render(view, app, &mut ui);
        self.input.end_frame();
    }

    /// Where every glyph of the last frame was drawn.
    pub fn glyphs(&self) -> Vec<(f32, f32)> {
        self.scene.layers.iter().flat_map(|layer| layer.glyphs.iter()).map(|glyph| (glyph.pos[0], glyph.pos[1])).collect()
    }

    pub fn frames(&mut self, app: &mut App, n: usize) {
        for _ in 0..n {
            self.frame(app);
        }
    }

    /// Runs frames until `done` holds, or fails the test naming what it waited for.
    pub fn until(&mut self, app: &mut App, what: &str, done: impl Fn(&mut App) -> bool) {
        let start = Instant::now();
        loop {
            self.frame(app);
            if done(app) {
                self.frame(app);
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Whether an element with this key is there, drawn in one of the last frames.
    pub fn exists(&mut self, app: &mut App, key: impl std::hash::Hash) -> bool {
        self.frames(app, 2);
        rect_of(key).is_some()
    }

    pub fn rect(&mut self, app: &mut App, key: impl std::hash::Hash + Clone) -> Rect {
        self.frames(app, 2);
        rect_of(key).expect("an element with that key")
    }

    pub fn point(&mut self, app: &mut App, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame(app);
    }

    /// Clicks whatever carries this key, as a pointer would.
    pub fn click(&mut self, app: &mut App, key: impl std::hash::Hash) {
        self.frames(app, 2);
        let r = rect_of(key).expect("nothing to click");
        self.click_at(app, r.x + r.w / 2.0, r.y + r.h / 2.0);
    }

    /// Rests the pointer on what carries this key, long enough for hover text to come up, and says how many glyphs were drawn for it.
    pub fn rest_on(&mut self, app: &mut App, key: impl std::hash::Hash) -> usize {
        self.input.pointer = None;
        self.frames(app, 2);
        let r = rect_of(key).expect("nothing to rest on");
        self.input.pointer = Some((r.x + r.w / 2.0, r.y + r.h / 2.0));
        self.frame(app);
        self.time += 1.0;
        self.frame(app);
        self.scene.layers[fluent::gpu::LAYER_TOOLTIP].glyphs.len()
    }

    pub fn click_at(&mut self, app: &mut App, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame(app);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(app);
        self.input.down = false;
        self.input.released = true;
        self.frame(app);
        self.frame(app);
    }

    pub fn double_click(&mut self, app: &mut App, key: impl std::hash::Hash) {
        self.frames(app, 2);
        let r = rect_of(key).expect("nothing to click");
        self.input.pointer = Some((r.x + r.w / 2.0, r.y + r.h / 2.0));
        self.frame(app);
        for second in [false, true] {
            self.input.pressed = true;
            self.input.down = true;
            self.input.double_click = second;
            self.frame(app);
            self.input.down = false;
            self.input.released = true;
            self.frame(app);
        }
        self.frame(app);
    }

    /// Holds the button at one point and moves to another without letting go.
    pub fn press_and_move(&mut self, app: &mut App, from: (f32, f32), to: (f32, f32)) {
        self.input.pointer = Some(from);
        self.frame(app);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(app);
        for step in 1..=4 {
            let k = step as f32 / 4.0;
            self.input.pointer = Some((from.0 + (to.0 - from.0) * k, from.1 + (to.1 - from.1) * k));
            self.frame(app);
        }
    }

    pub fn release(&mut self, app: &mut App) {
        self.input.down = false;
        self.input.released = true;
        self.frame(app);
        self.frame(app);
    }

    pub fn drag(&mut self, app: &mut App, from: (f32, f32), to: (f32, f32)) {
        self.press_and_move(app, from, to);
        self.release(app);
    }

    pub fn key(&mut self, app: &mut App, press: KeyPress) {
        self.input.keys.push(press);
        self.frame(app);
        self.frame(app);
    }

    pub fn plain(&mut self, app: &mut App, key: Key) {
        self.key(app, KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false });
    }

    pub fn cmd(&mut self, app: &mut App, letter: char) {
        self.key(app, KeyPress { key: Key::Char(letter), shift: false, cmd: true, alt: false, ctrl: false });
    }

    /// Text as a window delivers it, to whatever has the keyboard.
    pub fn type_text(&mut self, app: &mut App, text: &str) {
        self.input.text = text.to_owned();
        self.frame(app);
        self.frame(app);
    }

    pub fn focused(&self) -> u64 {
        self.mem.focus
    }
}

pub fn workspace_of<'a>(app: &'a App, id: &str) -> &'a mewrk_client::model::Workspace {
    app.doc.workspaces.iter().find(|workspace| workspace.id == id).expect("a project")
}

pub fn conversation_order(app: &App, workspace: &str) -> Vec<String> {
    workspace_of(app, workspace).conversations.iter().map(|conversation| conversation.title.clone()).collect()
}
