//! Mewrk, in pure Rust: the window driver. What it draws is `mewrk_shell::root`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fluent::gpu::SURFACE_FORMAT;
use fluent::ui::{Clipboard, CursorKind, Input, Key, KeyPress, NoClipboard, Ui, UiMemory};
use fluent::{Gpu, Renderer, Scene, TextCtx};
use mewrk_client::{App, StartError, StartOptions};
use mewrk_shell::root;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

mod fatal;

const USAGE: &str = "\
mewrk [--theme system|light|dark] [--exit-after SECONDS] [--scene NAME] [--lang zh|en]
headless: --headless [--size WxH] [--scale F] [--frames N] [--shot PNG] [--bench SECONDS] [--json PATH] [--scroll PX] [--scene NAME] [--right-click XxY]
diagnostics: MEWRK_TRACE=1 prints each frame's cost, MEWRK_ANIMATE=1 draws without end, --fullscreen-test toggles full screen every 4 s
accessibility: --a11y-probe JSON [--a11y-steps 'press:NAME;focus:NAME;set:NAME=VALUE;click:NAME;at:X,Y;type:WORDS'] asks the window what an assistive client would and quits; headless, --a11y builds the tree every frame
Data lives in MEWRK_DATA_DIR, or ~/Library/Application Support/dev.mewrk.native.";

const IDENTIFIER: &str = "dev.mewrk.native";

/// Deliberately not upstream's folder: the two apps must be able to run side by side. The format is the same, so
/// copying upstream's folder here is all it takes to bring its data along.
fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MEWRK_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join("Library/Application Support").join(IDENTIFIER)
}

/// Why the app did not start, for what the driver does next.
enum StartFailure {
    /// The document, or the data folder holding it, could not be read: there is a window to show that in.
    Load(String),
}

fn start_options(wake: Arc<dyn Fn() + Send + Sync>, exit: Arc<dyn Fn(i32) + Send + Sync>, show_window: Arc<dyn Fn() + Send + Sync>) -> StartOptions {
    StartOptions { identifier: IDENTIFIER.to_owned(), version: env!("CARGO_PKG_VERSION").to_owned(), data_dir: data_dir(), resource_dir: None, wake, exit, show_window }
}

fn start_app(wake: Arc<dyn Fn() + Send + Sync>, exit: Arc<dyn Fn(i32) + Send + Sync>, show_window: Arc<dyn Fn() + Send + Sync>) -> Result<Option<App>, StartFailure> {
    match App::start(start_options(wake, exit, show_window)) {
        Ok(app) => Ok(Some(app)),
        // Another Mewrk holds this data folder and has just been asked to show its window.
        Err(StartError::AnotherInstanceAnswered) => Ok(None),
        Err(StartError::Unavailable(message)) => Err(StartFailure::Load(message)),
    }
}

/// The host for `--reset-document`: the same folder, with no window to answer to.
fn reset_config() -> mewrk_client::HostConfig {
    let data = data_dir();
    mewrk_client::HostConfig {
        identifier: IDENTIFIER.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        local_data_dir: data.clone(),
        data_dir: data,
        resource_dir: None,
        run_on_main: None,
        exit: None,
        show_window: None,
    }
}

/// `ErrorView`'s text, in the language the system prefers: the document that would say otherwise is what could not be read.
fn fatal_app(args: &Args, message: String) -> mewrk_shell::dialogs::error_view::FatalApp {
    mewrk_client::i18n::set_english(!mewrk_client::i18n::system_prefers_chinese());
    apply_language(args);
    let can_reset = fatal::reset_can_help(&message);
    mewrk_shell::dialogs::error_view::FatalApp::new(message, args.value(fatal::RESET_FAILED_FLAG).map(str::to_owned), can_reset)
}

struct Args(Vec<String>);

impl Args {
    fn value(&self, name: &str) -> Option<&str> {
        self.0.iter().position(|a| a == name).and_then(|i| self.0.get(i + 1)).map(String::as_str)
    }
    fn flag(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }
    fn num<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.value(name) {
            Some(v) => v.parse().map_err(|_| format!("{name}: bad value {v}")),
            None => Ok(default),
        }
    }
}

/// The part of the app that exists whether or not there is a window.
struct Core {
    app: App,
    text: TextCtx,
    scene: Scene,
    mem: UiMemory,
    input: Input,
    renderer: Renderer,
    started: Instant,
    last_frame: Instant,
    /// The window's scale times the app's zoom, so layout, hit testing and scrollbars scale together.
    scale: f32,
    window_scale: f32,
    size_px: (u32, u32),
    timings: Vec<(u32, u32, u32, u32)>,
    keep_timings: bool,
    forced_theme: Option<bool>,
    /// The window's size in the last frame: a scene built for it can be drawn again only at it.
    last_size: Option<(u32, u32)>,
}

struct FrameOutcome {
    animating: bool,
    wake_in: Option<f32>,
    cursor: CursorKind,
    ime_area: Option<fluent::ui::Rect>,
    submission: wgpu::SubmissionIndex,
    /// When a background that moves next wants looking at.
    motion_next: Option<Instant>,
}

impl Core {
    fn frame(&mut self, target: &wgpu::TextureView, clipboard: &mut dyn Clipboard) -> FrameOutcome {
        let t0 = Instant::now();
        let dt = t0.duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = t0;
        let time = t0.duration_since(self.started).as_secs_f32();
        let theme = root::theme(&self.app, self.forced_theme);
        mewrk_settings_global::on_scheme_changed(&mut self.app, theme.dark);
        let scale = self.window_scale * mewrk_settings_global::zoom(&self.app);
        if scale != self.scale {
            // What the pointer was measured in just changed under it.
            if let Some((x, y)) = self.input.pointer {
                self.input.pointer = Some((x * self.scale / scale, y * self.scale / scale));
            }
            self.scale = scale;
        }
        let size = (self.size_px.0 as f32 / self.scale, self.size_px.1 as f32 / self.scale);
        self.text.set_scale(self.scale);
        let appearance = &self.app.doc.global_settings.appearance;
        self.text.set_font_families(&appearance.ui_font_family, &appearance.mono_font_family);
        self.text.set_serif_family(if appearance.serif_messages { mewrk_client::appearance::SERIF_FONT_STACK } else { "" });

        let epoch = self.text.epoch;
        self.scene.clear();
        let mut ui = Ui::new(&mut self.scene, &mut self.text, theme, &self.input, &mut self.mem, clipboard, self.scale, size, dt, time);
        fluent::el::set_theme(theme);
        fluent::el::set_window_size(size.0, size.1);
        self.app.tick();
        self.app.frame_no += 1;
        let view = root::root(&mut self.app);
        let again = fluent::el::render(view, &mut self.app, &mut ui);
        // Handlers just ran, so what they changed is not on screen yet.
        let (animating, wake_in, cursor, ime_area) = (ui.animating || again, ui.wake_in, ui.cursor, ui.ime_area);
        drop(ui);
        // An atlas reset partway through means glyphs built earlier in this frame are stale. The next one is clean.
        let animating = animating || self.text.epoch != epoch;
        self.input.end_frame();
        let t1 = Instant::now();

        self.text.flush_atlases(&self.renderer);
        self.scene.cull_occluded(self.size_px);
        let submission = self.renderer.render(&self.scene, &theme, target, self.size_px);
        self.last_size = Some(self.size_px);
        // Under something solid that covers all of it there is nothing to play for, until that goes.
        let shows_motion = mewrk_shell::glass::shows_motion(&mut self.app, &self.scene);
        let uncovered = mewrk_settings_global::set_backdrop_covered(&mut self.app, !shows_motion);
        let motion_next = if uncovered { Some(Instant::now()) } else { mewrk_shell::glass::frame_tick().next };
        let t2 = Instant::now();
        if self.keep_timings {
            let (_, glyphs) = self.scene.counts();
            self.timings.push((t1.duration_since(t0).as_micros() as u32, t2.duration_since(t1).as_micros() as u32, glyphs as u32, 0));
        }
        FrameOutcome { animating, wake_in, cursor, ime_area, submission, motion_next }
    }

