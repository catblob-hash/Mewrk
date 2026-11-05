//! The surfaces beside the pane, driven the same way: the sketch overlay by pointer and key, an attachment's dialog and a
//! native drop against the real host.

#[path = "rig/mod.rs"]
mod rig;

use fluent::el::*;
use fluent::ui::{Input, Key, KeyPress, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::model::{FileAttachment, FileAttachmentFormat};
use mewrk_client::App;
use mewrk_pane_files::attachment_drop::*;
use mewrk_pane_files::attachment_preview::*;
use mewrk_pane_files::attachments::*;
use mewrk_pane_files::sketch::strokes::*;
use mewrk_pane_files::sketch::*;
use rig::*;
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

struct Frames {
    app: App,
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: TestClipboard,
    _data: TempDir,
}

impl Frames {
    fn new() -> Self {
        let data = TempDir::new("surf");
        let app = start_app(&data.0);
        Self { app, scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: TestClipboard::default(), _data: data }
    }

    fn frame(&mut self, view: &dyn Fn(&mut App) -> El<App>) {
        self.app.tick();
        self.scene.clear();
        set_window_size(800.0, 600.0);
        let root = view(&mut self.app);
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, &mut self.clipboard, 1.0, (800.0, 600.0), 0.016, 0.0);
        render(root, &mut self.app, &mut ui);
        self.input.end_frame();
    }

    fn drag(&mut self, view: &dyn Fn(&mut App) -> El<App>, from: (f32, f32), to: (f32, f32)) {
        self.input.pointer = Some(from);
        self.frame(view);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(view);
        self.input.pointer = Some(to);
        self.frame(view);
        self.input.down = false;
        self.input.released = true;
        self.frame(view);
        self.frame(view);
    }

    fn click(&mut self, view: &dyn Fn(&mut App) -> El<App>, r: fluent::Rect) {
        let at = (r.x + r.w * 0.5, r.y + r.h * 0.5);
        self.drag(view, at, at);
    }

    fn until(&mut self, what: &str, view: &dyn Fn(&mut App) -> El<App>, done: impl Fn() -> bool) {
        let started = Instant::now();
        while !done() {
            self.frame(view);
            assert!(started.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
        self.frame(view);
    }
}

fn overlay_view(key: &'static str, attached: Rc<RefCell<Option<String>>>, cancelled: Rc<Cell<u32>>) -> impl Fn(&mut App) -> El<App> {
    move |app: &mut App| {
        let (attached, cancelled) = (attached.clone(), cancelled.clone());
        div().size(800.0, 600.0).child(sketch_overlay(app, SketchOverlayProps {
            key: key.to_owned(),
            backdrop: None,
            on_cancel: Rc::new(move |_| cancelled.set(cancelled.get() + 1)),
            on_attach: Rc::new(move |_, url| *attached.borrow_mut() = Some(url)),
            attach_label: None,
        }))
    }
}

#[test]
fn draws_with_the_pointer_exports_a_png_and_keeps_the_primary_action_off_until_then() {
    let mut f = Frames::new();
    let (attached, cancelled) = (Rc::new(RefCell::new(None)), Rc::new(Cell::new(0)));
    let view = overlay_view("draw", attached.clone(), cancelled.clone());
    f.frame(&view);
    f.frame(&view);
    // Nothing to attach, so a press on the primary action does nothing.
    let attach = rect_of(("sketch-attach", "draw")).expect("the toolbar");
    f.click(&view, attach);
    assert!(attached.borrow().is_none());
    f.drag(&view, (100.0, 100.0), (300.0, 200.0));
    let sketch = sketch_of("draw");
    assert_eq!(sketch.borrow().history.present.len(), 1);
    assert_eq!(sketch.borrow().size, (800.0, 600.0));
    let attach = rect_of(("sketch-attach", "draw")).expect("the toolbar");
    f.click(&view, attach);
    f.until("the export", &view, || attached.borrow().is_some());
    assert!(attached.borrow().as_ref().unwrap().starts_with("data:image/png;base64,"));
}

#[test]
fn closes_without_confirming_while_unmarked_and_asks_before_discarding_marks() {
    let mut f = Frames::new();
    let (attached, cancelled) = (Rc::new(RefCell::new(None)), Rc::new(Cell::new(0)));
    let view = overlay_view("close", attached, cancelled.clone());
    f.frame(&view);
    f.frame(&view);
    f.drag(&view, (100.0, 100.0), (300.0, 200.0));
    let close = rect_of(("sketch-close", "close")).unwrap();
    f.click(&view, close);
    assert_eq!(cancelled.get(), 0, "marks exist, so it asks");
    assert!(sketch_of("close").borrow().confirming);
    f.frame(&view);
    f.frame(&view);
    let keep = rect_of(("sketch-keep", "close")).expect("the dialog");
    f.click(&view, keep);
    assert!(!sketch_of("close").borrow().confirming);
    let close = rect_of(("sketch-close", "close")).unwrap();
    f.click(&view, close);
    f.frame(&view);
    f.frame(&view);
    let discard = rect_of(("sketch-discard", "close")).expect("the dialog");
    f.click(&view, discard);
    assert_eq!(cancelled.get(), 1);
    // An unmarked page closes at once.
    f.frame(&view);
    f.frame(&view);
    let close = rect_of(("sketch-close", "close")).unwrap();
    f.click(&view, close);
    assert_eq!(cancelled.get(), 2);
}

#[test]
fn selects_a_tool_and_an_ink_from_the_rows_and_undoes_from_the_keyboard() {
    let mut f = Frames::new();
    let view = overlay_view("rows", Rc::new(RefCell::new(None)), Rc::new(Cell::new(0)));
    f.frame(&view);
    f.frame(&view);
    let arrow = rect_of(("sketch-tool", "rows", format!("{:?}", SketchTool::Arrow))).unwrap();
    f.click(&view, arrow);
    assert_eq!(sketch_of("rows").borrow().tool, SketchTool::Arrow);
    let green = rect_of(("sketch-color", "rows", 0x2F9E44u32)).unwrap();
    f.click(&view, green);
    assert_eq!(sketch_of("rows").borrow().color, 0x2F9E44);
    f.drag(&view, (50.0, 50.0), (150.0, 150.0));
    assert!(matches!(&sketch_of("rows").borrow().history.present[0], Stroke::Shape { kind: Shape::Arrow, color: 0x2F9E44, .. }));
    // Focus inside the overlay is what its keys need, as a click on it gives.
    focus_request(("sketch", "rows"));
    f.frame(&view);
    f.input.keys.push(KeyPress { key: Key::Char('z'), shift: false, cmd: true, alt: false, ctrl: false });
    f.frame(&view);
    f.frame(&view);
    assert!(sketch_of("rows").borrow().history.present.is_empty());
    f.input.keys.push(KeyPress { key: Key::Char('z'), shift: true, cmd: true, alt: false, ctrl: false });
    f.frame(&view);
    f.frame(&view);
    assert_eq!(sketch_of("rows").borrow().history.present.len(), 1);
}

#[test]
fn types_a_label_with_the_text_tool_commits_on_ctrl_enter_and_drops_on_escape() {
    let mut f = Frames::new();
    let view = overlay_view("label", Rc::new(RefCell::new(None)), Rc::new(Cell::new(0)));
    f.frame(&view);
    f.frame(&view);
    let text_tool = rect_of(("sketch-tool", "label", format!("{:?}", SketchTool::Text))).unwrap();
    f.click(&view, text_tool);
    f.drag(&view, (60.0, 60.0), (60.0, 60.0));
    f.frame(&view);
    assert!(sketch_of("label").borrow().draft.is_some());
    f.input.text.push_str("look");
    f.frame(&view);
    f.frame(&view);
    assert_eq!(sketch_of("label").borrow().draft.as_ref().unwrap().value, "look");
    f.input.keys.push(KeyPress { key: Key::Enter, shift: false, cmd: true, alt: false, ctrl: false });
    f.frame(&view);
    f.frame(&view);
    assert!(sketch_of("label").borrow().draft.is_none());
    assert!(matches!(&sketch_of("label").borrow().history.present[0], Stroke::Text { text, .. } if text == "look"));
    f.drag(&view, (90.0, 90.0), (90.0, 90.0));
    f.frame(&view);
    f.input.text.push_str("never mind");
    f.frame(&view);
    f.frame(&view);
    f.input.keys.push(KeyPress { key: Key::Escape, shift: false, cmd: false, alt: false, ctrl: false });
    f.frame(&view);
    f.frame(&view);
    assert!(sketch_of("label").borrow().draft.is_none());
    assert_eq!(sketch_of("label").borrow().history.present.len(), 1);
}

#[test]
fn a_label_is_typed_in_the_ink_and_the_keys_come_back_to_the_overlay_afterwards() {
    let mut f = Frames::new();
    let view = overlay_view("back", Rc::new(RefCell::new(None)), Rc::new(Cell::new(0)));
    f.frame(&view);
    f.frame(&view);
    f.drag(&view, (20.0, 20.0), (80.0, 80.0));
    let text_tool = rect_of(("sketch-tool", "back", format!("{:?}", SketchTool::Text))).unwrap();
    f.click(&view, text_tool);
    f.drag(&view, (60.0, 60.0), (60.0, 60.0));
    f.frame(&view);
    f.input.text.push_str("look");
    f.frame(&view);
    f.frame(&view);
    // While it is being typed a shortcut is the field's, not the sketch's.
    f.input.keys.push(KeyPress { key: Key::Char('z'), shift: false, cmd: true, alt: false, ctrl: false });
    f.frame(&view);
    f.frame(&view);
    assert_eq!(sketch_of("back").borrow().history.present.len(), 1);
    assert_eq!(sketch_of("back").borrow().draft.as_ref().unwrap().value, "", "the field took back what was typed");
    f.input.text.push_str("again");
    f.frame(&view);
    f.frame(&view);
    f.input.keys.push(KeyPress { key: Key::Enter, shift: false, cmd: true, alt: false, ctrl: false });
    f.frame(&view);
    f.frame(&view);
    f.frame(&view);
    assert_eq!(sketch_of("back").borrow().history.present.len(), 2);
    // Focus went back to the surface, so the shortcut now reaches the sketch.
    f.input.keys.push(KeyPress { key: Key::Char('z'), shift: false, cmd: true, alt: false, ctrl: false });
    f.frame(&view);
    f.frame(&view);
    assert_eq!(sketch_of("back").borrow().history.present.len(), 1);
    // And an undo that empties the list leaves the keyboard working when its button disables itself.
    let undo = rect_of(("sketch-undo", "back")).unwrap();
    f.click(&view, undo);
    f.frame(&view);
    assert!(sketch_of("back").borrow().history.present.is_empty());
    f.input.keys.push(KeyPress { key: Key::Char('z'), shift: true, cmd: true, alt: false, ctrl: false });
    f.frame(&view);
    f.frame(&view);
    assert_eq!(sketch_of("back").borrow().history.present.len(), 1, "from a disabled button, the keys still arrive");
}

#[test]
fn a_press_elsewhere_with_the_text_tool_keeps_what_was_typed_and_starts_a_new_label() {
    let mut f = Frames::new();
    let view = overlay_view("next", Rc::new(RefCell::new(None)), Rc::new(Cell::new(0)));
    f.frame(&view);
    f.frame(&view);
    let text_tool = rect_of(("sketch-tool", "next", format!("{:?}", SketchTool::Text))).unwrap();
    f.click(&view, text_tool);
    f.drag(&view, (60.0, 60.0), (60.0, 60.0));
    f.frame(&view);
    f.input.text.push_str("first");
    f.frame(&view);
    f.frame(&view);
    f.drag(&view, (300.0, 200.0), (300.0, 200.0));
    f.frame(&view);
    f.frame(&view);
    assert!(matches!(&sketch_of("next").borrow().history.present[0], Stroke::Text { text, .. } if text == "first"));
    assert_eq!(sketch_of("next").borrow().history.present.len(), 1);
    let draft = sketch_of("next").borrow().draft.clone().expect("a new one, where the press was");
    assert_eq!((draft.at.x, draft.at.y, draft.value.as_str()), (300.0, 200.0, ""));
    f.input.text.push_str("second");
    f.frame(&view);
    f.frame(&view);
    assert_eq!(sketch_of("next").borrow().draft.as_ref().unwrap().value, "second", "with nothing of the last one in it");
}

#[test]
fn holding_shift_snaps_a_drag_on_the_surface() {
    let mut f = Frames::new();
    let view = overlay_view("shift", Rc::new(RefCell::new(None)), Rc::new(Cell::new(0)));
    f.frame(&view);
    f.frame(&view);
    let rect_tool = rect_of(("sketch-tool", "shift", format!("{:?}", SketchTool::Rect))).unwrap();
    f.click(&view, rect_tool);
    f.input.shift = true;
    f.drag(&view, (100.0, 100.0), (300.0, 140.0));
    f.input.shift = false;
    assert!(matches!(&sketch_of("shift").borrow().history.present[0], Stroke::Shape { to, .. } if *to == Point { x: 300.0, y: 300.0 }));
}

#[test]
fn collapses_the_tool_and_colour_rows_into_menus_when_the_pill_overflows() {
    let mut f = Frames::new();
    let attached = Rc::new(RefCell::new(None));
    let cancelled = Rc::new(Cell::new(0));
    let view = {
        let (attached, cancelled) = (attached.clone(), cancelled.clone());
        move |app: &mut App| {
            let (attached, cancelled) = (attached.clone(), cancelled.clone());
            div().child(div().size(360.0, 400.0).child(sketch_overlay(app, SketchOverlayProps { key: "narrow".to_owned(), backdrop: None, on_cancel: Rc::new(move |_| cancelled.set(cancelled.get() + 1)), on_attach: Rc::new(move |_, url| *attached.borrow_mut() = Some(url)), attach_label: None })))
        }
    };
    for _ in 0..4 {
        f.frame(&view);
    }
    assert!(rect_of(mewrk_ui_common::popover_menu::trigger_key(("sketch-tools", "narrow"))).is_some(), "the tools became a menu");
    assert!(rect_of(("sketch-tool", "narrow", format!("{:?}", SketchTool::Pen))).is_none());
}

/// What the last frame painted, for a person to look at.
fn shot(f: &mut Frames, renderer: &mut fluent::gpu::Renderer, path: &std::path::Path) {
    let gpu = renderer.gpu.clone();
    f.text.flush_atlases(renderer);
    let size = (800u32, 600u32);
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor { label: Some("shot"), size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: fluent::gpu::SURFACE_FORMAT, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
    let view = target.create_view(&Default::default());
    let _ = renderer.render(&f.scene, &Theme::dark(), &view, size);
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

#[test]
#[ignore = "draws pictures: MEWRK_SHOTS names the folder"]
fn sketch_pictures() {
    let out = std::path::PathBuf::from(std::env::var("MEWRK_SHOTS").expect("MEWRK_SHOTS"));
    // One renderer for all of them: the glyph atlas uploads what changed since the last flush.
    let mut renderer = fluent::gpu::Renderer::new(std::sync::Arc::new(fluent::gpu::Gpu::new(wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()), None).unwrap()));
    let mut f = Frames::new();
    let view = overlay_view("pictures", Rc::new(RefCell::new(None)), Rc::new(Cell::new(0)));
    f.frame(&view);
    f.frame(&view);
    f.drag(&view, (60.0, 60.0), (220.0, 120.0));
    for (name, from, to) in [(SketchTool::Line, (60.0, 160.0), (220.0, 200.0)), (SketchTool::Arrow, (60.0, 230.0), (220.0, 280.0)), (SketchTool::Rect, (280.0, 60.0), (440.0, 160.0)), (SketchTool::Ellipse, (280.0, 190.0), (440.0, 290.0))] {
        let tool = rect_of(("sketch-tool", "pictures", format!("{name:?}"))).unwrap();
        f.click(&view, tool);
        f.drag(&view, from, to);
    }
    let green = rect_of(("sketch-color", "pictures", 0x2F9E44u32)).unwrap();
    f.click(&view, green);
    let text_tool = rect_of(("sketch-tool", "pictures", format!("{:?}", SketchTool::Text))).unwrap();
    f.click(&view, text_tool);
    f.drag(&view, (500.0, 80.0), (500.0, 80.0));
    f.frame(&view);
    f.input.text.push_str("a label");
    f.frame(&view);
    f.frame(&view);
    shot(&mut f, &mut renderer, &out.join("sketch.png"));
    f.input.keys.push(KeyPress { key: Key::Enter, shift: false, cmd: true, alt: false, ctrl: false });
    f.frame(&view);
    f.frame(&view);
    let close = rect_of(("sketch-close", "pictures")).unwrap();
    f.click(&view, close);
    f.frame(&view);
    f.frame(&view);
    shot(&mut f, &mut renderer, &out.join("sketch-confirm.png"));
    close_sketch("pictures");

    let narrow = {
        move |app: &mut App| {
            div().child(div().size(360.0, 400.0).child(sketch_overlay(app, SketchOverlayProps { key: "pictures-narrow".to_owned(), backdrop: None, on_cancel: Rc::new(|_| {}), on_attach: Rc::new(|_, _| {}), attach_label: None })))
        }
    };
    for _ in 0..4 {
        f.frame(&narrow);
    }
    let colors = rect_of(mewrk_ui_common::popover_menu::trigger_key(("sketch-colors", "pictures-narrow"))).unwrap();
    f.click(&narrow, colors);
    f.frame(&narrow);
    f.frame(&narrow);
    shot(&mut f, &mut renderer, &out.join("sketch-narrow.png"));
}

fn upload(f: &mut Frames, name: &str, bytes: &[u8], format: &str) -> FileAttachment {
    let args = json!({ "name": name, "data": mewrk_pane_files::format::base64_encode(bytes), "format": format, "text": if format == "pdf" { Some("[Page 1]\nx") } else { None }, "pages": if format == "pdf" { Some(1) } else { None } });
    let value = f.app.host.invoke_blocking("file_attachment_upload", args).expect("the host stores it");
    serde_json::from_value(value).unwrap()
}

#[test]
fn shows_an_attachment_from_the_store_through_the_files_panes_readers() {
    let mut f = Frames::new();
    let file = upload(&mut f, "notes.md", b"# Title\n\nbody\n", "text");
    assert_eq!(file.format, FileAttachmentFormat::Text);
    let closed = Rc::new(Cell::new(false));
    let view = {
        let (file, closed) = (file.clone(), closed.clone());
        move |app: &mut App| {
            let closed = closed.clone();
            div().size(800.0, 600.0).child(file_attachment_preview(app, &file, false, Rc::new(move |_| closed.set(true))))
        }
    };
    let scroller = format!("doc-scroll/attachment/{}", file.id);
    let probe = scroller.clone();
    f.until("the document", &view, move || rect_of(probe.as_str()).is_some());
    // Markdown offers its source too.
    let toggle = rect_of(("attachment-toggle", format!("attachment/{}", file.id).as_str())).unwrap();
    f.click(&view, toggle);
    f.frame(&view);
    assert!(rect_of(("numbered-code", format!("attachment/{}", file.id))).is_some());
    let close = rect_of(("attachment-close", format!("attachment/{}", file.id).as_str())).unwrap();
    f.click(&view, close);
    assert!(closed.get());
}

#[test]
fn says_when_an_attachment_cannot_be_read_and_retries() {
    let mut f = Frames::new();
    let mut file = upload(&mut f, "gone.txt", b"x", "text");
    file.id = "0".repeat(64);
    let view = {
        let file = file.clone();
        move |app: &mut App| div().size(800.0, 600.0).child(file_attachment_preview(app, &file, false, Rc::new(|_| {})))
    };
    let key = format!("attachment/{}", file.id);
    let probe = key.clone();
    f.until("the retry button", &view, move || rect_of(("attachment-retry", probe.as_str())).is_some());
}

#[test]
fn a_native_drop_is_probed_read_and_sorted_by_the_real_host() {
    let mut f = Frames::new();
    let folder = TempDir::new("drop");
    let (notes, bundle, shot, dir) = (folder.0.join("notes.md"), folder.0.join("bundle.zip"), folder.0.join("shot.png"), folder.0.join("photos"));
    std::fs::write(&notes, "# hi").unwrap();
    std::fs::write(&bundle, [0x50u8, 0x4b, 3, 4, 0, 1]).unwrap();
    std::fs::write(&shot, [0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).unwrap();
    std::fs::create_dir(&dir).unwrap();
    let paths = vec![notes.clone(), bundle.clone(), shot.clone(), dir.clone()];
    let strings: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();

    let dropped = Rc::new(RefCell::new(None));
    let state = Rc::new(RefCell::new(DropZoneState::default()));
    let view = {
        let (dropped, state) = (dropped.clone(), state.clone());
        move |app: &mut App| {
            let dropped = dropped.clone();
            *state.borrow_mut() = drop_zone(app, "zone", false, false, Rc::new(move |_: &mut App, files, rejected| *dropped.borrow_mut() = Some((files, rejected))));
            div().size(800.0, 600.0).child(div().key("zone").size(400.0, 400.0))
        }
    };
    f.frame(&view);
    f.frame(&view);
    f.app.host.drag_entered(&paths);
    handle_native_drag(&mut f.app, NativeDragEvent::Enter { paths: strings.clone(), position: DragPoint { x: 10.0, y: 10.0 } });
    let probe = state.clone();
    f.until("the probe", &view, move || probe.borrow().items.is_some());
    {
        let s = state.borrow();
        assert!(s.dragging && s.over);
        let items = s.items.as_ref().unwrap();
        let summary = summarize_drag(items, false);
        assert_eq!(summary.accepted, 1, "{items:?}");
        assert_eq!(summary.rejected.iter().map(|(_, n)| n).sum::<usize>(), 3);
    }
    f.app.host.drag_dropped(&paths);
    handle_native_drag(&mut f.app, NativeDragEvent::Drop { paths: strings, position: DragPoint { x: 10.0, y: 10.0 } });
    let probe = dropped.clone();
    f.until("the drop", &view, move || probe.borrow().is_some());
    let (files, rejected) = dropped.borrow_mut().take().unwrap();
    assert_eq!(files.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["notes.md"]);
    assert_eq!(files[0].bytes, b"# hi");
    let reasons: Vec<RejectionReason> = rejected.iter().map(|r| r.reason).collect();
    assert_eq!(reasons, [RejectionReason::Unsupported, RejectionReason::ImageInputUnavailable, RejectionReason::Directory]);
}

#[test]
fn ignores_a_drop_that_misses_every_zone_and_a_drag_that_carries_no_files() {
    let mut f = Frames::new();
    let dropped = Rc::new(Cell::new(false));
    let state = Rc::new(RefCell::new(DropZoneState::default()));
    let view = {
        let (dropped, state) = (dropped.clone(), state.clone());
        move |app: &mut App| {
            let dropped = dropped.clone();
            *state.borrow_mut() = drop_zone(app, "zone2", true, false, Rc::new(move |_: &mut App, _, _| dropped.set(true)));
            div().size(800.0, 600.0).child(div().key("zone2").size(100.0, 100.0))
        }
    };
    f.frame(&view);
    f.frame(&view);
    handle_native_drag(&mut f.app, NativeDragEvent::Enter { paths: Vec::new(), position: DragPoint { x: 1.0, y: 1.0 } });
    f.frame(&view);
    assert!(!state.borrow().dragging);
    handle_native_drag(&mut f.app, NativeDragEvent::Enter { paths: vec!["/w/a.md".to_owned()], position: DragPoint { x: 500.0, y: 500.0 } });
    f.frame(&view);
    assert!(state.borrow().dragging && !state.borrow().over, "somewhere over the window, not this zone");
    handle_native_drag(&mut f.app, NativeDragEvent::Drop { paths: vec!["/w/a.md".to_owned()], position: DragPoint { x: 500.0, y: 500.0 } });
    f.frame(&view);
    std::thread::sleep(Duration::from_millis(50));
    f.frame(&view);
    assert!(!dropped.get());
    assert!(!state.borrow().dragging);
}

#[test]
fn offers_a_path_to_the_app_first_and_gives_up_on_a_relative_one_nobody_can_resolve() {
    use mewrk_pane_files::links::PathOpenRequest;
    use mewrk_pane_files::path_open::*;
    let mut f = Frames::new();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = seen.clone();
    set_path_open_handler(Rc::new(move |_, request: &PathOpenRequest| {
        log.borrow_mut().push((request.path.clone(), request.base_dir.clone(), request.line));
        true
    }));
    let request = PathOpenRequest { path: "src/App.tsx".to_owned(), base_dir: Some("/w".to_owned()), line: Some(12), ..Default::default() };
    assert!(open_path(&mut f.app, &request));
    assert_eq!(*seen.borrow(), [("src/App.tsx".to_owned(), Some("/w".to_owned()), Some(12))]);
    // Once it declines, a relative path with no base is nothing the file manager could find either.
    set_path_open_handler(Rc::new(|_, _| false));
    assert!(!open_path(&mut f.app, &PathOpenRequest { path: "src/App.tsx".to_owned(), ..Default::default() }));
    clear_path_open_handler();
}

// ---------------------------------------------------------------- accessibility

#[test]
fn the_sketch_overlay_names_its_canvas_its_groups_and_its_controls() {
    use accesskit::Role;
    let mut frames = Frames::new();
    let attached = Rc::new(RefCell::new(None));
    let cancelled = Rc::new(Cell::new(0));
    let view = overlay_view("a11y-sketch", attached, cancelled);
    set_window_size(800.0, 600.0);
    set_a11y_active(true);
    for _ in 0..4 {
        frames.frame(&view);
    }
    let tree = take_a11y_update().expect("a client gets the tree").nodes;
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    a11y_is(&tree, Role::Application, "Sketch canvas");
    a11y_is(&tree, Role::Group, "Drawing tool");
    a11y_is(&tree, Role::Group, "Ink color");
    a11y_is(&tree, Role::Button, "Add to chat");
    set_a11y_active(false);
}

#[test]
fn an_attachments_window_is_named_for_it_and_its_controls_all_have_names() {
    use accesskit::Role;
    let mut frames = Frames::new();
    let file = upload(&mut frames, "notes.txt", b"plain\n", "text");
    let view = move |app: &mut App| div().size(800.0, 600.0).child(file_attachment_preview(app, &file, false, Rc::new(|_| {})));
    set_a11y_active(true);
    for _ in 0..4 {
        frames.frame(&view);
    }
    let tree = take_a11y_update().expect("a client gets the tree").nodes;
    assert!(a11y_is(&tree, Role::Dialog, "notes.txt").is_modal());
    a11y_is(&tree, Role::Button, "Close");
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    set_a11y_active(false);
}
