//! Renders the pane through the real GPU path to PNGs, for looking at. `cargo run -p mewrk-pane-files --example files_shot -- <out dir>`

#[path = "../tests/rig/mod.rs"]
#[allow(dead_code)]
mod rig;

use fluent::el::*;
use fluent::gpu::SURFACE_FORMAT;
use fluent::ui::{Input, NoClipboard, UiMemory};
use fluent::{Gpu, Renderer, Scene, TextCtx, Theme, Ui};
use mewrk_client::App;
use mewrk_pane_files::pane::*;
use mewrk_pane_files::*;
use rig::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

const W: u32 = 1400;
const H: u32 = 900;
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
    props: FilesPaneProps,
    theme: Theme,
}

impl Shot {
    fn frame(&mut self) {
        self.app.tick();
        self.scene.clear();
        let size = (W as f32 / SCALE, H as f32 / SCALE);
        set_window_size(size.0, size.1);
        set_theme(self.theme);
        let view = files_pane(&mut self.app, self.props.clone());
        let root = col::<App>().size(size.0, size.1).bg(self.theme.layer).child(row::<App>().h(32.0).shrink0().items_center().child(view.header)).child(col::<App>().flex1().min_w(0.0).min_h(0.0).clip().child(view.body));
        let mut clipboard = NoClipboard;
        let mut ui = Ui::new(&mut self.scene, &mut self.text, self.theme, &self.input, &mut self.mem, &mut clipboard, SCALE, size, 0.016, 0.0);
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

    fn open(&mut self, path: &str) {
        let page = pane(&mut self.app, "files").and_then(|st| st.active_page.clone()).unwrap();
        show_file(&mut self.app, "files", &page, Location::new(&None, path.to_owned()), true);
    }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let dark = std::env::args().nth(2).as_deref() != Some("light");
    let data = TempDir::new("shot-data");
    let files = TempDir::new("shot-root");
    let root = files.0.clone();
    let write = |name: &str, content: &[u8]| {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };
    write("README.md", b"---\ntitle: Sample\n---\n# Mewrk Files\n\nA **document** with `src/main.rs:12`, [a link](docs/guide.md) and /etc/hosts in prose.\n\n<p align=\"center\"><img src=\"assets/shot.png\" width=\"120\" alt=\"logo\"></p>\n\n## List\n\n- one\n- [x] done\n- [ ] not yet\n\n| Name | Count |\n|------|------:|\n| a | 1 |\n| b | 22 |\n\n> A quote.\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n");
    write("src/main.rs", b"// A long enough comment to see colours and wrapping working together in the numbered view of a file, on one line.\nuse std::io;\n\nfn main() -> io::Result<()> {\n    let name = \"mewrk\";\n    println!(\"{name}\");\n    Ok(())\n}\n");
    write("src/lib/deep.ts", b"export const x = 1;\n");
    write("Cargo.toml", b"[package]\nname = \"x\"\n");
    write("package.json", b"{}\n");
    write("data/table.csv", b"name,count,note\nalpha,1,first\nbeta,22,second\ngamma,333,third\n");
    write("n.ipynb", br##"{"nbformat":4,"metadata":{"language_info":{"name":"python"}},"cells":[{"cell_type":"markdown","source":["# Notebook\n","Some *text*."]},{"cell_type":"code","execution_count":1,"source":["print('hi')\n","1 + 1"],"outputs":[{"output_type":"stream","name":"stdout","text":"hi\n"},{"output_type":"execute_result","execution_count":1,"data":{"text/plain":["2"]}},{"output_type":"display_data","data":{"text/html":["<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>"]}}]}]}"##);
    let mut png = Vec::new();
    let mut picture = image::RgbaImage::new(160, 90);
    for (x, y, p) in picture.enumerate_pixels_mut() {
        *p = image::Rgba([(x * 255 / 160) as u8, (y * 255 / 90) as u8, 180, if (x / 20 + y / 20) % 2 == 0 { 255 } else { 120 }]);
    }
    picture.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    write("assets/shot.png", &png);
    let wav = audio::encode_wav(&audio::DecodedAudio { samples: (0..24000).map(|i| ((i as f32 * 0.03).sin() * (i as f32 / 24000.0)) * 0.8).collect(), channels: 1, rate: 8000 });
    write("voice.wav", &wav);
    if let Ok(font) = std::fs::read("/System/Library/Fonts/Supplemental/Arial.ttf") {
        write("Arial.ttf", &font);
    }
    write("Fraunces.woff2", include_bytes!("../tests/fixtures/fraunces-latin-600-normal.woff2"));
    let wide: String = (0..40).map(|row| (0..24).map(|column| if row == 0 { format!("column {column}") } else { format!("{}", row * column) }).collect::<Vec<_>>().join(",") + "\n").collect();
    write("data/wide.csv", wide.as_bytes());

    let app = start_app(&data.0);
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let gpu = Arc::new(Gpu::new(instance, None).expect("a GPU"));
    let renderer = Renderer::new(gpu.clone());
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor { label: Some("shot"), size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: SURFACE_FORMAT, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
    let props = FilesPaneProps { pane_id: "files".to_owned(), workspaces: vec![FilesPaneWorkspace { number: 1, machine: None, path: root.to_string_lossy().into_owned() }], ssh_machines: Vec::new(), host_windows: false, active: true, open_request: None, on_open_request_handled: None, on_mention: None };
    let theme = if dark { Theme::dark() } else { Theme::light() };
    let mut shot = Shot { gpu, renderer, target, scene: Scene::default(), text: TextCtx::new(SCALE), mem: UiMemory::default(), input: Input::default(), app, props, theme };
    shot.settle(300);
    let path = |n: &str| root.join(n).to_string_lossy().into_owned();
    shot.save(&format!("{out}/1-tree.png"));
    shot.open(&path("src/main.rs"));
    let page = pane(&mut shot.app, "files").unwrap().active_page.clone().unwrap();
    set_directory_open(&mut shot.app, "files", &page, &None, &path("src"), true);
    shot.settle(300);
    shot.save(&format!("{out}/2-code.png"));
    for (name, file) in [("3-markdown", "README.md"), ("4-csv", "data/table.csv"), ("4-wide-csv", "data/wide.csv"), ("8-woff2", "Fraunces.woff2"), ("5-notebook", "n.ipynb"), ("6-image", "assets/shot.png"), ("7-audio", "voice.wav"), ("8-font", "Arial.ttf")] {
        shot.open(&path(file));
        shot.settle(500);
        shot.save(&format!("{out}/{name}.png"));
    }
    // A menu, as a right click opens it.
    let sections = pane(&mut shot.app, "files").map(|st| {
        let location = st.active().and_then(|p| p.file.clone()).unwrap();
        entry_menu(st, &location, browser::BrowseEntryKind::File)
    }).unwrap();
    open_menu(&mut shot.app, "files", mewrk_ui_common::context_menu::ContextMenuAnchor::Point { x: 120.0, y: 220.0 }, sections);
    shot.settle(100);
    shot.save(&format!("{out}/9-menu.png"));
    shot.app.host.finalize();
}