    /// Whether a background that moves has something new, and when to look again. It goes to the GPU from here, so all that is left is
    /// [`Self::draw_again`].
    fn tick_motion(&mut self, now: Instant) -> mewrk_motion::Tick {
        mewrk_shell::glass::tick_motion(&mut self.app, now)
    }

    /// The last frame's scene again, over whatever has changed on the GPU since: no view, no layout, no text.
    fn draw_again(&mut self, target: &wgpu::TextureView) -> Option<wgpu::SubmissionIndex> {
        // None before the first frame, and no scene to draw again.
        self.last_size.filter(|size| *size == self.size_px)?;
        let t0 = Instant::now();
        let submission = self.renderer.render_again(&self.scene, target);
        if self.keep_timings {
            let (_, glyphs) = self.scene.counts();
            self.timings.push((0, t0.elapsed().as_micros() as u32, glyphs as u32, 0));
        }
        Some(submission)
    }
}

// ---------------------------------------------------------------- headless

fn percentile(sorted: &[u32], q: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

fn run_headless(args: &Args) -> Result<(), String> {
    let size: (u32, u32) = {
        let s = args.value("--size").unwrap_or("2800x1800");
        let (w, h) = s.split_once('x').ok_or("--size WxH")?;
        (w.parse().map_err(|_| "--size")?, h.parse().map_err(|_| "--size")?)
    };
    let scale: f32 = args.num("--scale", 2.0)?;
    let frames: u32 = args.num("--frames", 60)?;
    let bench: f32 = args.num("--bench", 0.0)?;
    // `--motion-only`: past the first frames, draw only as a background that moves asks to, as a window does at rest.
    let motion_only = args.flag("--motion-only");

    let started = start_app(Arc::new(|| {}), Arc::new(|code| std::process::exit(code)), Arc::new(|| {}));
    let mut app = match started {
        Ok(Some(app)) => app,
        Ok(None) => return Ok(()),
        Err(StartFailure::Load(message)) => {
            if let Some(path) = args.value("--shot") {
                return shoot_error_view(args, message, size, scale, frames, path);
            }
            return Err(message);
        }
    };
    app.system_dark = args.value("--theme") != Some("light");
    apply_language(args);
    root::init(&mut app);
    if let Some(scene) = args.value("--scene")
        && !mewrk_shell::scenes::apply(&mut app, scene)
    {
        return Err(format!("--scene: no scene {scene}; there are {:?}", mewrk_shell::scenes::SCENES));
    }

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let gpu = Arc::new(Gpu::new(instance, None)?);
    let renderer = Renderer::new(gpu.clone());
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("headless"),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: SURFACE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let now = Instant::now();
    let mut core = Core { app, text: TextCtx::new(scale), scene: Scene::default(), mem: UiMemory::default(), input: Input::default(), renderer, started: now, last_frame: now, scale, window_scale: scale, size_px: size, timings: Vec::new(), keep_timings: bench > 0.0, forced_theme: forced_theme(args), last_size: None };
    let mut clipboard = NoClipboard;
    let vsync = Duration::from_secs_f64(1.0 / 120.0);
    let begun = Instant::now();
    let mut n = 0;
    let scroll: f32 = args.num("--scroll", 0.0)?;
    if args.flag("--a11y") {
        fluent::el::set_a11y_active(true);
    }
    let right_click = args.value("--right-click").and_then(|at| at.split_once('x')).and_then(|(x, y)| Some((x.parse::<f32>().ok()?, y.parse::<f32>().ok()?)));
    loop {
        // Late enough that what it lands on has settled, and with frames left for the menu it opens.
        if let Some(at) = right_click.filter(|_| n + 30 == frames) {
            core.input.pointer = Some(at);
            core.input.right_pressed = true;
        }
        // A steady scroll, reversing every few seconds, to measure the frames scrolling a long conversation costs.
        if scroll != 0.0 {
            core.input.pointer = Some((size.0 as f32 / scale * 0.4, size.1 as f32 / scale * 0.5));
            core.input.scroll = if n / 600 % 2 == 0 { scroll } else { -scroll };
            core.input.scroll_is_pixels = true;
        }
        if bench > 0.0 {
            if begun.elapsed().as_secs_f32() >= bench {
                break;
            }
        } else if n >= frames {
            break;
        }
        let start = Instant::now();
        // The first seconds go to importing and everything arriving.
        if motion_only && begun.elapsed().as_secs_f32() > 3.0 {
            let tick = core.tick_motion(start);
            if tick.rebuild {
                let outcome = core.frame(&view, &mut clipboard);
                let _ = gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(outcome.submission), timeout: Some(Duration::from_secs(5)) });
                core.timings.clear();
                continue;
            }
            if !tick.changed {
                std::thread::sleep(tick.next.map_or(Duration::from_millis(1), |next| next.saturating_duration_since(start)).min(Duration::from_millis(4)));
                continue;
            }
            if let Some(submission) = core.draw_again(&view) {
                let submitted = Instant::now();
                let _ = gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(Duration::from_secs(5)) });
                if let Some(last) = core.timings.last_mut() {
                    last.3 = submitted.elapsed().as_micros() as u32;
                }
            }
            n += 1;
            continue;
        }
        let outcome = core.frame(&view, &mut clipboard);
        if motion_only {
            core.timings.clear();
        }
        let _ = fluent::el::take_a11y_update();
        let submitted = Instant::now();
        let _ = gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(outcome.submission), timeout: Some(Duration::from_secs(5)) });
        if let Some(last) = core.timings.last_mut() {
            last.3 = submitted.elapsed().as_micros() as u32;
        }
        n += 1;
        let spent = start.elapsed();
        if spent < vsync {
            std::thread::sleep(vsync - spent);
        }
    }
    if bench > 0.0 {
        let col = |f: fn(&(u32, u32, u32, u32)) -> u32| {
            let mut v: Vec<u32> = core.timings.iter().map(f).collect();
            v.sort_unstable();
            v
        };
        let (build, render, glyphs, gpu_wait) = (col(|t| t.0), col(|t| t.1), col(|t| t.2), col(|t| t.3));
        let stat = |v: &[u32]| serde_json::json!({"p50_ms": percentile(v, 0.5) as f64 / 1000.0, "p99_ms": percentile(v, 0.99) as f64 / 1000.0, "max_ms": percentile(v, 1.0) as f64 / 1000.0});
        let report = serde_json::json!({"frames": core.timings.len(), "cpu_build": stat(&build), "cpu_render": stat(&render), "gpu_wait": stat(&gpu_wait), "glyphs_p50": percentile(&glyphs, 0.5), "rss_mib": rss_mib()});
        println!("{}", serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?);
        if let Some(path) = args.value("--json") {
            std::fs::write(path, report.to_string()).map_err(|e| format!("{path}: {e}"))?;
        }
    }
    if let Some(path) = args.value("--shot") {
        save_png(&gpu, &target, size, path)?;
        println!("wrote {path}");
    }
    core.app.host.finalize();
    Ok(())
}

