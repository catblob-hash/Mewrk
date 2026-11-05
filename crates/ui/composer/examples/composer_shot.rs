//! Renders the composer through the real GPU path to PNGs, for looking at.
//! `cargo run -p mewrk-ui-composer --features shots --example composer_shot -- <out dir> [light]`
//!
//! Everything shown is the real view on a real host over a temp data dir. Only what a person would have done is scripted.

use fluent::el::*;
use fluent::gpu::SURFACE_FORMAT;
use fluent::ui::{Input, NoClipboard, UiMemory};
use fluent::{Gpu, Renderer, Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::*;
use mewrk_client::runs::{ComposerFile, PendingToolPrompt, PromptKind};
use mewrk_client::{App, StartOptions};
use mewrk_ui_common::popover_menu::trigger_key;
use mewrk_ui_composer::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const W: u32 = 1800;
const H: u32 = 1500;
const SCALE: f32 = 2.0;
const WS: &str = "ws1";

struct Shot {
    gpu: Arc<Gpu>,
    renderer: Renderer,
    target: wgpu::Texture,
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    app: App,
    theme: Theme,
    out: PathBuf,
    landing: bool,
    started: Instant,
}

impl Shot {
    fn frame(&mut self) {
        self.app.tick();
        self.scene.clear();
        let size = (W as f32 / SCALE, H as f32 / SCALE);
        set_window_size(size.0, size.1);
        set_theme(self.theme);
        let main: E = if self.landing { draft_landing(&mut self.app) } else { div().flex1().min_h(0.0) };
        let composer_view = composer(&mut self.app, ComposerProps::default());
        let root = col::<App>().size(size.0, size.1).bg(self.theme.layer).child(col().flex1().min_h(0.0).child(main)).child(composer_view);
        let mut clipboard = NoClipboard;
        let time = self.started.elapsed().as_secs_f32();
        let mut ui = Ui::new(&mut self.scene, &mut self.text, self.theme, &self.input, &mut self.mem, &mut clipboard, SCALE, size, 0.016, time);
        render(root, &mut self.app, &mut ui);
        self.input.end_frame();
        self.text.flush_atlases(&self.renderer);
        let view = self.target.create_view(&Default::default());
        let submission = self.renderer.render(&self.scene, &self.theme, &view, (W, H));
        let _ = self.gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(Duration::from_secs(5)) });
    }

    fn settle(&mut self, ms: u64) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(ms) {
            self.frame();
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    fn save(&mut self, name: &str) {
        self.settle(120);
        let device = &self.gpu.device;
        let bytes_per_row = (W * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (bytes_per_row * H) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &self.target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: None } },
            wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        );
        self.gpu.queue.submit([encoder.finish()]);
        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(Duration::from_secs(5)) });
        let data = buffer.slice(..).get_mapped_range().expect("mapped");
        let mut image = image::RgbaImage::new(W, H);
        for y in 0..H {
            for x in 0..W {
                let at = (y * bytes_per_row + x * 4) as usize;
                image.put_pixel(x, y, image::Rgba([data[at + 2], data[at + 1], data[at], 255]));
            }
        }
        let path = self.out.join(format!("{name}.png"));
        image.save(&path).expect("save");
        println!("wrote {}", path.display());
    }

    fn click_at(&mut self, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame();
        self.input.pressed = true;
        self.input.down = true;
        self.frame();
        self.input.down = false;
        self.input.released = true;
        self.frame();
        self.input.pointer = Some((2.0, 2.0));
        self.frame();
    }

    fn click(&mut self, key: u64) {
        self.frame();
        let r = rect_of(key).expect("on screen");
        self.click_at(r.x + r.w * 0.5, r.y + r.h * 0.5);
    }

    fn close_menus(&mut self) {
        self.click_at(W as f32 / SCALE - 10.0, 12.0);
        self.settle(40);
    }

    fn id(&mut self) -> String {
        self.app.active_conversation_id().unwrap()
    }
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let picture = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 150, 255]));
    let mut bytes = Vec::new();
    picture.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
    bytes
}

