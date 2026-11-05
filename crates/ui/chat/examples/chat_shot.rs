//! Renders the timeline through the real GPU path to PNGs, for looking at, and times frames over a long timeline.
//! `cargo run -p mewrk-ui-chat --example chat_shot -- <out dir> [dark|light]`, `cargo run --release -p mewrk-ui-chat --example chat_shot -- --perf`.

#[path = "../tests/suite/fixture.rs"]
mod fixture;

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fluent::el::*;
use fluent::gpu::SURFACE_FORMAT;
use fluent::ui::{Input, NoClipboard, UiMemory};
use fluent::{Gpu, Renderer, Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::E;
use mewrk_client::{App, StartOptions};
use mewrk_ui_chat::*;

const W: u32 = 1400;
const H: u32 = 1800;
const SCALE: f32 = 2.0;

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
    started: Instant,
}

impl Shot {
    fn frame(&mut self, view: &mut dyn FnMut(&mut App) -> E) {
        self.app.tick();
        self.scene.clear();
        let size = (W as f32 / SCALE, H as f32 / SCALE);
        set_window_size(size.0, size.1);
        set_theme(self.theme);
        let body = view(&mut self.app);
        let root = col::<App>().size(size.0, size.1).bg(self.theme.layer).child(body);
        let mut clipboard = NoClipboard;
        let time = self.started.elapsed().as_secs_f32();
        let mut ui = Ui::new(&mut self.scene, &mut self.text, self.theme, &self.input, &mut self.mem, &mut clipboard, SCALE, size, 0.016, time);
        render(root, &mut self.app, &mut ui);
        self.input.end_frame();
        self.text.flush_atlases(&self.renderer);
        let target_view = self.target.create_view(&Default::default());
        let submission = self.renderer.render(&self.scene, &self.theme, &target_view, (W, H));
        let _ = self.gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(Duration::from_secs(5)) });
    }

    fn settle(&mut self, view: &mut dyn FnMut(&mut App) -> E, ms: u64) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(ms) {
            self.frame(view);
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    fn save(&mut self, path: &str) {
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
        image.save(path).expect("save");
        println!("wrote {path}");
    }
}

