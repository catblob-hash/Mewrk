//! A real host behind a scripted window: `App::start` over a temp data dir and a temp project with a real `.mewrk` folder, and no GPU.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fluent::el::{hash_key, rect_of, render, repaint, set_theme, set_window_size, theme};
use fluent::ui::{Input, Key, KeyPress, NoClipboard, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::*;
use mewrk_client::{i18n, App, StartOptions};
use mewrk_settings_conv::widgets::probe;
use mewrk_settings_conv::ops;
use mewrk_ui_chat::inline_text_editor::{text_field_key, InsertKind};
use mewrk_ui_common::context_menu::menu_row_key;

/// `model.rs`'s current document version.
const SCHEMA_VERSION: u32 = 6;

const SHOT_FORMAT: wgpu::TextureFormat = fluent::gpu::SURFACE_FORMAT;

/// Tall enough that a page's rows are on screen rather than scrolled out of reach.
const WIN: (f32, f32) = (1400.0, 1000.0);

/// One test at a time: the language and `HOME` are process-wide, and the host keeps statics of its own.
static SERIAL: Mutex<()> = Mutex::new(());

pub struct Rig {
    /// The window's logical size, for looking at what a narrower one lays out.
    pub size: (f32, f32),
    pub dir: PathBuf,
    pub home: PathBuf,
    pub project: PathBuf,
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: NoClipboard,
    renderer: Option<fluent::gpu::Renderer>,
    time: f32,
    _serial: MutexGuard<'static, ()>,
}

pub type View<'a> = &'a dyn Fn(&mut App) -> E;

fn fresh_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("mewrk-settings-{tag}-{}-{n:x}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // macOS answers with the /private spelling, which is what the host compares against.
    dir.canonicalize().unwrap()
}

pub fn start_app(dir: &Path) -> App {
    let mut app = App::start(StartOptions {
        identifier: "mewrk.test".into(),
        version: "0.0.0".into(),
        data_dir: dir.to_owned(),
        resource_dir: None,
        wake: Arc::new(|| {}),
        exit: Arc::new(|_| {}),
        show_window: Arc::new(|| {}),
    })
    .map_err(|e| format!("{e:?}"))
    .unwrap();
    i18n::set_english(false);
    app.system_dark = true;
    app
}

/// A real project folder, the way a user would have written it.
pub fn seed_project(project: &Path) {
    let mewrk = project.join(".mewrk");
    std::fs::create_dir_all(mewrk.join("skills/demo-skill")).unwrap();
    std::fs::create_dir_all(mewrk.join("agents")).unwrap();
    std::fs::write(mewrk.join("skills/demo-skill/SKILL.md"), "---\nname: demo-skill\ndescription: A skill of this project\n---\nDo the thing.\n").unwrap();
    std::fs::write(mewrk.join("mcp.json"), r#"{"mcpServers":{"demo-mcp":{"command":"/bin/echo","args":["hi"]}}}"#).unwrap();
    std::fs::write(mewrk.join("hooks.json"), r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"true"}]}]}}"#).unwrap();
    std::fs::write(mewrk.join("agents/reviewer.json"), r#"{"name":"reviewer","description":"Reviews code","modelSelection":{"kind":"inherit"}}"#).unwrap();
}

/// The project in the document before the host starts: a directory a stored document already holds is one it has already been granted, and the picker that grants new ones needs a person.
fn seed_document(dir: &Path, project: &Path) {
    let document = json!({
        "schemaVersion": SCHEMA_VERSION,
        "globalSettings": {},
        "workspaces": [{
            "id": "ws1",
            "name": "project",
            "kind": "directory",
            "path": project.to_str().unwrap(),
            "createdAt": "2026-01-01T00:00:00.000Z",
            "defaultConversationPresetId": "",
            "conversations": [],
        }],
        // Refreshed from the host's own catalogue on load; it only refuses an empty one.
        "tools": [{ "name": "read", "label": "Read", "description": "", "category": "filesystem", "dangerous": false, "parameters": [] }],
        "capabilities": { "skills": [], "mcps": [] },
    });
    std::fs::write(dir.join("document.v1.json"), serde_json::to_vec_pretty(&document).unwrap()).unwrap();
}