/// `ErrorView`, as a picture: `--headless --shot` on a data folder whose document does not load.
fn shoot_error_view(args: &Args, message: String, size: (u32, u32), scale: f32, frames: u32, path: &str) -> Result<(), String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let gpu = Arc::new(Gpu::new(instance, None)?);
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("headless"),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: fatal::FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let mut core = fatal::FatalCore::new(fatal_app(args, message), gpu.clone(), scale, size, args.value("--theme") != Some("light"));
    for _ in 0..frames.max(2) {
        let (_, submission) = core.frame(&view);
        let _ = gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(Duration::from_secs(5)) });
    }
    save_png(&gpu, &target, size, path)?;
    println!("wrote {path}");
    Ok(())
}

/// `--lang`, for pictures and for testing: a choice in the document still wins on the next document change.
fn apply_language(args: &Args) {
    match args.value("--lang") {
        Some("en") => mewrk_client::i18n::set_english(true),
        Some("zh") => mewrk_client::i18n::set_english(false),
        _ => {}
    }
}

fn forced_theme(args: &Args) -> Option<bool> {
    match args.value("--theme") {
        Some("light") => Some(false),
        Some("dark") => Some(true),
        _ => None,
    }
}

fn rss_mib() -> f64 {
    // SAFETY: rusage is plain old data and getrusage fully initialises it on success.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return 0.0;
        }
        usage.ru_maxrss as f64 / 1048576.0
    }
}

fn save_png(gpu: &Gpu, texture: &wgpu::Texture, size: (u32, u32), path: &str) -> Result<(), String> {
    let row = size.0 * 4;
    let aligned = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor { label: Some("readback"), size: (aligned * size.1) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(aligned), rows_per_image: None } },
        wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| e.to_string())?;
    let data = slice.get_mapped_range().map_err(|e| format!("{e:?}"))?;
    let mut rgba = Vec::with_capacity((row * size.1) as usize);
    for y in 0..size.1 as usize {
        let line = &data[y * aligned as usize..y * aligned as usize + row as usize];
        for px in line.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    image::save_buffer(path, &rgba, size.0, size.1, image::ColorType::Rgba8).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- windowed

#[derive(Debug)]
enum UserEvent {
    Wake,
    Show,
    Exit(i32),
    #[cfg(target_os = "macos")]
    Native(mewrk_shell::native::NativeEvent),
    A11y(accesskit_winit::Event),
}

impl From<accesskit_winit::Event> for UserEvent {
    fn from(event: accesskit_winit::Event) -> Self {
        UserEvent::A11y(event)
    }
}

struct SystemClipboard(Option<arboard::Clipboard>);

impl Clipboard for SystemClipboard {
    fn get(&mut self) -> Option<String> {
        self.0.as_mut()?.get_text().ok()
    }
    fn set(&mut self, text: &str) {
        if let Some(c) = self.0.as_mut() {
            let _ = c.set_text(text);
        }
    }
}

#[allow(dead_code)]
struct WindowState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    core: Core,
    present_blocked: bool,
    /// `config` changed and the surface has not been told. Configuring waits for the GPU to go idle, so it happens once, as
    /// the next frame starts, however many sizes the OS sent before then.
    surface_stale: bool,
    fullscreen: bool,
    a11y: accesskit_winit::Adapter,
}

impl WindowState {
    /// Full screen as winit reports it, which changes when a transition ends and sends no event of its own.
    fn sync_fullscreen(&mut self) {
        let fullscreen = self.window.fullscreen().is_some();
        if fullscreen == self.fullscreen {
            return;
        }
        self.fullscreen = fullscreen;
        self.config.desired_maximum_frame_latency = frame_latency(fullscreen);
        self.surface_stale = true;
        if !fullscreen {
            // AppKit put the lights back while it laid the title bar out again.
            place_traffic_lights(&self.window);
        }
        self.window.request_redraw();
    }
}

/// How many drawables the layer keeps beyond the one on screen. One gives a window its lowest latency, since every extra one is
/// another vsync between a keypress and the screen. Full screen needs two: the display scans the layer out directly and holds
/// the outgoing drawable through the next refresh, so with one a frame could only start every other vsync, and a 5K window
/// ran at 30 fps however cheap its frames were.
fn frame_latency(fullscreen: bool) -> u32 {
    if fullscreen { 2 } else { 1 }
}

/// The window when the document did not load: everything `WindowState` has, except an app.
struct FatalWindow {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    core: fatal::FatalCore,
}

impl FatalWindow {
    fn redraw(&mut self) {
        let gpu = self.core.gpu();
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&gpu.device, &self.config);
                self.window.request_redraw();
                return;
            }
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());
        let (animating, _) = self.core.frame(&view);
        self.window.pre_present_notify();
        gpu.queue.present(frame);
        if self.core.app.reset_requested {
            fatal::begin_reset();
        }
        if std::mem::take(&mut self.core.app.drag_requested) {
            let _ = self.window.drag_window();
            // AppKit tracks the drag to its end and keeps the release to itself, so the press has to be ended here.
            self.core.input.down = false;
            self.core.input.released = true;
            self.window.request_redraw();
        }
        if std::mem::take(&mut self.core.app.zoom_requested) {
            self.window.set_maximized(!self.window.is_maximized());
        }
        if animating {
            self.window.request_redraw();
        }
    }

    fn event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        let scale = self.core.scale;
        match event {
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            WindowEvent::CloseRequested => {
                event_loop.exit();
                return;
            }
            WindowEvent::Resized(size) => {
                self.config.width = size.width.max(1);
                self.config.height = size.height.max(1);
                self.surface.configure(&self.core.gpu().device, &self.config);
                self.core.size_px = (self.config.width, self.config.height);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => self.core.scale = scale_factor as f32,
            WindowEvent::ThemeChanged(theme) => self.core.dark = theme == winit::window::Theme::Dark,
            WindowEvent::CursorMoved { position, .. } => self.core.input.pointer = Some((position.x as f32 / scale, position.y as f32 / scale)),
            WindowEvent::CursorLeft { .. } => self.core.input.pointer = None,
            WindowEvent::MouseInput { state: button_state, button: MouseButton::Left, .. } => match button_state {
                ElementState::Pressed => {
                    self.core.input.down = true;
                    self.core.input.pressed = true;
                }
                ElementState::Released => {
                    self.core.input.down = false;
                    self.core.input.released = true;
                }
            },
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let key = match event.logical_key {
                    WKey::Named(NamedKey::Escape) => Key::Escape,
                    WKey::Named(NamedKey::Enter) => Key::Enter,
                    WKey::Named(NamedKey::Tab) => Key::Tab,
                    WKey::Named(NamedKey::Space) => Key::Char(' '),
                    _ => return,
                };
                self.core.input.keys.push(KeyPress { key, ctrl: false, shift: false, alt: false, cmd: false });
            }
            _ => return,
        }
        self.window.request_redraw();
    }
}

struct Shell {
    /// Set instead of `state` when the document did not load.
    fatal: Option<FatalWindow>,
    args: Args,
    proxy: EventLoopProxy<UserEvent>,
    state: Option<WindowState>,
    modifiers: ModifiersState,
    clipboard: SystemClipboard,
    next_wake: Option<Instant>,
    /// When a background that moves next wants looking at. Not a reason to build a frame.
    next_motion: Option<Instant>,
    last_click: Option<Instant>,
    /// The press under way is a Control-click, so its release is no click.
    control_click: bool,
    exit_after: Option<Instant>,
    error: Option<String>,
    launched: Instant,
    first_frame_logged: bool,
    /// `MEWRK_TRACE`: every frame and resize goes to stderr with what it cost.
    trace: bool,
    fullscreen_test: Option<Instant>,
    /// `MEWRK_ANIMATE`: frames without end, to see what the display gives a window that always has something to draw.
    animate_test: bool,
    exit_code: i32,
    #[cfg(target_os = "macos")]
    native: Option<mewrk_shell::native::NativeShell>,
    /// What the window's last set theme was, to change it only when it changes.
    window_theme: Option<Option<winit::window::Theme>>,
    /// Paths the OS says are being dragged, gathered until the event loop has told them all.
    drag: FileDrag,
    a11y_probe: Option<A11yProbe>,
    /// The window was closed in full screen and is on its way out of it.
    hide_once_windowed: bool,
}

