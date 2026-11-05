//! A composer on the real host, driven with scripted input and no GPU: `App::start` over a temp data dir, frames run by hand. Only
//! `run_model` is held by a stand-in, so a test can say how a run goes; everything else reaches the host's own commands.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once};
use std::time::{Duration, Instant};

use crate::attach::{attach_ports, ClipboardContents};
use crate::{composer, ComposerProps};
use fluent::ui::{Clipboard, Input, KeyPress, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::*;
use mewrk_client::runs::{EventSink, RunBackend};
use mewrk_client::{App, Host, StartOptions};

pub const WS: &str = "ws1";
pub const CONV: &str = "conversation-1";

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
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("mewrk-composer-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `run_model` held until a test lets it go; anything else goes to the host.
pub struct HeldRuns {
    host: Host,
    pub calls: Mutex<Vec<(String, Value)>>,
    held: Mutex<u64>,
    released: Condvar,
    /// Refuses `run_model`, which is how a send that never started looks.
    pub refuse: Mutex<Option<String>>,
    /// Answers a command in the host's place.
    pub answers: Mutex<std::collections::HashMap<String, Result<Value, String>>>,
}

impl HeldRuns {
    pub fn count(&self, command: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|(name, _)| name == command).count()
    }

    pub fn last(&self, command: &str) -> Option<Value> {
        self.calls.lock().unwrap().iter().rev().find(|(name, _)| name == command).map(|(_, args)| args.clone())
    }

    pub fn release_runs(&self) {
        *self.held.lock().unwrap() += 1;
        self.released.notify_all();
    }
}

impl RunBackend for HeldRuns {
    fn invoke(&self, command: &str, args: Value, _events: Option<EventSink>) -> Result<Value, String> {
        self.calls.lock().unwrap().push((command.to_owned(), args.clone()));
        if let Some(answer) = self.answers.lock().unwrap().get(command) {
            return answer.clone();
        }
        match command {
            "run_model" => {
                if let Some(reason) = self.refuse.lock().unwrap().clone() {
                    return Err(reason);
                }
                let started = *self.held.lock().unwrap();
                let mut held = self.held.lock().unwrap();
                while *held == started {
                    held = self.released.wait(held).unwrap();
                }
                Ok(json!({ "contexts": [], "usage": {}, "model": "model-1", "providerName": "Provider", "durationMs": 1 }))
            }
            "cancel_conversation_run" | "cancel_model_run" => {
                self.release_runs();
                Ok(json!(true))
            }
            "attach_model_run" => Ok(json!({ "status": "none" })),
            "take_run_settlement" => Ok(Value::Null),
            "resolve_tool_prompt" | "request_tool_approval" => Ok(json!({ "expiresInMs": 0 })),
            "steer_model_run" => Ok(Value::Null),
            _ => self.host.invoke_blocking(command, args),
        }
    }
}

pub struct Rig {
    pub app: App,
    pub repo: PathBuf,
    pub host: Arc<HeldRuns>,
    pub input: Input,
    pub clipboard: TestClipboard,
    pub size: (f32, f32),
    pub theme: Theme,
    pub props: ComposerProps,
    pub view: Option<Box<dyn FnMut(&mut App) -> E>>,
    pub picked: Arc<Mutex<Option<Vec<PathBuf>>>>,
    pub system_clipboard: Rc<std::cell::RefCell<ClipboardContents>>,
    scene: Scene,
    text: TextCtx,
    pub mem: UiMemory,
    _data: TempDir,
    _project: TempDir,
    _language: MutexGuard<'static, ()>,
}

pub fn git(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").args(args).current_dir(repo).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What seeding the document can change.
pub struct Seed {
    /// The conversations of the project, as JSON.
    pub conversations: Vec<Value>,
    /// The capabilities of the one model.
    pub capabilities: Vec<&'static str>,
    pub git: bool,
    pub english: bool,
}

impl Default for Seed {
    fn default() -> Self {
        Self { conversations: vec![conversation(CONV, "Task")], capabilities: vec!["image_recognition"], git: false, english: true }
    }
}

pub fn conversation(id: &str, title: &str) -> Value {
    json!({ "id": id, "title": title, "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z", "settings": { "enabledTools": ["read"] }, "contexts": [] })
}

impl Rig {
    pub fn new() -> Self {
        Self::seeded(Seed::default())
    }

    pub fn seeded(seed: Seed) -> Self {
        let language = crate::tests::language(seed.english);
        static ENVIRONMENT: Once = Once::new();
        let (data, project, home) = (TempDir::new("data"), TempDir::new("project"), TempDir::new("home"));
        // SAFETY: the first thing any rig does, before a host thread exists to read the environment.
        ENVIRONMENT.call_once(|| unsafe {
            std::env::set_var("HOME", &home.0);
            // Where `std::env::home_dir` looks on Windows.
            std::env::set_var("USERPROFILE", &home.0);
            std::env::set_var("MEWRK_LANG", "en");
        });
        std::mem::forget(home);
        if seed.git {
            git(&project.0, &["init", "-b", "main"]);
            std::fs::write(project.0.join("README.md"), "# readme\n").unwrap();
            git(&project.0, &["add", "."]);
            git(&project.0, &["commit", "-m", "first"]);
            git(&project.0, &["branch", "other"]);
        }
        let document = json!({
            "schemaVersion": 6,
            "globalSettings": {},
            "workspaces": [{ "id": WS, "name": "project", "kind": "directory", "path": project.0.to_str().unwrap(), "createdAt": "2026-01-01T00:00:00.000Z", "defaultConversationPresetId": "", "conversations": [] }],
            "tools": [{ "name": "read", "label": "Read", "description": "", "category": "filesystem", "dangerous": false, "parameters": [] }],
            "capabilities": { "skills": [], "mcps": [] },
        });
        std::fs::write(data.0.join("document.v1.json"), serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        set_theme(Theme::dark());
        let mut app = App::start(StartOptions { identifier: "mewrk.composer.tests".into(), version: "0.0.0".into(), data_dir: data.0.clone(), resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).expect("the host starts");
        mewrk_client::i18n::set_english(seed.english);
        mewrk_client::doc::init(&mut app);
        let host = Arc::new(HeldRuns { host: app.host.clone(), calls: Mutex::default(), held: Mutex::default(), released: Condvar::new(), refuse: Mutex::default(), answers: Mutex::default() });
        app.set_run_backend(host.clone());
        // Conversations live in the host's store, not in the document, so they arrive the way a real one does: through a save.
        let conversations: Vec<mewrk_client::model::Conversation> = seed.conversations.iter().map(|conversation| serde_json::from_value(conversation.clone()).unwrap()).collect();
        let created = conversations.clone();
        let ordered: Vec<String> = conversations.iter().map(|conversation| conversation.id.clone()).collect();
        app.update_document(|doc| {
            if let Some(workspace) = doc.workspaces.iter_mut().find(|workspace| workspace.id == WS) {
                workspace.conversations = conversations;
            }
        });
        for conversation in created {
            app.sync_conversation_created(WS, conversation, ordered.clone());
        }
        let saved = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = saved.clone();
        app.flush_latest_document(true, move |app, result| {
            result.expect("the host takes the conversations");
            app.flush_document_saves(move |_, _| done.store(true, Ordering::SeqCst));
        });
        let started = Instant::now();
        while !saved.load(Ordering::SeqCst) {
            app.tick();
            assert!(started.elapsed() < Duration::from_secs(20), "the save never landed");
            std::thread::sleep(Duration::from_millis(2));
        }
        app.doc.assets.api_providers = vec![provider(&seed.capabilities)];
        app.doc.global_settings.active_provider_id = Some("provider-1".to_owned());
        app.doc.global_settings.appearance.send_shortcut = vec!["Enter".to_owned()];
        app.doc.global_settings.appearance.newline_shortcut = vec!["Shift".to_owned(), "Enter".to_owned()];
        if app.doc.workspaces.iter().any(|workspace| workspace.id == WS && workspace.conversations.iter().any(|conversation| conversation.id == CONV)) {
            app.select_conversation(WS, CONV);
        } else {
            app.open_draft_conversation(Some(WS));
        }
        let picked: Arc<Mutex<Option<Vec<PathBuf>>>> = Arc::default();
        let system_clipboard: Rc<std::cell::RefCell<ClipboardContents>> = Rc::default();
        {
            let (picked, contents) = (picked.clone(), system_clipboard.clone());
            let ports = attach_ports(&mut app);
            ports.pick_files = Arc::new(move || picked.lock().unwrap().clone());
            ports.read_clipboard = Rc::new(move || contents.borrow().clone());
        }
        mewrk_ui_common::popover_menu::probe::start();
        Self { app, repo: project.0.clone(), host, input: Input::default(), clipboard: TestClipboard::default(), size: (1100.0, 800.0), theme: Theme::dark(), props: ComposerProps::default(), view: None, picked, system_clipboard, scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), _data: data, _project: project, _language: language }
    }

    /// Replaces what the rig draws: the composer at the foot of the window, by default.
    pub fn with_view(mut self, view: impl FnMut(&mut App) -> E + 'static) -> Self {
        self.view = Some(Box::new(view));
        self
    }

    pub fn frame(&mut self) {
        self.draw();
    }

    /// One frame, answering what the views said while they were built.
    fn draw(&mut self) -> Vec<String> {
        self.app.tick();
        self.scene.clear();
        set_window_size(self.size.0, self.size.1);
        set_theme(self.theme);
        mewrk_ui_common::popover_menu::probe::take();
        let body: E = match &mut self.view {
            Some(view) => view(&mut self.app),
            None => {
                let props = self.props.clone();
                composer(&mut self.app, props)
            }
        };
        let said = mewrk_ui_common::popover_menu::probe::take();
        let root = col::<App>().size(self.size.0, self.size.1).justify_end().child(body);
        let mut ui = Ui::new(&mut self.scene, &mut self.text, self.theme, &self.input, &mut self.mem, &mut self.clipboard, 1.0, self.size, 0.016, 0.0);
        render(root, &mut self.app, &mut ui);
        self.input.end_frame();
        said
    }

    /// Frames until `done` says so: answers from the host arrive when they arrive.
    pub fn until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        let started = Instant::now();
        loop {
            self.frame();
            if done(self) {
                self.frame();
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "timed out waiting for {what}; on screen: {:?}", self.words());
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    pub fn settle(&mut self, ms: u64) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(ms) {
            self.frame();
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    /// What the views say now, every string they put through the probe.
    pub fn words(&mut self) -> Vec<String> {
        self.frame();
        self.draw()
    }

    pub fn says(&mut self, word: &str) -> bool {
        let words = self.words();
        words.iter().any(|said| said == word)
    }

    #[track_caller]
    pub fn rect(&self, key: impl std::hash::Hash) -> Rect {
        rect_of(key).expect("the element is on screen")
    }

    pub fn has(&self, key: impl std::hash::Hash) -> bool {
        rect_of(key).is_some()
    }

    /// How many line segments the last frame drew: what a squiggle is made of.
    pub fn lines_drawn(&self) -> usize {
        const KIND_LINE: f32 = 3.0;
        self.scene.layers.iter().flat_map(|layer| &layer.rects).filter(|inst| inst.params[2] == KIND_LINE).count()
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

    #[track_caller]
    pub fn click(&mut self, key: impl std::hash::Hash) {
        self.frame();
        let r = self.rect(key);
        self.click_at(r.x + r.w * 0.5, r.y + r.h * 0.5);
        self.frame();
    }

    pub fn press(&mut self, key: Key) {
        self.press_with(key, false, false);
    }

    pub fn press_with(&mut self, key: Key, shift: bool, cmd: bool) {
        self.input.keys.push(KeyPress { key, shift, cmd, alt: false, ctrl: false });
        self.frame();
        self.frame();
    }

    pub fn type_text(&mut self, text: &str) {
        self.input.text.push_str(text);
        self.frame();
        self.frame();
    }

    /// Clicks into the composer's text area.
    pub fn focus_text_area(&mut self) {
        self.click(crate::editor::textarea_key());
    }

    pub fn draft(&mut self) -> String {
        let id = self.app.active_conversation_id().unwrap_or_default();
        self.app.composer_draft(&id)
    }

    pub fn conversation_id(&mut self) -> String {
        self.app.active_conversation_id().unwrap_or_default()
    }
}

pub fn provider(capabilities: &[&str]) -> mewrk_client::model::ApiProvider {
    serde_json::from_value(json!({
        "id": "provider-1", "name": "Provider", "enabled": true, "family": "openai_chat", "baseUrl": "http://localhost:9/v1",
        "models": [{ "id": "model-1", "name": "", "group": "", "capabilities": capabilities, "reasoningContent": "plaintext", "contextWindow": 200000 }, { "id": "model-2", "name": "", "group": "", "capabilities": capabilities, "reasoningContent": "plaintext" }],
        "activeModelId": "model-1"
    }))
    .unwrap()
}

/// A small real PNG.
pub fn png() -> Vec<u8> {
    let picture = image::RgbaImage::from_fn(8, 8, |x, y| image::Rgba([(x * 30) as u8, (y * 30) as u8, 120, 255]));
    let mut bytes = Vec::new();
    picture.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
    bytes
}