/// A data folder, a home folder and a project, ready for a host to start on.
pub struct Seeded {
    pub dir: PathBuf,
    pub home: PathBuf,
    pub project: PathBuf,
}

impl Seeded {
    pub fn remove(&self) {
        for dir in [&self.dir, &self.home, &self.project] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

pub fn seed(tag: &str) -> Seeded {
    let dir = fresh_dir(&format!("{tag}-data"));
    let home = fresh_dir(&format!("{tag}-home"));
    let project = fresh_dir(&format!("{tag}-project"));
    seed_project(&project);
    seed_document(&dir, &project);
    Seeded { dir, home, project }
}

/// A test leaves nothing behind. The host still holds these open, which the OS does not mind.
impl Drop for Rig {
    fn drop(&mut self) {
        if std::env::var_os("MEWRK_TEST_DATA").is_none() {
            Seeded { dir: self.dir.clone(), home: self.home.clone(), project: self.project.clone() }.remove();
        }
    }
}

impl Rig {
    pub fn new(tag: &str) -> (Self, App) {
        Self::open(seed(tag))
    }

    pub fn open(seeded: Seeded) -> (Self, App) {
        let serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let Seeded { dir, home, project } = seeded;
        // The global level is read from the home folder, and a test must not see the machine's own.
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var("HOME", &home);
        }
        probe::enable();
        set_theme(Theme::dark());
        set_window_size(WIN.0, WIN.1);
        let rig = Self { size: WIN, dir: dir.clone(), home, project, scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: NoClipboard, renderer: None, time: 0.0, _serial: serial };
        (rig, start_app(&dir))
    }

    /// Runs one named `#[ignore]`d test of this binary in a process of its own, over this data. Only a process's end lets go of the data folder, so that is what a restart is.
    pub fn in_new_process(test: &str, seeded: &Seeded) {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--ignored", "--nocapture"])
            .env("MEWRK_TEST_DATA", &seeded.dir)
            .env("MEWRK_TEST_HOME", &seeded.home)
            .env("MEWRK_TEST_PROJECT", &seeded.project)
            .status()
            .unwrap();
        assert!(status.success(), "{test} failed");
    }

    /// What a test running through `in_new_process` was handed.
    pub fn seeded_from_env() -> Seeded {
        let get = |k: &str| PathBuf::from(std::env::var(k).unwrap_or_else(|_| panic!("{k}: run through in_new_process")));
        Seeded { dir: get("MEWRK_TEST_DATA"), home: get("MEWRK_TEST_HOME"), project: get("MEWRK_TEST_PROJECT") }
    }

    pub fn frame(&mut self, app: &mut App, view: View) {
        app.tick();
        probe::clear();
        self.scene.clear();
        app.frame_no += 1;
        set_window_size(self.size.0, self.size.1);
        let mut ui = Ui::new(&mut self.scene, &mut self.text, theme(), &self.input, &mut self.mem, &mut self.clipboard, 1.0, self.size, 0.016, self.time);
        render(view(app), app, &mut ui);
        self.input.end_frame();
    }

    /// Runs frames until `done` holds, or fails the test naming what it waited for.
    pub fn until(&mut self, app: &mut App, view: View, what: &str, done: impl Fn(&mut App) -> bool) {
        let start = Instant::now();
        loop {
            self.frame(app, view);
            if done(app) {
                self.frame(app, view);
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}; the frame said: {:?}", probe::said());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Runs until everything the app has written is on disk: the barrier quitting uses.
    pub fn settle(&mut self, app: &mut App, view: View) {
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d2 = done.clone();
        app.flush_latest_document(true, move |app, _| app.flush_document_saves(move |_, _| d2.store(true, std::sync::atomic::Ordering::SeqCst)));
        self.until(app, view, "writes to finish", move |app| done.load(std::sync::atomic::Ordering::SeqCst) && ops::idle(app));
    }

    /// Draws what the last frame painted, for a person to look at.
    pub fn shot(&mut self, app: &mut App, view: View, path: &Path) {
        use fluent::gpu::{Gpu, Renderer};
        self.frame(app, view);
        self.frame(app, view);
        let size = (WIN.0 as u32, WIN.1 as u32);
        // One renderer for all of them: the glyph atlas uploads what changed since the last flush, so a new one would start without what is already there.
        let renderer = self.renderer.get_or_insert_with(|| Renderer::new(std::sync::Arc::new(Gpu::new(wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()), None).unwrap())));
        let gpu = renderer.gpu.clone();
        self.text.flush_atlases(renderer);
        let target = gpu.device.create_texture(&wgpu::TextureDescriptor { label: Some("shot"), size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: SHOT_FORMAT, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
        let view_tex = target.create_view(&Default::default());
        let _ = renderer.render(&self.scene, &theme(), &view_tex, size);
        let row = size.0 * 4;
        let aligned = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (aligned * size.1) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(aligned), rows_per_image: None } },
            wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = slice.get_mapped_range().unwrap();
        let mut rgba = Vec::with_capacity((row * size.1) as usize);
        for y in 0..size.1 as usize {
            for px in data[y * aligned as usize..y * aligned as usize + row as usize].chunks_exact(4) {
                rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        image::save_buffer(path, &rgba, size.0, size.1, image::ColorType::Rgba8).unwrap();
    }

    pub fn said(&self) -> Vec<String> {
        probe::said()
    }

    pub fn shows(&self, text: &str) -> bool {
        probe::said().iter().any(|s| s == text)
    }

    pub fn shows_containing(&self, text: &str) -> bool {
        probe::said().iter().any(|s| s.contains(text))
    }

    /// Clicks whatever carries this key, as a pointer would.
    pub fn click(&mut self, app: &mut App, view: View, key: impl std::hash::Hash) {
        self.frame(app, view);
        self.frame(app, view);
        let r = rect_of(key).unwrap_or_else(|| panic!("nothing to click; the frame said: {:?}", probe::said()));
        self.input.pointer = Some((r.x + r.w / 2.0, r.y + r.h / 2.0));
        self.frame(app, view);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(app, view);
        self.input.down = false;
        self.input.released = true;
        self.frame(app, view);
        self.frame(app, view);
    }

    /// Rests the pointer on whatever carries this key.
    pub fn hover(&mut self, app: &mut App, view: View, key: impl std::hash::Hash) {
        self.frame(app, view);
        let r = rect_of(key).unwrap_or_else(|| panic!("nothing to hover; the frame said: {:?}", probe::said()));
        self.input.pointer = Some((r.x + r.w / 2.0, r.y + r.h / 2.0));
        self.frame(app, view);
        self.time += 1.0;
        self.frame(app, view);
    }

    /// The text drawn in the layer hover text goes in.
    pub fn tooltip_layer_glyphs(&self) -> usize {
        self.scene.layers[fluent::gpu::LAYER_TOOLTIP].glyphs.len()
    }

    /// A press and release at a point of the window, whatever is there.
    pub fn click_at(&mut self, app: &mut App, view: View, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame(app, view);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(app, view);
        self.input.down = false;
        self.input.released = true;
        self.frame(app, view);
        self.frame(app, view);
    }

    /// A right press at a point of the window, whatever is there.
    pub fn right_click_at(&mut self, app: &mut App, view: View, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame(app, view);
        self.input.right_pressed = true;
        self.frame(app, view);
        self.frame(app, view);
        self.frame(app, view);
    }

    /// A right press on whatever carries this key.
    pub fn right_click(&mut self, app: &mut App, view: View, key: impl std::hash::Hash) {
        self.frame(app, view);
        self.frame(app, view);
        let r = rect_of(key).unwrap_or_else(|| panic!("nothing to right-click; the frame said: {:?}", probe::said()));
        self.right_click_at(app, view, r.x + r.w / 2.0, r.y + r.h / 2.0);
    }

    /// A click with Command or Control held, which is what picks a message out of a timeline.
    pub fn modified_click(&mut self, app: &mut App, view: View, key: impl std::hash::Hash) {
        self.input.zoom_modifier = true;
        self.click(app, view, key);
        self.input.zoom_modifier = false;
        self.frame(app, view);
    }

    /// A drag with Command or Control held between two points of the window: the box that picks messages out of a timeline.
    pub fn modified_drag_at(&mut self, app: &mut App, view: View, from: (f32, f32), to: (f32, f32)) {
        self.input.zoom_modifier = true;
        self.input.pointer = Some(from);
        self.frame(app, view);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(app, view);
        self.input.pointer = Some(to);
        self.frame(app, view);
        self.frame(app, view);
        self.input.down = false;
        self.input.released = true;
        self.frame(app, view);
        self.input.zoom_modifier = false;
        self.frame(app, view);
        self.frame(app, view);
    }

    /// A drag from one element to the edge of another.
    pub fn drag(&mut self, app: &mut App, view: View, from: u64, to: u64, before: bool) {
        self.frame(app, view);
        self.frame(app, view);
        let a = rect_of(from).expect("drag source");
        let b = rect_of(to).expect("drag target");
        self.input.pointer = Some((a.x + 4.0, a.y + a.h / 2.0));
        self.frame(app, view);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(app, view);
        self.input.pointer = Some((b.x + 4.0, if before { b.y + 2.0 } else { b.y + b.h - 2.0 }));
        self.frame(app, view);
        self.frame(app, view);
        self.input.down = false;
        self.input.released = true;
        self.frame(app, view);
        self.frame(app, view);
    }

    /// Whether an element with this key was laid out in the last frame.
    pub fn exists(&self, key: impl std::hash::Hash) -> bool {
        rect_of(key).is_some()
    }

    /// Types into a `field` built with this key, replacing nothing: the field holds what it holds.
    pub fn type_into(&mut self, app: &mut App, view: View, field_key: impl std::hash::Hash + Clone, value: &str) {
        fluent::el::focus_request(("field", field_key));
        repaint();
        self.frame(app, view);
        self.frame(app, view);
        // A window delivers a line break as the Enter key, not as text.
        for (i, line) in value.split('\n').enumerate() {
            if i > 0 {
                self.input.keys.push(KeyPress { key: Key::Enter, shift: false, cmd: false, alt: false, ctrl: false });
                self.frame(app, view);
            }
            self.input.text = line.to_owned();
            self.frame(app, view);
        }
        self.frame(app, view);
    }

    /// Replaces what a `field` built with this key holds, as selecting all and typing over it does.
    pub fn replace_in(&mut self, app: &mut App, view: View, field_key: impl std::hash::Hash + Clone, value: &str) {
        fluent::el::focus_request(("field", field_key));
        repaint();
        self.frame(app, view);
        self.frame(app, view);
        self.press_cmd(app, view, Key::Char('a'));
        self.input.text = value.to_owned();
        self.frame(app, view);
        self.frame(app, view);
    }

    pub fn press(&mut self, app: &mut App, view: View, key: Key, alt: bool) {
        self.input.keys.push(KeyPress { key, shift: false, cmd: false, alt, ctrl: false });
        self.frame(app, view);
        self.frame(app, view);
    }

    pub fn press_cmd(&mut self, app: &mut App, view: View, key: Key) {
        self.input.keys.push(KeyPress { key, shift: false, cmd: true, alt: false, ctrl: false });
        self.frame(app, view);
        self.frame(app, view);
    }
}

/// What the shell does at startup, for a test that runs the client's real document, drafts and capability scan.
pub fn init_live(app: &mut App) {
    mewrk_client::doc::init(app);
}

/// A conversation in the seeded project, made active: created through the host as `App` does, with the built-in preset's settings.
pub fn create_conversation(app: &mut App, id: &str) {
    let preset = app.doc.presets.conversation_presets.first().expect("the built-in preset").clone();
    let mut settings: mewrk_client::model::ConversationSettings = serde_json::from_value(json!({ "enabledTools": [] })).unwrap();
    settings = mewrk_client::presets::apply_conversation_preset_settings(&settings, &preset.settings, None);
    let conversation = json!({ "id": id, "title": "demo", "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z", "settings": settings, "contexts": [], "presetId": preset.id });
    let created = app.host.invoke_blocking("create_conversation", json!({ "workspaceId": "ws1", "conversation": conversation })).expect("create_conversation");
    let created: mewrk_client::model::Conversation = serde_json::from_value(created).unwrap();
    app.update_document(|doc| doc.workspaces.iter_mut().find(|w| w.id == "ws1").expect("seeded project").conversations.insert(0, created));
    app.set_active(Some("ws1"), Some(id));
}

/// Replaces a conversation's timeline in the host's own store, as a run would have left it.
pub fn set_timeline(app: &mut App, id: &str, contexts: Value) {
    let mut c = app.find_conversation("ws1", id).expect("conversation").clone();
    c.contexts = serde_json::from_value(contexts).unwrap();
    let stored = app.host.invoke_blocking("update_conversation", json!({ "workspaceId": "ws1", "conversation": c, "expectedContextIds": [] })).expect("update_conversation");
    let stored: mewrk_client::model::Conversation = serde_json::from_value(stored).unwrap();
    app.update_conversation_local("ws1", id, |c| {
        *c = stored;
        true
    });
}

// ---------------------------------------------------------------- a template's timeline

/// The key of a row of the right-click menu on a template's timeline, by the ids of the rows that lead to it.
pub fn timeline_menu_row(timeline: &str, path: &[&str], id: &str) -> u64 {
    menu_row_key(("tl-menu", hash_key(("context-scroll", timeline))), path, id)
}

/// What a person does to write a message into an open template: a right click on its timeline, 用户输入, the words, the editor's own Save.
pub fn place_user_message(rig: &mut Rig, app: &mut App, view: View, timeline: &str, words: &str) {
    rig.right_click(app, view, hash_key(("context-scroll", timeline)));
    rig.click(app, view, timeline_menu_row(timeline, &[], "User input"));
    let editor = hash_key(("tl-text-insert", InsertKind::User as u8));
    rig.type_into(app, view, text_field_key(editor), words);
    rig.click(app, view, ("text-editor-save", editor));
}

// ---------------------------------------------------------------- fixtures

pub fn tool(name: &str, label: &str, category: &str, dangerous: bool) -> mewrk_client::model::ToolDescriptor {
    serde_json::from_value(json!({ "name": name, "label": label, "description": "", "category": category, "dangerous": dangerous, "parameters": [] })).unwrap()
}

pub fn resource(id: &str, name: &str, workspace: Option<&str>) -> mewrk_client::model::ResourceDescriptor {
    let mut r: mewrk_client::model::ResourceDescriptor = serde_json::from_value(json!({ "id": id, "name": name, "description": format!("{name} description"), "location": format!("/somewhere/{name}"), "source": if workspace.is_some() { "workspace" } else { "user" }, "available": true })).unwrap();
    r.workspace_key = workspace.map(str::to_owned);
    r
}

pub fn builtin(id: &str, name: &str) -> mewrk_client::model::ResourceDescriptor {
    let mut r = resource(id, name, None);
    r.source = mewrk_client::model::ResourceSource::Builtin;
    r
}

pub fn provider(id: &str, name: &str, family: &str, models: &[(&str, &[&str])]) -> mewrk_client::model::ApiProvider {
    let ms: Vec<Value> = models.iter().map(|(m, caps)| json!({ "id": m, "capabilities": caps })).collect();
    serde_json::from_value(json!({ "id": id, "name": name, "enabled": true, "family": family, "baseUrl": "https://x.invalid", "models": ms, "activeModelId": models.first().map(|(m, _)| *m) })).unwrap()
}

/// The active provider and model, as the composer would leave them.
pub fn select_model(app: &mut App, provider: mewrk_client::model::ApiProvider) {
    app.doc.global_settings.active_provider_id = Some(provider.id.clone());
    app.doc.assets.api_providers = vec![provider];
}

/// A lock whose last request went out on the selected model a moment ago, so its cache is warm.
pub fn warm_lock(provider_id: &str, model_id: &str) -> mewrk_client::model::ConversationToolLock {
    mewrk_client::model::ConversationToolLock {
        last_request: Some(mewrk_client::model::ToolLockRequest { provider_id: provider_id.into(), model_id: model_id.into(), at: mewrk_client::doc::now_iso() }),
        ..Default::default()
    }
}

pub fn role_file(name: &str) -> Value {
    json!({ "name": name, "description": format!("{name} role"), "modelSelection": { "kind": "inherit" }, "effort": null, "tools": ["read"], "disallowedTools": [], "skillIds": [], "mcpIds": [], "hookIds": [], "webSearch": serde_json::to_value(mewrk_client::model::ConversationWebSearchSettings::default()).unwrap(), "templateId": null, "toolDescriptionFileId": null })
}

/// Lists a role in the catalog through the host's own row shape.
pub fn add_role(app: &mut App, resource: mewrk_client::model::ResourceDescriptor, role: Option<Value>) {
    let mut row = serde_json::to_value(&resource).unwrap();
    row["role"] = role.unwrap_or(Value::Null);
    let mut catalog = serde_json::to_value(&app.doc.capabilities).unwrap();
    catalog["agents"].as_array_mut().unwrap().push(row);
    app.doc.capabilities = serde_json::from_value(catalog).unwrap();
}

pub fn empty_catalog(app: &mut App) {
    app.doc.capabilities = serde_json::from_value(json!({ "skills": [], "mcps": [] })).unwrap();
}

// ---------------------------------------------------------------- accessibility

pub type AxNodes = Vec<(accesskit::NodeId, accesskit::Node)>;

/// The whole tree as the frame left it, with a client that has just arrived.
pub fn a11y_tree(rig: &mut Rig, app: &mut App, view: View) -> AxNodes {
    set_a11y_active(true);
    for _ in 0..3 {
        rig.frame(app, view);
    }
    take_a11y_update().expect("a client gets the tree").nodes
}

/// What a node is called: a label reads its value, everything else its label.
pub fn a11y_name(node: &accesskit::Node) -> &str {
    let words = if node.role() == accesskit::Role::Label { node.value() } else { node.label() };
    words.unwrap_or("")
}

/// The node of this role that says this: the words beside a field say what it does too.
pub fn a11y_is<'a>(tree: &'a AxNodes, role: accesskit::Role, name: &str) -> &'a accesskit::Node {
    tree.iter().map(|(_, node)| node).find(|node| node.role() == role && a11y_name(node) == name).unwrap_or_else(|| panic!("no {role:?} says {name:?} in {:#?}", tree.iter().map(|(_, node)| format!("{:?} {:?}", node.role(), a11y_name(node))).collect::<Vec<_>>()))
}

/// Whatever a keyboard or a reader can act on and has no name. A sweep of a surface asserts this is empty.
pub fn a11y_unnamed(tree: &AxNodes) -> Vec<String> {
    use accesskit::{Action, Role};
    tree.iter()
        .map(|(_, node)| node)
        .filter(|node| !matches!(node.role(), Role::Window | Role::TextRun | Role::Label))
        .filter(|node| [Action::Click, Action::Focus, Action::SetValue, Action::Increment].into_iter().any(|a| node.supports_action(a)) || matches!(node.role(), Role::Dialog | Role::AlertDialog))
        .filter(|node| a11y_name(node).trim().is_empty() && node.labelled_by().is_empty())
        .map(|node| format!("{:?} at {:?}", node.role(), node.bounds()))
        .collect()
}

/// Walks the Tab order from the top, all the way round, and says what each stop is called. A stop the tree doesn't know lands the
/// focus on the window or the dialog, and reads as what is around it.
pub fn a11y_tab_stops(rig: &mut Rig, app: &mut App, view: View) -> Vec<String> {
    let mut seen = Vec::new();
    let mut stops = Vec::new();
    for _ in 0..400 {
        rig.press(app, view, Key::Tab, false);
        let focus = focused_id();
        if focus == 0 {
            break;
        }
        if seen.contains(&focus) {
            assert_eq!(focus, seen[0], "Tab goes round the whole page, and this one sends it back early after {stops:#?}");
            break;
        }
        seen.push(focus);
        set_a11y_active(true);
        rig.frame(app, view);
        rig.frame(app, view);
        let update = take_a11y_update().expect("a client gets the tree");
        let node = update.nodes.iter().find(|(id, _)| *id == update.focus).map(|(_, node)| node).expect("the focus is in the tree");
        stops.push(format!("{:?} {:?}", node.role(), a11y_name(node)));
    }
    stops
}