/// `--a11y-probe`: asks the window what an assistive client would, through AppKit, and writes down what it answers.
struct A11yProbe {
    path: PathBuf,
    /// `press:NAME`, `focus:NAME` or `set:NAME=VALUE`, done as a client does them; `click:NAME`, `at:X,Y` and `type:WORDS`, as a person does.
    steps: std::collections::VecDeque<String>,
    next: Instant,
    /// The first question only wakes the adapter, so the first answer is its placeholder.
    asked: bool,
    log: Vec<serde_json::Value>,
}

/// The OS tells a drag one file at a time, and has no event for where it is over the window; a drop zone wants the whole
/// list at once and the point.
#[derive(Default)]
struct FileDrag {
    hovering: Vec<PathBuf>,
    entered: bool,
    dropped: Vec<PathBuf>,
}

/// How often a drag's position is read while files are held over the window.
const DRAG_POLL: Duration = Duration::from_millis(50);

/// How often a window that cannot present tries again.
const BLOCKED_RETRY: Duration = Duration::from_millis(100);

impl Shell {
    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<Option<WindowState>, String> {
        use winit::platform::macos::WindowAttributesExtMacOS;
        let attributes = Window::default_attributes()
            .with_title("Mewrk")
            .with_inner_size(LogicalSize::new(1440.0, 900.0))
            .with_min_inner_size(LogicalSize::new(680.0, 520.0))
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true)
            // AccessKit has to take over the view before anything can be observing it, which a window on show already is.
            .with_visible(false);
        let window = Arc::new(event_loop.create_window(attributes).map_err(|e| e.to_string())?);
        window.set_ime_allowed(true);
        place_traffic_lights(&window);
        #[cfg(target_os = "macos")]
        self.install_native();
        #[cfg(target_os = "macos")]
        if let Some(native) = &self.native {
            native.watch_window(content_view(&window));
        }
        #[cfg(target_os = "macos")]
        if let Err(error) = mewrk_shell::native::drop_bubbled_keys(content_view(&window)) {
            eprintln!("could not guard the window's keys: {error}");
        }
        // Takes the view over by giving it a subclass, which is why it comes after what needs winit's own class above.
        let a11y = accesskit_winit::Adapter::with_event_loop_proxy(event_loop, &window, self.proxy.clone());
        #[cfg(target_os = "macos")]
        if let Err(error) = mewrk_shell::native::keep_child_views_accessible(content_view(&window)) {
            eprintln!("accessibility: {error}");
        }
        window.set_visible(true);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(Box::new(event_loop.owned_display_handle())));
        let surface = instance.create_surface(window.clone()).map_err(|e| e.to_string())?;
        let gpu = Arc::new(Gpu::new(instance, Some(&surface))?);
        let renderer = Renderer::new(gpu.clone());
        let size = window.inner_size();
        let caps = surface.get_capabilities(&gpu.adapter);
        if !caps.formats.contains(&SURFACE_FORMAT) {
            return Err(format!("surface does not support {SURFACE_FORMAT:?}: {:?}", caps.formats));
        }
        let mut config = surface.get_default_config(&gpu.adapter, size.width.max(1), size.height.max(1)).ok_or("surface is not compatible with the adapter")?;
        config.format = SURFACE_FORMAT;
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.desired_maximum_frame_latency = frame_latency(false);
        config.view_formats = vec![];
        surface.configure(&gpu.device, &config);

        let proxy = self.proxy.clone();
        let wake_fn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = proxy.send_event(UserEvent::Wake);
        });
        let scale = window.scale_factor() as f32;
        let exit_proxy = self.proxy.clone();
        let exit: Arc<dyn Fn(i32) + Send + Sync> = Arc::new(move |code| {
            let _ = exit_proxy.send_event(UserEvent::Exit(code));
        });
        let show_proxy = self.proxy.clone();
        let show: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = show_proxy.send_event(UserEvent::Show);
        });
        let mut app = match start_app(wake_fn, exit, show) {
            Ok(Some(app)) => app,
            Ok(None) => std::process::exit(0),
            Err(StartFailure::Load(message)) => {
                let dark = match forced_theme(&self.args) {
                    Some(dark) => dark,
                    None => window.theme().is_none_or(|t| t == winit::window::Theme::Dark),
                };
                let core = fatal::FatalCore::new(fatal_app(&self.args, message), gpu, scale, (config.width, config.height), dark);
                window.focus_window();
                window.request_redraw();
                self.fatal = Some(FatalWindow { window, surface, config, core });
                return Ok(None);
            }
        };
        app.system_dark = window.theme().is_none_or(|t| t == winit::window::Theme::Dark);
        app.native_view = content_view(&window);
        apply_language(&self.args);
        root::init(&mut app);
        if let Some(scene) = self.args.value("--scene")
            && !mewrk_shell::scenes::apply(&mut app, scene)
        {
            return Err(format!("--scene: no scene {scene}"));
        }
        // A scene is a picture of something else. Writing `permissions-prompted` into the data folder keeps the sheet out of any other run.
        if self.args.value("--scene").is_none() {
            mewrk_settings_global::first_launch(&mut app);
        }
        let forced = forced_theme(&self.args);

        let now = Instant::now();
        let core = Core { app, text: TextCtx::new(scale), scene: Scene::default(), mem: UiMemory::default(), input: Input::default(), renderer, started: now, last_frame: now, scale, window_scale: scale, size_px: (config.width, config.height), timings: Vec::new(), keep_timings: false, forced_theme: forced, last_size: None };
        window.focus_window();
        window.request_redraw();
        Ok(Some(WindowState { window, surface, config, core, present_blocked: false, surface_stale: false, fullscreen: false, a11y }))
    }

    #[cfg(target_os = "macos")]
    fn install_native(&mut self) {
        let proxy = self.proxy.clone();
        let chinese = mewrk_client::i18n::system_prefers_chinese();
        let template = tray_template_png();
        fluent::set_reduced_motion(mewrk_shell::native::reduce_motion());
        self.native = mewrk_shell::native::install(
            move |event| {
                let _ = proxy.send_event(UserEvent::Native(event));
            },
            chinese,
            template.as_deref(),
        );
    }

    /// `show_main_window`: reveals the window wherever it is: hidden in the menu bar, minimized, or behind other windows.
    fn show_main_window(&mut self) {
        let Some(state) = self.state.as_mut() else { return };
        self.hide_once_windowed = false;
        // Back in the Dock first, so the window returns to an app that can take the focus.
        #[cfg(target_os = "macos")]
        mewrk_shell::native::set_in_dock(true);
        state.window.set_visible(true);
        state.window.set_minimized(false);
        state.window.focus_window();
        #[cfg(target_os = "macos")]
        mewrk_shell::native::activate();
        state.window.request_redraw();
    }

    /// `hide_main_window`: with a menu bar item the window is only a view on the running process; quitting is that item's job.
    fn hide_main_window(&mut self) {
        let Some(state) = self.state.as_mut() else { return };
        // Hiding a full-screen window leaves the black backdrop AppKit keeps behind it, over the desktop, until the process ends. So it
        // leaves full screen first, and hides when `LeftFullScreen` says that is done.
        #[cfg(target_os = "macos")]
        if self.native.as_ref().is_some_and(|native| native.in_full_screen()) {
            self.hide_once_windowed = true;
            state.window.set_fullscreen(None);
            return;
        }
        state.window.set_visible(false);
        #[cfg(target_os = "macos")]
        mewrk_shell::native::set_in_dock(false);
    }

    fn tray_installed(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.native.as_ref().is_some_and(|native| native.tray_installed());
        #[cfg(not(target_os = "macos"))]
        false
    }

    /// Quit: what the app holds that the host's barrier does not, then the barrier: every save before leaving, then the exit
    /// hook. A failed save keeps the app open, with its window back.
    fn request_quit(&mut self) {
        let Some(state) = self.state.as_mut() else { return };
        state.core.app.save_conversation_turns();
        state.core.app.flush_ui_store();
        state.core.app.host.request_exit(0);
    }

    /// One step of `--a11y-probe` when its time has come. A step and the answer to it are a second apart, so the frame it caused has run.
    fn step_a11y_probe(&mut self) {
        use objc2::{msg_send, runtime::AnyObject};
        let Some(probe) = self.a11y_probe.as_mut() else { return };
        let Some(state) = self.state.as_ref() else { return };
        if Instant::now() < probe.next {
            return;
        }
        probe.next = Instant::now() + Duration::from_secs(1);
        let view = content_view(&state.window);
        let tree = mewrk_shell::native::describe_accessibility(view);
        if !std::mem::replace(&mut probe.asked, true) {
            probe.log.push(serde_json::json!({ "step": "first question", "answer": tree }));
            return;
        }
        probe.log.push(serde_json::json!({ "step": "answer", "answer": tree, "focused_id": fluent::el::focused_id() }));
        let Some(step) = probe.steps.pop_front() else {
            if let Err(error) = std::fs::write(&probe.path, serde_json::to_string_pretty(&probe.log).unwrap_or_default()) {
                eprintln!("--a11y-probe: {error}");
            }
            self.a11y_probe = None;
            self.request_quit();
            return;
        };
        let (kind, rest) = step.split_once(':').unwrap_or((step.as_str(), ""));
        let (name, value) = rest.split_once('=').unwrap_or((rest, ""));
        if kind == "type" {
            mewrk_shell::native::type_keys(view, rest);
            probe.log.push(serde_json::json!({ "step": step }));
            return;
        }
        if let ("at", Some((x, y))) = (kind, rest.split_once(',').and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?)))) {
            mewrk_shell::native::click_view_point(view, x, y);
            probe.log.push(serde_json::json!({ "step": step }));
            return;
        }
        // SAFETY: the selectors of `NSAccessibility`, sent on the main thread to elements the window just answered with, which take what is
        // declared here.
        let found = mewrk_shell::native::act_on_accessibility(view, name, |element: &AnyObject| unsafe {
            match kind {
                "press" => {
                    let done: bool = msg_send![element, accessibilityPerformPress];
                    let _ = done;
                }
                "focus" => {
                    let _: () = msg_send![element, setAccessibilityFocused: true];
                }
                "set" => {
                    let value = objc2_foundation::NSString::from_str(value);
                    let _: () = msg_send![element, setAccessibilityValue: &*value];
                }
                "click" => mewrk_shell::native::click_element(view, element, &[objc2_app_kit::NSEventType::LeftMouseDown, objc2_app_kit::NSEventType::LeftMouseUp]),
                "down" => mewrk_shell::native::click_element(view, element, &[objc2_app_kit::NSEventType::LeftMouseDown]),
                "up" => mewrk_shell::native::click_element(view, element, &[objc2_app_kit::NSEventType::LeftMouseUp]),
                _ => eprintln!("--a11y-steps: no step {kind}"),
            }
        });
        probe.log.push(serde_json::json!({ "step": step, "found": found }));
    }

    /// Hands the OS's drag to the zones, once the event loop has told all of it.
    fn pump_file_drag(&mut self) {
        use mewrk_pane_files::attachment_drop::{handle_native_drag, DragPoint, NativeDragEvent};
        let Some(state) = self.state.as_mut() else { return };
        let position = {
            #[cfg(target_os = "macos")]
            let point = mewrk_shell::native::pointer_in_view(state.core.app.native_view);
            #[cfg(not(target_os = "macos"))]
            let point = state.core.input.pointer;
            let zoom = state.core.scale / state.core.window_scale;
            let (x, y) = point.unwrap_or((0.0, 0.0));
            DragPoint { x: x / zoom, y: y / zoom }
        };
        let text_paths = |paths: &[PathBuf]| paths.iter().map(|path| path.to_string_lossy().into_owned()).collect::<Vec<String>>();
        if !self.drag.dropped.is_empty() {
            let paths = text_paths(&std::mem::take(&mut self.drag.dropped));
            self.drag.hovering.clear();
            self.drag.entered = false;
            handle_native_drag(&mut state.core.app, NativeDragEvent::Drop { paths, position });
            state.window.request_redraw();
        } else if !self.drag.hovering.is_empty() {
            if std::mem::replace(&mut self.drag.entered, true) {
                handle_native_drag(&mut state.core.app, NativeDragEvent::Over { position });
            } else {
                let paths = text_paths(&self.drag.hovering);
                handle_native_drag(&mut state.core.app, NativeDragEvent::Enter { paths, position });
            }
            state.window.request_redraw();
        }
    }

    /// A background that moves, when nothing else has happened: the new frame goes to the GPU and last frame's scene draws over it, so a
    /// frame of video costs what the GPU spends on it and nothing for the view, however much the view holds.
    fn draw_motion(&mut self) {
        let Some(state) = self.state.as_mut() else { return };
        let now = Instant::now();
        let tick = state.core.tick_motion(now);
        self.next_motion = tick.next;
        if tick.rebuild {
            state.window.request_redraw();
            return;
        }
        if !tick.changed || state.present_blocked {
            return;
        }
        let gpu = state.core.renderer.gpu.clone();
        if std::mem::take(&mut state.surface_stale) {
            state.surface.configure(&gpu.device, &state.config);
        }
        let frame = match state.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                state.surface.configure(&gpu.device, &state.config);
                return;
            }
            // Nothing can be shown right now. The frame stays on the GPU for the next look.
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());
        if state.core.draw_again(&view).is_none() {
            // The window changed size, and a full frame is on its way. Nothing was drawn, so this one isn't shown.
            drop(frame);
            state.window.request_redraw();
            return;
        }
        state.window.pre_present_notify();
        gpu.queue.present(frame);
    }

    fn redraw(&mut self) {
        let Some(state) = self.state.as_mut() else { return };
        let gpu = state.core.renderer.gpu.clone();
        let trace_start = Instant::now();
        state.sync_fullscreen();
        if std::mem::take(&mut state.surface_stale) {
            state.surface.configure(&gpu.device, &state.config);
        }
        let frame = match state.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                state.surface.configure(&gpu.device, &state.config);
                state.window.request_redraw();
                return;
            }
            other => {
                if self.trace {
                    eprintln!("blocked t={:.1} {other:?} waited={:.1}", self.launched.elapsed().as_secs_f64() * 1000.0, trace_start.elapsed().as_secs_f64() * 1000.0);
                }
                // Occluded or timed out: nothing can be shown right now, so try again soon.
                state.present_blocked = true;
                self.next_wake = Some(Instant::now() + BLOCKED_RETRY);
                return;
            }
        };
        state.present_blocked = false;
        let view = frame.texture.create_view(&Default::default());
        let acquired = Instant::now();
        let outcome = state.core.frame(&view, &mut self.clipboard);
        if let Some(update) = fluent::el::take_a11y_update() {
            state.a11y.update_if_active(|| update);
        }
        let built = Instant::now();
        state.window.pre_present_notify();
        gpu.queue.present(frame);
        if self.trace {
            let ms = |a: Instant, b: Instant| b.duration_since(a).as_secs_f64() * 1000.0;
            let now = Instant::now();
            eprintln!("frame t={:.1} size={:?} acquire={:.1} build={:.1} present={:.1}", ms(self.launched, now), state.core.size_px, ms(trace_start, acquired), ms(acquired, built), ms(built, now));
        }
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            if std::env::var_os("MEWRK_TRACE").is_some() {
                eprintln!("first frame after {:.1} ms", self.launched.elapsed().as_secs_f64() * 1000.0);
            }
        }

        // Anything that blocks or needs the window waits until the frame is done.
        let requests = std::mem::take(&mut state.core.app.window);
        if requests.drag {
            let _ = state.window.drag_window();
            // AppKit tracks the drag to its end and keeps the release to itself, so the press has to be ended here or the
            // next move reads as a drag of whatever was under it.
            state.core.input.down = false;
            state.core.input.released = true;
            state.window.request_redraw();
        }
        if requests.zoom {
            state.window.set_maximized(!state.window.is_maximized());
        }
        // The window's own light or dark: the title bar's controls and native pages read it.
        let wanted_theme = match (state.core.forced_theme, state.core.app.doc.global_settings.theme) {
            (Some(true), _) | (None, mewrk_client::model::ThemePreference::Night) => Some(winit::window::Theme::Dark),
            (Some(false), _) | (None, mewrk_client::model::ThemePreference::Day) => Some(winit::window::Theme::Light),
            (None, mewrk_client::model::ThemePreference::System) => None,
        };
        if self.window_theme != Some(wanted_theme) {
            self.window_theme = Some(wanted_theme);
            state.window.set_theme(wanted_theme);
            // AppKit puts the lights back whenever it lays the title bar out again.
            place_traffic_lights(&state.window);
        }
        #[cfg(target_os = "macos")]
        if let Some(native) = &mut self.native {
            native.sync_language(state.core.app.language_is_chinese());
        }

        let (show_requested, hide_requested, quit_requested) = (requests.show, requests.hide, requests.quit);
        state.window.set_cursor(match outcome.cursor {
            CursorKind::Default => winit::window::CursorIcon::Default,
            CursorKind::Hand => winit::window::CursorIcon::Pointer,
            CursorKind::Text => winit::window::CursorIcon::Text,
            CursorKind::ColResize => winit::window::CursorIcon::ColResize,
            CursorKind::RowResize => winit::window::CursorIcon::RowResize,
            CursorKind::Grab => winit::window::CursorIcon::Grab,
            CursorKind::Grabbing => winit::window::CursorIcon::Grabbing,
            CursorKind::Crosshair => winit::window::CursorIcon::Crosshair,
        });
        if let Some(area) = outcome.ime_area {
            let scale = state.core.scale as f64;
            state.window.set_ime_cursor_area(PhysicalPosition::new(area.x as f64 * scale, area.y as f64 * scale), PhysicalSize::new(area.w as f64 * scale, area.h as f64 * scale));
        }
        self.next_wake = outcome.wake_in.map(|s| Instant::now() + Duration::from_secs_f32(s.max(0.001)));
        self.next_motion = outcome.motion_next;
        if outcome.animating || self.fullscreen_test.is_some() || self.animate_test {
            state.window.request_redraw();
        }
        if show_requested {
            self.show_main_window();
        }
        if hide_requested {
            self.hide_main_window();
        }
        if quit_requested {
            self.request_quit();
        }
    }

    fn on_key(&mut self, event: winit::event::KeyEvent) {
        let Some(state) = self.state.as_mut() else { return };
        if event.state != ElementState::Pressed {
            return;
        }
        let cmd = self.modifiers.super_key();
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        // Option types accented letters in a text field and is Meta in a terminal, so the same chord means two things
        // depending on who has the keyboard.
        let terminal = root::is_terminal_focus(&mut state.core.app, state.core.mem.focus);
        let meta = alt && terminal;
        let plain = event.key_without_modifiers();
        let key = match &event.logical_key {
            WKey::Named(NamedKey::Enter) => Some(Key::Enter),
            WKey::Named(NamedKey::Backspace) => Some(Key::Backspace),
            WKey::Named(NamedKey::Delete) => Some(Key::Delete),
            WKey::Named(NamedKey::ArrowLeft) => Some(Key::Left),
            WKey::Named(NamedKey::ArrowRight) => Some(Key::Right),
            WKey::Named(NamedKey::ArrowUp) => Some(Key::Up),
            WKey::Named(NamedKey::ArrowDown) => Some(Key::Down),
            WKey::Named(NamedKey::Home) => Some(Key::Home),
            WKey::Named(NamedKey::End) => Some(Key::End),
            WKey::Named(NamedKey::PageUp) => Some(Key::PageUp),
            WKey::Named(NamedKey::PageDown) => Some(Key::PageDown),
            WKey::Named(NamedKey::Escape) => Some(Key::Escape),
            WKey::Named(NamedKey::Tab) => Some(Key::Tab),
            WKey::Named(NamedKey::F1) => Some(Key::F(1)),
            WKey::Named(NamedKey::F2) => Some(Key::F(2)),
            WKey::Named(NamedKey::F3) => Some(Key::F(3)),
            WKey::Named(NamedKey::F4) => Some(Key::F(4)),
            WKey::Named(NamedKey::F5) => Some(Key::F(5)),
            WKey::Named(NamedKey::F6) => Some(Key::F(6)),
            WKey::Named(NamedKey::F7) => Some(Key::F(7)),
            WKey::Named(NamedKey::F8) => Some(Key::F(8)),
            WKey::Named(NamedKey::F9) => Some(Key::F(9)),
            WKey::Named(NamedKey::F10) => Some(Key::F(10)),
            WKey::Named(NamedKey::F11) => Some(Key::F(11)),
            WKey::Named(NamedKey::F12) => Some(Key::F(12)),
            WKey::Named(NamedKey::Space) if cmd || ctrl || meta || !terminal => Some(Key::Char(' ')),
            WKey::Character(s) if cmd || ctrl => s.chars().next().map(|c| Key::Char(c.to_ascii_lowercase())),
            WKey::Character(_) if meta => match &plain {
                WKey::Character(s) => s.chars().next().map(Key::Char),
                _ => None,
            },
            // Plain letters are key presses too: a shortcut can be bound to one, a recorder has to hear it, and a focused row
            // acts on Space. A terminal would type them twice, once as the key and once as text, so its keys stay as they were.
            // Option's accent is not the key's name: the letter under it is, the way `KeyboardEvent.code` read.
            WKey::Character(s) if !terminal => match (&plain, alt) {
                (WKey::Character(p), true) => p.chars().next().map(Key::Char),
                _ => s.chars().next().map(Key::Char),
            },
            _ => None,
        };
        if let Some(key) = key {
            state.core.input.keys.push(KeyPress { key, shift: self.modifiers.shift_key(), cmd, alt: self.modifiers.alt_key(), ctrl });
        }
        if !cmd
            && !ctrl
            && !(meta && matches!(event.logical_key, WKey::Character(_) | WKey::Named(NamedKey::Space)))
            && let Some(text) = &event.text
        {
            state.core.input.text.push_str(text);
        }
        state.window.request_redraw();
    }
}