fn git(repo: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").args(args).current_dir(repo).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn long_text() -> String {
    (0..30).map(|index| format!("line {index} {}", "x".repeat(40))).collect::<Vec<_>>().join("\n")
}

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".to_owned()));
    std::fs::create_dir_all(&out).unwrap();
    let dark = std::env::args().nth(2).as_deref() != Some("light");
    let chinese = std::env::args().nth(3).as_deref() == Some("zh");
    let root = std::env::temp_dir().join(format!("mewrk-composer-shot-{}", std::process::id()));
    let (data, project, home) = (root.join("data"), root.join("project"), root.join("home"));
    for dir in [&data, &project, &home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let project = project.canonicalize().unwrap();
    git(&project, &["init", "-b", "main"]);
    std::fs::write(project.join("README.md"), "# readme\n").unwrap();
    git(&project, &["add", "."]);
    git(&project, &["commit", "-m", "first"]);
    let document = json!({
        "schemaVersion": 6,
        "globalSettings": {},
        "workspaces": [{ "id": WS, "name": "chromewm", "kind": "directory", "path": project.to_str().unwrap(), "createdAt": "2026-01-01T00:00:00.000Z", "defaultConversationPresetId": "", "conversations": [] }],
        "tools": [{ "name": "read", "label": "Read", "description": "", "category": "filesystem", "dangerous": false, "parameters": [] }],
        "capabilities": { "skills": [], "mcps": [] },
    });
    std::fs::write(data.join("document.v1.json"), serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    // SAFETY: nothing else runs yet.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("MEWRK_LANG", if chinese { "zh-CN" } else { "en" });
    }
    let mut app = App::start(StartOptions { identifier: "mewrk.composer.shot".into(), version: "0.0.0".into(), data_dir: data, resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).expect("the host starts");
    mewrk_client::doc::init(&mut app);
    mewrk_client::i18n::set_english(!chinese);
    let provider: mewrk_client::model::ApiProvider = serde_json::from_value(json!({
        "id": "provider-1", "name": "Anthropic", "enabled": true, "family": "anthropic", "baseUrl": "http://localhost:9",
        "models": [{ "id": "claude-sonnet-5", "name": "", "group": "", "capabilities": ["image_recognition", "tool_append", "native_compaction"], "reasoningContent": "plaintext", "contextWindow": 200000 }, { "id": "claude-haiku-5", "name": "", "group": "", "capabilities": ["image_recognition"], "reasoningContent": "plaintext" }],
        "activeModelId": "claude-sonnet-5"
    })).unwrap();
    let other: mewrk_client::model::ApiProvider = serde_json::from_value(json!({
        "id": "provider-2", "name": "OpenRouter", "enabled": true, "family": "openai_chat", "baseUrl": "http://localhost:9",
        "models": [{ "id": "qwen3-coder", "name": "", "group": "", "capabilities": [], "reasoningContent": "plaintext" }], "activeModelId": "qwen3-coder"
    })).unwrap();
    app.doc.assets.api_providers = vec![provider, other];
    app.doc.global_settings.active_provider_id = Some("provider-1".to_owned());
    app.doc.global_settings.appearance.send_shortcut = vec!["Enter".to_owned()];
    app.doc.global_settings.appearance.newline_shortcut = vec!["Shift".to_owned(), "Enter".to_owned()];
    let tasks: Vec<mewrk_client::model::Conversation> = [("Fix the footer spacing", "2026-11-04T09:00:00.000Z"), ("Port the composer", "2026-11-03T09:00:00.000Z"), ("Read the review pane", "2026-10-20T09:00:00.000Z")]
        .iter()
        .enumerate()
        .map(|(index, (title, at))| serde_json::from_value(json!({ "id": format!("task-{index}"), "title": title, "createdAt": at, "updatedAt": at, "settings": { "enabledTools": ["read"] }, "contexts": [{ "kind": "user", "id": format!("u{index}"), "content": "hi", "createdAt": at }] })).unwrap())
        .collect();
    app.update_document(|doc| doc.workspaces.iter_mut().find(|workspace| workspace.id == WS).unwrap().conversations = tasks);
    app.open_draft_conversation(Some(WS));

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let gpu = Arc::new(Gpu::new(instance, None).expect("a GPU"));
    let renderer = Renderer::new(gpu.clone());
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor { label: Some("shot"), size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: SURFACE_FORMAT, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
    let theme = if dark { Theme::dark() } else { Theme::light() };
    let mut shot = Shot { gpu, renderer, target, scene: Scene::default(), text: TextCtx::new(SCALE), mem: UiMemory::default(), input: Input::default(), app, theme, out, landing: true, started: Instant::now() };
    shot.frame();

    // ---- the landing, with its git chips read from the real repository
    let id = shot.id();
    chips::refresh_git_snapshot(&mut shot.app, mewrk_pane_git::git_snapshot_key(&id, 1), mewrk_pane_git::git_surface_key(WS, 1), mewrk_pane_git::git_workspace_target(WS, Some(1)));
    shot.settle(400);
    shot.save("01-landing");

    // ---- attachments and chips in the box
    let conversation = shot.id();
    shot.app.add_composer_attachments(&conversation, vec![ComposerFile { name: "screenshot.png".into(), media_type: "image/png".into(), bytes: png(320, 200) }, ComposerFile { name: "sunset.png".into(), media_type: "image/png".into(), bytes: png(80, 200) }, ComposerFile { name: "notes.md".into(), media_type: String::new(), bytes: b"# notes\nhello there\n".to_vec() }], Vec::new());
    let block = "<mewrk-selected-element>\n<element tag=\"button\" has-screenshot=\"false\" class=\"btn btn-primary btn-lg\">\n  <text>Place order</text>\n</element>\n</mewrk-selected-element>".to_owned();
    stage_element_pick(&mut shot.app, &conversation, block, None);
    shot.app.composer().attachment_notices.insert(conversation.clone(), vec![mewrk_client::runs::AttachmentRejection { name: Some("archive.zip".into()), reason: mewrk_client::runs::AttachmentRejectionReason::Unsupported }]);
    // A long paste through the same path a person's takes: a tag in the middle of what they typed.
    let pasted = long_text();
    let id_for_paste = conversation.clone();
    let label = pasted_text::pasted_text_label(1, pasted_text::pasted_text_extra_lines(&pasted));
    shot.app.set_composer_pasted_texts(&id_for_paste, vec![mewrk_client::runs::PastedText { id: 1, label: label.clone(), content: pasted }]);
    shot.app.set_composer_draft(&id_for_paste, format!("Look at this log:{label}and tell me what went wrong.\nSecond line, 你好世界."));
    shot.landing = false;
    shot.settle(800);
    focus_composer();
    shot.save("02-composer-attachments");

    // ---- menus
    shot.click(trigger_key(chips::PROJECT_MENU));
    shot.save("03-project-menu");
    shot.close_menus();
    shot.click(trigger_key(chips::WORKSPACE_MENU));
    shot.save("04-workspace-menu");
    shot.close_menus();
    shot.click(trigger_key(footer::SECURITY_MENU));
    shot.save("05-security-menu");
    shot.close_menus();
    shot.click(trigger_key(footer::MODEL_MENU));
    shot.save("06-model-menu");
    shot.close_menus();
    shot.click(trigger_key(footer::EFFORT_MENU));
    shot.save("07-effort-menu");
    shot.close_menus();
    shot.click(trigger_key((footer::ADD_MENU, conversation.as_str())));
    shot.save("08-add-menu");
    shot.close_menus();
    shot.click(meter::meter_trigger_key());
    shot.save("09-context-meter");
    shot.click(meter::compact_row_key());
    shot.save("10-context-meter-compact");
    shot.close_menus();


    // ---- docks
    let base = PendingToolPrompt { prompt_id: "p1".into(), tool_name: "run".into(), kind: PromptKind::Tool, label: "Run a shell command".into(), summary: "cargo test -p mewrk-ui-composer\nrm -rf target/debug/incremental".into(), risk_level: "High".into(), reason: "It deletes files in the workspace.".into(), requester: None, source_agent: None, source_call_id: None, allow_always_offered: true, mandatory: false, questions: None };
    shot.app.runs().tool_prompts.insert(conversation.clone(), vec![base.clone(), PendingToolPrompt { prompt_id: "p2".into(), ..base.clone() }]);
    shot.save("11-approval-dock");
    shot.app.runs().tool_prompts.insert(conversation.clone(), vec![PendingToolPrompt { prompt_id: "plan".into(), kind: PromptKind::PlanExit, ..base.clone() }]);
    shot.save("12-plan-dock");
    let questions = json!([
        { "question": "Which database should this use?", "header": "Database", "options": [{ "label": "SQLite", "description": "One file, no server." }, { "label": "Postgres", "description": "A server, and everything that comes with it." }] },
        { "question": "Which of these should run?", "header": "Checks", "multiSelect": true, "options": ["Lint", "Tests", "Types"] },
        { "question": "Which layout?", "header": "Layout", "options": [{ "label": "Sidebar", "preview": "| nav | body |\n" }, { "label": "Top bar", "preview": "| nav |\n| body |\n" }] }
    ]);
    shot.app.runs().tool_prompts.insert(conversation.clone(), vec![PendingToolPrompt { prompt_id: "q".into(), kind: PromptKind::Question, questions: Some(questions), ..base.clone() }]);
    shot.save("13-question-dock");
    shot.click(hash_key(("question-tab", "q", 1usize)));
    shot.save("14-question-multi");
    shot.click(hash_key(("question-tab", "q", 2usize)));
    shot.save("15-question-preview");
    shot.click(hash_key(("question-tab", "q", 3usize)));
    shot.save("16-question-review");
    shot.app.runs().tool_prompts.remove(&conversation);

    // ---- a drag over the window, and what the box says when a message cannot go
    let dropped = project.join("README.md");
    shot.input.pointer = None;
    shot.input.hovered_file = true;
    shot.input.hovered_paths = vec![dropped];
    shot.settle(200);
    shot.save("18-drop-overlay");
    shot.input.hovered_file = false;
    shot.input.hovered_paths.clear();
    shot.frame();
    shot.input.pointer = Some((2.0, 2.0));
    shot.app.update_document(|doc| {
        doc.assets.api_providers[0].models[0].capabilities.clear();
    });
    shot.save("19-images-unsupported");
    let first = shot.app.composer().image_drafts.get(&conversation).and_then(|images| images.first().cloned());
    if let Some(image) = first {
        shot.app.update_document(|doc| {
            doc.assets.api_providers[0].models = doc.assets.api_providers[1].models.iter().cloned().chain(doc.assets.api_providers[0].models.iter().cloned()).collect();
        });
        let strip = format!("composer:{conversation}");
        let chip = rect_of(image_strip::image_key(&strip, &image.id)).expect("the chip");
        shot.click_at(chip.x + 10.0, chip.y + 30.0);
        shot.save("20-image-viewer");
        shot.input.keys.push(fluent::ui::KeyPress { key: Key::Escape, shift: false, cmd: false, alt: false, ctrl: false });
        shot.settle(40);
    }

    // ---- what a redeemed conversation shows
    shot.frame();
    let queued: Vec<mewrk_client::model::QueuedMessage> = serde_json::from_value(json!([{ "id": "m1", "content": "and also fix the footer", "createdAt": "2026-01-01T00:00:00Z" }, { "id": "m2", "content": "", "createdAt": "2026-01-01T00:00:00Z", "images": [{ "id": "i", "name": "shot.png", "mime": "image/png", "width": 1, "height": 1, "bytes": 1 }] }])).unwrap();
    shot.app.update_document(|doc| {
        let workspace = doc.workspaces.iter_mut().find(|workspace| workspace.id == WS).unwrap();
        let mut conversation: mewrk_client::model::Conversation = serde_json::from_value(json!({ "id": "c2", "title": "A task", "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z", "settings": { "enabledTools": ["read"] }, "contexts": [{ "kind": "user", "id": "u1", "content": "hi", "createdAt": "2026-01-01T00:00:00Z" }] })).unwrap();
        conversation.queued_messages = queued;
        conversation.attached_workspaces = vec![mewrk_client::model::AttachedWorkspace { machine: None, path: "/Users/someone/projects/another-repository".into() }];
        workspace.conversations.insert(0, conversation);
    });
    shot.app.select_conversation(WS, "c2");
    shot.app.composer().failed_queued_promotion_ids.insert("m1".to_owned());
    shot.settle(100);
    shot.save("17-queued-and-attached");

    let _ = std::fs::remove_dir_all(&root);
    shot.app.host.finalize();
}