fn start(theme: Theme) -> (Shot, std::path::PathBuf) {
    // SAFETY: before any host thread exists.
    unsafe { std::env::set_var("MEWRK_LANG", "en") };
    let data = std::env::temp_dir().join(format!("mewrk-chat-shot-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let app = App::start(StartOptions { identifier: "dev.mewrk.shot".to_owned(), version: "0.0.0".to_owned(), data_dir: data.clone(), resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let gpu = Arc::new(Gpu::new(instance, None).expect("a GPU"));
    let renderer = Renderer::new(gpu.clone());
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor { label: Some("shot"), size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: SURFACE_FORMAT, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
    (Shot { gpu, renderer, target, scene: Scene::default(), text: TextCtx::new(SCALE), mem: UiMemory::default(), input: Input::default(), app, theme, started: Instant::now() }, data)
}

/// A handler that does nothing, so every affordance is drawn.
fn all_affordances(contexts: Rc<Vec<mewrk_client::runs::LiveContext>>) -> ConversationViewProps {
    ConversationViewProps {
        contexts,
        timeline_id: Some("shot".to_owned()),
        tools: Rc::new(fixture::descriptors()),
        enabled_tools: Rc::new(vec!["read".to_owned(), "write".to_owned(), "bash".to_owned()]),
        on_edit: Some(Rc::new(|_, _| {})),
        on_delete: Some(Rc::new(|_, _| {})),
        on_insert: Some(Rc::new(|_, _, _, _| {})),
        on_delete_contexts: Some(Rc::new(|_, _| {})),
        on_move_contexts: Some(Rc::new(|_, _, _| {})),
        on_copy_contexts: Some(Rc::new(|_, _| {})),
        on_paste_contexts: Some(Rc::new(|_, _, _| {})),
        on_fork_at: Some(Rc::new(|_, _| {})),
        on_save_tool: Some(Rc::new(|_, _, _, _| {})),
        ..ConversationViewProps::default()
    }
}

fn perf() {
    let (mut shot, data) = start(Theme::dark());
    for count in [20_000usize] {
        let contexts = Rc::new(fixture::long_timeline(count));
        let props = all_affordances(contexts);
        let mut view = |app: &mut App| conversation_view(app, props.clone());
        let opened = Instant::now();
        shot.frame(&mut view);
        println!("{count} cards: first frame {:?}", opened.elapsed());
        shot.settle(&mut view, 300);
        // Steady, then scrolling by pixels, then a far jump, as a window would see them.
        let mut time = |shot: &mut Shot, label: &str, frames: usize, step: &mut dyn FnMut(&mut Shot, usize)| {
            let mut samples = Vec::with_capacity(frames);
            for at in 0..frames {
                step(shot, at);
                let started = Instant::now();
                // Build, layout, paint and handlers: everything but handing the frame to the GPU.
                shot.app.tick();
                shot.scene.clear();
                let size = (W as f32 / SCALE, H as f32 / SCALE);
                let body = view(&mut shot.app);
                let root = col::<App>().size(size.0, size.1).child(body);
                let mut clipboard = NoClipboard;
                let mut ui = Ui::new(&mut shot.scene, &mut shot.text, shot.theme, &shot.input, &mut shot.mem, &mut clipboard, SCALE, size, 0.016, 0.0);
                render(root, &mut shot.app, &mut ui);
                shot.input.end_frame();
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(|a, b| a.total_cmp(b));
            println!("{label}: median {:.2} ms, p99 {:.2} ms, worst {:.2} ms over {frames} frames", samples[frames / 2], samples[frames * 99 / 100], samples[frames - 1]);
        };
        time(&mut shot, "steady", 200, &mut |_, _| {});
        time(&mut shot, "scrolling", 600, &mut |shot, at| {
            shot.input.pointer = Some((300.0, 300.0));
            shot.input.scroll = if at < 300 { 60.0 } else { -60.0 };
            shot.input.scroll_is_pixels = true;
        });
        let key = hash_key(("context-scroll", "shot"));
        time(&mut shot, "far jumps", 60, &mut |_, at| scroll_request(key, ScrollRequest::Shift(if at % 2 == 0 { -400_000.0 } else { 390_000.0 })));
    }
    shot.app.host.finalize();
    let _ = std::fs::remove_dir_all(data);
}

fn click(shot: &mut Shot, view: &mut dyn FnMut(&mut App) -> E, x: f32, y: f32) {
    shot.input.pointer = Some((x, y));
    shot.frame(view);
    shot.input.pressed = true;
    shot.input.down = true;
    shot.frame(view);
    shot.input.down = false;
    shot.input.released = true;
    shot.frame(view);
    shot.settle(view, 60);
}

fn card(id: &str) -> fluent::ui::Rect {
    rect_of(mewrk_ui_chat::frame::unit_key(0, id)).unwrap_or_else(|| panic!("{id} is not built"))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--perf") {
        return perf();
    }
    let out = args.get(1).cloned().unwrap_or_else(|| ".".to_owned());
    let dark = args.get(2).map(String::as_str) != Some("light");
    let (mut shot, data) = start(if dark { Theme::dark() } else { Theme::light() });
    let suffix = if dark { "dark" } else { "light" };
    let props = all_affordances(Rc::new(fixture::every_kind()));
    {
        let mut view = |app: &mut App| conversation_view(app, props.clone());
        shot.settle(&mut view, 500);
        let key = hash_key(("context-scroll", "shot"));
        scroll_request(key, ScrollRequest::Shift(-100_000.0));
        shot.settle(&mut view, 400);
        shot.save(&format!("{out}/timeline-top-{suffix}.png"));
        scroll_request(key, ScrollRequest::Shift(100_000.0));
        shot.settle(&mut view, 400);
        shot.save(&format!("{out}/timeline-bottom-{suffix}.png"));

        // Every row open, top to bottom.
        scroll_request(key, ScrollRequest::Shift(-100_000.0));
        shot.settle(&mut view, 300);
        for id in ["t-mcp", "t-web", "t-fail", "t-bash", "t-edit", "t-grep", "t-read", "r1"] {
            let r = card(id);
            click(&mut shot, &mut view, r.x + r.w * 0.8, r.y + 12.0);
        }
        let r = card("t-read");
        shot.input.pointer = Some((r.x + 200.0, r.y + 12.0));
        shot.settle(&mut view, 200);
        shot.save(&format!("{out}/rows-open-{suffix}.png"));
        for id in ["t-mcp", "t-web", "t-fail", "t-bash", "t-edit", "t-grep", "t-read", "r1"] {
            let r = card(id);
            click(&mut shot, &mut view, r.x + r.w * 0.8, r.y + 12.0);
        }

        // A right click: the menu with its tool submenu, and the insertion line.
        let r = card("t-ls");
        shot.input.pointer = Some((r.x + 200.0, r.y + 22.0));
        shot.frame(&mut view);
        shot.input.right_pressed = true;
        shot.settle(&mut view, 120);
        let menu = mewrk_ui_common::context_menu::menu_row_key(("tl-menu", key), &[], "tool");
        let row = rect_of(menu).expect("the tool row");
        shot.input.pointer = Some((row.x + 20.0, row.y + 10.0));
        shot.settle(&mut view, 400);
        shot.save(&format!("{out}/menu-{suffix}.png"));
        click(&mut shot, &mut view, 1.0, 1.0);

        // A box over some rows, then a drag of what it picked.
        let (top, bottom) = (card("t-grep"), card("t-bash"));
        shot.input.zoom_modifier = true;
        shot.input.pointer = Some((top.x + 300.0, top.y + 4.0));
        shot.frame(&mut view);
        shot.input.pressed = true;
        shot.input.down = true;
        shot.frame(&mut view);
        shot.input.pointer = Some((bottom.x + 500.0, bottom.y + 20.0));
        shot.settle(&mut view, 100);
        shot.save(&format!("{out}/marquee-{suffix}.png"));
        shot.input.down = false;
        shot.input.released = true;
        shot.frame(&mut view);
        shot.input.zoom_modifier = false;
        shot.settle(&mut view, 60);
        let held = card("t-ls");
        shot.input.pointer = Some((held.x + 200.0, held.y + 12.0));
        shot.frame(&mut view);
        shot.input.pressed = true;
        shot.input.down = true;
        shot.frame(&mut view);
        shot.input.pointer = Some((held.x + 260.0, held.y + 340.0));
        shot.settle(&mut view, 400);
        shot.save(&format!("{out}/drag-{suffix}.png"));
        shot.input.down = false;
        shot.input.released = true;
        shot.frame(&mut view);
        shot.settle(&mut view, 60);
    }

    // Editors in place, and a live round.
    let contexts = Rc::new(fixture::every_kind());
    let item = contexts[2].item.clone();
    let mut editing = all_affordances(contexts.clone());
    editing.timeline_id = Some("editing".to_owned());
    editing.on_cancel_edit = Some(Rc::new(|_| {}));
    editing.on_save_text = Some(Rc::new(|_, _, _, _| {}));
    editing.editor = Some(TimelineEditorState::Edit { kind: inline_text_editor::InsertKind::User, item, index: 2 });
    {
        let mut view = |app: &mut App| conversation_view(app, editing.clone());
        shot.settle(&mut view, 300);
        scroll_request(hash_key(("context-scroll", "editing")), ScrollRequest::Shift(-100_000.0));
        shot.settle(&mut view, 300);
        shot.save(&format!("{out}/edit-text-{suffix}.png"));
    }
    let tool = contexts[5].item.clone();
    let mut tool_editing = editing.clone();
    tool_editing.timeline_id = Some("tool-editing".to_owned());
    tool_editing.on_save_tool_edit = Some(Rc::new(|_, _, _, _, _| {}));
    tool_editing.editor = Some(TimelineEditorState::Edit { kind: inline_text_editor::InsertKind::Tool, item: tool, index: 5 });
    {
        let mut view = |app: &mut App| conversation_view(app, tool_editing.clone());
        shot.settle(&mut view, 300);
        scroll_request(hash_key(("context-scroll", "tool-editing")), ScrollRequest::Shift(-100_000.0));
        shot.settle(&mut view, 300);
        shot.save(&format!("{out}/edit-tool-{suffix}.png"));
    }
    let mut live_round: Vec<_> = fixture::every_kind().into_iter().take(4).collect();
    live_round.push(fixture::streaming(fixture::reasoning("r-live", Some("Reading the next file now, to see how it is used.")), None));
    live_round.push(fixture::streaming(fixture::tool("t-live", "bash", serde_json::json!({ "command": "cargo test --all" }), ""), Some(mewrk_client::runs::StreamingToolPhase::Running)));
    live_round.push(fixture::streaming(fixture::assistant("a-live", "Still writing this **reply**, and"), None));
    let mut streaming = all_affordances(Rc::new(live_round));
    streaming.timeline_id = Some("live".to_owned());
    streaming.streaming = true;
    streaming.thinking = Some(mewrk_client::runs::LiveReasoningView { started_at: "2026-11-04T00:00:00.000Z".to_owned(), tokens: 1234 });
    streaming.running_task_count = 2;
    streaming.on_open_tasks = Some(Rc::new(|_| {}));
    {
        let mut view = |app: &mut App| conversation_view(app, streaming.clone());
        shot.settle(&mut view, 600);
        shot.save(&format!("{out}/streaming-{suffix}.png"));
    }
    shot.app.host.finalize();
    let _ = std::fs::remove_dir_all(data);
}