impl ApplicationHandler<UserEvent> for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() || self.fatal.is_some() {
            return;
        }
        match self.create_window(event_loop) {
            Ok(state) => self.state = state,
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::A11y(event) => {
                if self.trace {
                    eprintln!("accessibility t={:.1} {:?}", self.launched.elapsed().as_secs_f64() * 1000.0, event.window_event);
                }
                match event.window_event {
                    accesskit_winit::WindowEvent::InitialTreeRequested => {
                        fluent::el::set_a11y_active(true);
                        mewrk_client::chromium_set_accessibility(true);
                    }
                    accesskit_winit::WindowEvent::ActionRequested(request) => fluent::el::a11y_action(request),
                    accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                        fluent::el::set_a11y_active(false);
                        mewrk_client::chromium_set_accessibility(false);
                    }
                }
                if let Some(state) = &self.state {
                    state.window.request_redraw();
                }
            }
            UserEvent::Show => self.show_main_window(),
            #[cfg(target_os = "macos")]
            UserEvent::Native(native) => match native {
                mewrk_shell::native::NativeEvent::Quit => self.request_quit(),
                mewrk_shell::native::NativeEvent::Show => self.show_main_window(),
                mewrk_shell::native::NativeEvent::LeftFullScreen => {
                    if let Some(state) = &self.state {
                        place_traffic_lights(&state.window);
                        state.window.request_redraw();
                    }
                    if std::mem::take(&mut self.hide_once_windowed) {
                        self.hide_main_window();
                    }
                }
                mewrk_shell::native::NativeEvent::Settings => {
                    self.show_main_window();
                    if let Some(state) = &mut self.state {
                        mewrk_shell::actions::open_global_settings(&mut state.core.app, "providers");
                        state.window.request_redraw();
                    }
                }
            },
            UserEvent::Exit(code) => {
                if let Some(state) = &self.state {
                    state.core.app.host.finalize();
                }
                self.exit_code = code;
                event_loop.exit();
            }
            UserEvent::Wake => {
                if let Some(state) = &self.state
                    && !state.present_blocked
                {
                    state.window.request_redraw();
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let Some(fatal) = &mut self.fatal {
            fatal.event(event_loop, event);
            return;
        }
        if let Some(state) = &mut self.state {
            state.a11y.process_event(&state.window, &event);
        }
        if let WindowEvent::RedrawRequested = event {
            self.redraw();
            return;
        }
        if let WindowEvent::KeyboardInput { event, .. } = event {
            self.on_key(event);
            return;
        }
        if let WindowEvent::CloseRequested = event {
            // Without a menu bar item there would be no way back to a hidden window, so the close has to quit.
            if self.tray_installed() { self.hide_main_window() } else { self.request_quit() }
            return;
        }
        let Some(state) = self.state.as_mut() else { return };
        let input = &mut state.core.input;
        let scale = state.core.scale;
        match event {
            WindowEvent::Resized(size) => {
                state.config.width = size.width.max(1);
                state.config.height = size.height.max(1);
                state.surface_stale = true;
                state.core.size_px = (state.config.width, state.config.height);
                state.sync_fullscreen();
                // AppKit puts the lights back whenever it lays the title bar out again.
                place_traffic_lights(&state.window);
                if self.trace {
                    eprintln!("resize t={:.1} size={:?}", self.launched.elapsed().as_secs_f64() * 1000.0, state.core.size_px);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => state.core.window_scale = scale_factor as f32,
            WindowEvent::ThemeChanged(theme) => state.core.app.system_dark = theme == winit::window::Theme::Dark,
            WindowEvent::Focused(focused) => {
                // Key and not key are two title bars to AppKit, which lays out afresh between them.
                place_traffic_lights(&state.window);
                if !focused {
                    state.window.request_redraw();
                    return;
                }
                state.present_blocked = false;
                #[cfg(target_os = "macos")]
                fluent::set_reduced_motion(mewrk_shell::native::reduce_motion());
                // The panels that read something a terminal may have changed, and Git's polling, which backs off.
                mewrk_settings_global::window_refocused(&mut state.core.app);
                mewrk_pane_git::git_polling_wake(&mut state.core.app);
            }
            WindowEvent::Occluded(occluded) => {
                if !occluded {
                    state.present_blocked = false;
                    // A background that moves stopped asking while it couldn't be seen.
                    state.window.request_redraw();
                }
                mewrk_pane_git::git_polling_set_visible(&mut state.core.app, !occluded);
                mewrk_pane_browser::set_window_visible(&mut state.core.app, !occluded);
                mewrk_settings_global::set_window_visible(&mut state.core.app, !occluded);
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
                // A pinch or Cmd+wheel zooms a picture rather than scrolling.
                input.zoom_modifier = self.modifiers.super_key() || self.modifiers.control_key();
                input.shift = self.modifiers.shift_key();
                input.alt = self.modifiers.alt_key();
            }
            WindowEvent::CursorMoved { position, .. } => input.pointer = Some((position.x as f32 / scale, position.y as f32 / scale)),
            WindowEvent::CursorLeft { .. } => input.pointer = None,
            // On a Mac, Control-click is a right click, which a web engine would have told upstream by itself.
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } if cfg!(target_os = "macos") && self.modifiers.control_key() => {
                self.control_click = true;
                input.right_pressed = true;
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } if std::mem::take(&mut self.control_click) => {}
            WindowEvent::MouseInput { state: button_state, button: MouseButton::Left, .. } => match button_state {
                ElementState::Pressed => {
                    input.down = true;
                    input.pressed = true;
                    let now = Instant::now();
                    input.double_click = self.last_click.is_some_and(|l| now.duration_since(l) < Duration::from_millis(400));
                    self.last_click = Some(now);
                }
                ElementState::Released => {
                    input.down = false;
                    input.released = true;
                }
            },
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => input.right_pressed = true,
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Middle, .. } => input.middle_pressed = true,
            // A mouse's own back and forward buttons walk the conversations opened before.
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Back, .. } => state.core.app.step_conversation_history(-1),
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Forward, .. } => state.core.app.step_conversation_history(1),
            WindowEvent::MouseWheel { delta, .. } => match delta {
                MouseScrollDelta::LineDelta(x, y) => {
                    input.scroll += y * 48.0;
                    input.scroll_x += x * 48.0;
                    input.scroll_is_pixels = false;
                }
                MouseScrollDelta::PixelDelta(p) => {
                    input.scroll += p.y as f32 / scale;
                    input.scroll_x += p.x as f32 / scale;
                    input.scroll_is_pixels = true;
                }
            },
            WindowEvent::Ime(ime) => match ime {
                Ime::Commit(text) => {
                    input.ime_preedit = None;
                    input.ime_commit = Some(text);
                }
                Ime::Preedit(text, _) => input.ime_preedit = Some(text),
                Ime::Disabled => input.ime_preedit = None,
                Ime::Enabled => {}
            },
            WindowEvent::DroppedFile(path) => {
                input.hovered_file = false;
                // The drop commands are scoped to what the OS says was dragged.
                state.core.app.host.drag_dropped(std::slice::from_ref(&path));
                self.drag.dropped.push(path.clone());
                input.hovered_paths.clear();
                input.dropped.push(path);
            }
            WindowEvent::HoveredFile(path) => {
                state.core.app.host.drag_entered(std::slice::from_ref(&path));
                input.hovered_file = true;
                input.hovered_paths.push(path.clone());
                self.drag.hovering.push(path);
            }
            WindowEvent::HoveredFileCancelled => {
                input.hovered_file = false;
                input.hovered_paths.clear();
                self.drag = FileDrag::default();
                mewrk_pane_files::attachment_drop::handle_native_drag(&mut state.core.app, mewrk_pane_files::attachment_drop::NativeDragEvent::Leave);
            }
            _ => return,
        }
        state.window.request_redraw();
    }

    /// Quits that bypass every menu (Dock → Quit, logout) arrive as this, whatever the handler does.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = &mut self.state {
            state.core.app.save_conversation_turns();
            state.core.app.flush_ui_store();
            state.core.app.host.finalize();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.step_a11y_probe();
        self.pump_file_drag();
        if let Some(state) = &mut self.state {
            state.sync_fullscreen();
        }
        if self.next_motion.is_some_and(|at| at <= Instant::now()) {
            self.draw_motion();
        }
        // `--fullscreen-test`: goes in at 3 s and comes back at 7 s, for a trace of the transitions.
        if let Some(when) = self.fullscreen_test
            && Instant::now() >= when
            && let Some(state) = &self.state
        {
            let entering = state.window.fullscreen().is_none();
            eprintln!("toggle t={:.1} entering={entering}", self.launched.elapsed().as_secs_f64() * 1000.0);
            state.window.set_fullscreen(if entering { Some(winit::window::Fullscreen::Borderless(None)) } else { None });
            self.fullscreen_test = Some(Instant::now() + Duration::from_secs(4));
        }
        if let Some(when) = self.exit_after
            && Instant::now() >= when
        {
            if let Some(state) = &self.state {
                state.core.app.host.finalize();
            }
            event_loop.exit();
            return;
        }
        let mut wake = self.next_wake;
        if let Some(at) = self.next_motion {
            wake = Some(wake.map_or(at, |w| w.min(at)));
        }
        if !self.drag.hovering.is_empty() {
            // AppKit sends no moves while files are dragged, so the zones are told where the pointer is on a clock.
            let next = Instant::now() + DRAG_POLL;
            wake = Some(wake.map_or(next, |w| w.min(next)));
        }
        if let Some(exit_at) = self.exit_after {
            wake = Some(wake.map_or(exit_at, |w| w.min(exit_at)));
        }
        if let Some(at) = self.fullscreen_test {
            wake = Some(wake.map_or(at, |w| w.min(at)));
        }
        if let Some(at) = self.a11y_probe.as_ref().map(|probe| probe.next) {
            wake = Some(wake.map_or(at, |w| w.min(at)));
        }
        match wake {
            // Only a look at the clock for a moving background, and that has been seen to above.
            Some(when) if when <= Instant::now() && self.next_wake.is_none_or(|at| at > Instant::now()) => event_loop.set_control_flow(ControlFlow::Poll),
            Some(when) if when <= Instant::now() => {
                self.next_wake = None;
                if let Some(state) = &mut self.state {
                    if state.present_blocked {
                        self.redraw();
                    } else {
                        state.window.request_redraw();
                    }
                }
                event_loop.set_control_flow(if self.next_wake.is_some() { ControlFlow::Poll } else { ControlFlow::Wait });
            }
            Some(when) => event_loop.set_control_flow(ControlFlow::WaitUntil(when)),
            // Nothing is happening, so the process sleeps: no timers, no spinning, no GPU work.
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// The menu bar's picture: the eared mark, black on clear, which the bar tints for a light, dark or highlighted bar. A status
/// item's image is 18 pt tall; at 2x that is 36 px, and the mark 22 px wide keeps its sides on whole pixels and sits it at the
/// weight of the system's own menu bar glyphs. `native::install` gives the picture its size in points.
#[cfg(target_os = "macos")]
fn tray_template_png() -> Option<Vec<u8>> {
    use resvg::{tiny_skia, usvg};
    use mewrk_shell::chrome::{MARK_HEIGHT, MARK_PATH, MARK_WIDTH};
    const WIDTH: u32 = 22;
    const HEIGHT: u32 = 36;
    // The bar's own height in the mark's units, with the mark centred in it.
    let room = HEIGHT as f32 * MARK_WIDTH / WIDTH as f32;
    let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 {} {MARK_WIDTH} {room}"><path d="{MARK_PATH}" fill="black"/></svg>"#, (MARK_HEIGHT - room) / 2.0);
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(WIDTH, HEIGHT)?;
    let k = WIDTH as f32 / MARK_WIDTH;
    resvg::render(&tree, tiny_skia::Transform::from_scale(k, k), &mut pixmap.as_mut());
    let image = image::RgbaImage::from_raw(WIDTH, HEIGHT, pixmap.take())?;
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image).write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(png.into_inner())
}

/// The view winit draws into, which native child views get added to.
fn content_view(window: &Window) -> *mut std::ffi::c_void {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else { return std::ptr::null_mut() };
    match handle.as_raw() {
        RawWindowHandle::AppKit(h) => h.ns_view.as_ptr(),
        _ => std::ptr::null_mut(),
    }
}

/// Puts the traffic lights where Mewrk's layout expects them. Full screen has its own, in `native`.
fn place_traffic_lights(window: &Window) {
    let view = content_view(window);
    if view.is_null() {
        return;
    }
    // SAFETY: winit's content view, retained by its window, on the main thread.
    if let Some(window) = unsafe { &*(view as *const objc2_app_kit::NSView) }.window() {
        mewrk_shell::native::place_lights(&window);
    }
}

fn run_windowed(args: Args) -> Result<(), String> {
    let launched = Instant::now();
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        // The menu bar is `native::install`'s, worded in the app's language and with a Quit that runs the barrier.
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        builder.with_default_menu(false);
    }
    let event_loop = builder.build().map_err(|e| e.to_string())?;
    let proxy = event_loop.create_proxy();
    let fullscreen_test = args.flag("--fullscreen-test").then(|| Instant::now() + Duration::from_secs(3));
    let exit_after = args.value("--exit-after").and_then(|v| v.parse::<f32>().ok()).map(|s| Instant::now() + Duration::from_secs_f32(s));
    let a11y_probe = args.value("--a11y-probe").map(|path| A11yProbe { path: PathBuf::from(path), steps: args.value("--a11y-steps").map_or_else(Default::default, |steps| steps.split(';').map(str::to_owned).collect()), next: Instant::now() + Duration::from_secs(3), asked: false, log: Vec::new() });
    let mut shell = Shell {
        fatal: None,
        args,
        proxy,
        state: None,
        modifiers: ModifiersState::empty(),
        clipboard: SystemClipboard(arboard::Clipboard::new().ok()),
        next_wake: None,
        next_motion: None,
        last_click: None,
        control_click: false,
        exit_after,
        error: None,
        launched,
        first_frame_logged: false,
        trace: std::env::var_os("MEWRK_TRACE").is_some(),
        fullscreen_test,
        animate_test: std::env::var_os("MEWRK_ANIMATE").is_some(),
        exit_code: 0,
        #[cfg(target_os = "macos")]
        native: None,
        window_theme: None,
        drag: FileDrag::default(),
        a11y_probe,
        hide_once_windowed: false,
    };
    event_loop.run_app(&mut shell).map_err(|e| e.to_string())?;
    match shell.error {
        Some(e) => Err(e),
        None => {
            if shell.exit_code != 0 {
                std::process::exit(shell.exit_code);
            }
            Ok(())
        }
    }
}

fn main() {
    let args = Args(std::env::args().skip(1).collect());
    if args.flag("--help") || args.flag("-h") {
        println!("{USAGE}");
        return;
    }
    // `ssh` runs this executable to ask the user something; answer and leave before anything else loads.
    if let Some(code) = mewrk_client::askpass_main() {
        std::process::exit(code);
    }
    // A development build has no helper bundle, so Chromium relaunches this executable for its renderer, GPU and utility
    // processes, which must never reach the application.
    if let Some(code) = mewrk_client::chromium_subprocess_main() {
        std::process::exit(code);
    }
    if !args.flag("--headless") {
        mewrk_client::chromium_preload_framework();
    }
    mewrk_client::prepare_process();
    if args.flag(fatal::RESET_FLAG) {
        fatal::run_reset(reset_config());
    }
    let result = if args.flag("--headless") { run_headless(&args) } else { run_windowed(args) };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
