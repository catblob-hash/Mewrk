//! Fluent Agent: a native streaming agent client.

mod app;
mod bridge;
mod chat;
mod swarm;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use app::{App, ConnSettings, Page, ThemeMode};
use bridge::Notifier;
use fluent::gpu::SURFACE_FORMAT;
use fluent::ui::{Clipboard, CursorKind, Input, Key, KeyPress, NoClipboard, Ui, UiMemory};
use fluent::{Gpu, Renderer, Scene, TextCtx};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

const USAGE: &str = "\
fluent-agent [--url URL] [--ca PEM] [--model NAME] [--thinking on|off] [--http2] [--stall-secs N] [--media-inflight N]
             [--mock] [--mock-token-us N] [--mock-reasoning N]
             [--page chat|swarm|settings] [--theme system|light|dark] [--swarm] [--scenario N] [--size-choice N]
             [--ask TEXT] [--image PATH] [--video PATH] [--attach PATH] [--on-top] [--exit-after SECONDS] [--frame-log PATH]
headless:    --headless [--size WxH] [--scale F] [--frames N] [--bench SECONDS] [--shot PNG] [--json PATH]";

struct Args(Vec<String>);

impl Args {
    fn value(&self, name: &str) -> Option<&str> {
        self.0.iter().position(|a| a == name).and_then(|i| self.0.get(i + 1)).map(String::as_str)
    }
    fn all(&self, name: &str) -> Vec<&str> {
        self.0.iter().enumerate().filter(|(_, a)| *a == name).filter_map(|(i, _)| self.0.get(i + 1)).map(String::as_str).collect()
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

/// The part of the app that exists regardless of whether there is a window.
struct Core {
    app: App,
    text: TextCtx,
    scene: Scene,
    mem: UiMemory,
    input: Input,
    renderer: Renderer,
    started: Instant,
    last_frame: Instant,
    scale: f32,
    size_px: (u32, u32),
    timings: Vec<FrameTiming>,
    keep_timings: bool,
}

#[derive(Clone, Copy, Default)]
struct FrameTiming {
    drain_us: u32,
    build_us: u32,
    render_us: u32,
    rects: u32,
    glyphs: u32,
}

struct FrameOutcome {
    animating: bool,
    wake_in: Option<f32>,
    cursor: CursorKind,
    ime_area: Option<fluent::ui::Rect>,
    submission: wgpu::SubmissionIndex,
}

impl Core {
    /// Takes in what the runtime produced without drawing. A window that cannot present (occluded, minimised)
    /// still has to do this, or the wake-up that comes with the next frame would never be re-armed, and the
    /// window would come back blank when it reappears.
    fn drain_only(&mut self) {
        let time = self.started.elapsed().as_secs_f32();
        let theme = self.app.theme();
        let logical_w = self.size_px.0 as f32 / self.scale;
        let area_w = logical_w - if logical_w < 760.0 { 48.0 } else { 188.0 };
        self.text.set_scale(self.scale);
        self.app.drain(&mut self.text, &theme, time, area_w);
    }

    fn frame(&mut self, target: &wgpu::TextureView, clipboard: &mut dyn Clipboard) -> FrameOutcome {
        let t0 = Instant::now();
        let dt = t0.duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = t0;
        let time = t0.duration_since(self.started).as_secs_f32();
        let theme = self.app.theme();
        let size = (self.size_px.0 as f32 / self.scale, self.size_px.1 as f32 / self.scale);
        self.text.set_scale(self.scale);

        let area_w = size.0 - if size.0 < 760.0 { 48.0 } else { 188.0 };
        self.app.drain(&mut self.text, &theme, time, area_w);
        self.app.prepare_gpu(&mut self.renderer);
        let t1 = Instant::now();

        let epoch = self.text.epoch;
        self.scene.clear();
        let mut ui = Ui::new(&mut self.scene, &mut self.text, theme, &self.input, &mut self.mem, clipboard, self.scale, size, dt, time);
        self.app.show(&mut ui);
        let (animating, wake_in, cursor, ime_area) = (ui.animating, ui.wake_in, ui.cursor, ui.ime_area);
        drop(ui);
        // An atlas reset partway through means glyphs built earlier in this frame are stale. The next one is clean.
        let animating = animating || self.text.epoch != epoch;
        self.input.end_frame();
        let t2 = Instant::now();

        self.text.flush_atlases(&self.renderer);
        let submission = self.renderer.render(&self.scene, &theme, target, self.size_px);
        let t3 = Instant::now();

        let cpu_ms = t3.duration_since(t0).as_secs_f32() * 1000.0;
        self.app.perf.record(cpu_ms, time);
        if self.keep_timings {
            let (rects, glyphs) = self.scene.counts();
            self.timings.push(FrameTiming {
                drain_us: t1.duration_since(t0).as_micros() as u32,
                build_us: t2.duration_since(t1).as_micros() as u32,
                render_us: t3.duration_since(t2).as_micros() as u32,
                rects: rects as u32,
                glyphs: glyphs as u32,
            });
        }
        FrameOutcome { animating, wake_in, cursor, ime_area, submission }
    }
}

fn build_settings(args: &Args) -> Result<ConnSettings, String> {
    Ok(ConnSettings {
        url: args.value("--url").unwrap_or("http://127.0.0.1:8080/v1").to_owned(),
        ca_path: args.value("--ca").unwrap_or("").to_owned(),
        model: args.value("--model").unwrap_or("LFM3-VL-2B").to_owned(),
        thinking: args.value("--thinking") != Some("off"),
        http2: args.flag("--http2"),
        stall_secs: args.num("--stall-secs", 8)?,
        media_inflight: args.num("--media-inflight", agent_rt::RuntimeConfig::default().max_media_inflight)?,
    })
}

fn apply_startup_args(app: &mut App, args: &Args) -> Result<(), String> {
    app.page = match args.value("--page") {
        Some("swarm") => Page::Swarm,
        Some("settings") => Page::Settings,
        _ => Page::Chat,
    };
    app.theme_mode = match args.value("--theme") {
        Some("light") => ThemeMode::Light,
        Some("dark") => ThemeMode::Dark,
        _ => ThemeMode::System,
    };
    app.swarm.scenario = args.num("--scenario", app.swarm.scenario)?;
    app.swarm.size_choice = args.num("--size-choice", app.swarm.size_choice)?;
    let load = |flag: &str| -> Result<Option<Arc<agent_rt::media::MediaInput>>, String> {
        args.value(flag)
            .map(|p| agent_rt::media::MediaInput::from_path_blocking(std::path::Path::new(p), agent_rt::media::DEFAULT_MAX_IMAGE_EDGE).map(Arc::new).map_err(|e| e.0))
            .transpose()
    };
    app.swarm.media.image = load("--image")?;
    app.swarm.media.video = load("--video")?;
    let attachments: Vec<PathBuf> = args.all("--attach").into_iter().map(PathBuf::from).collect();
    if !attachments.is_empty() {
        let conn_handle = &app.conn;
        // The borrow of `conn` ends with this call, since attach_paths only clones what it needs from it.
        let (chat, conn) = (&mut app.chat, conn_handle);
        chat.attach_paths(attachments, conn);
    }
    if let Some(ask) = args.value("--ask") {
        app.chat.auto_submit = Some(ask.to_owned());
    }
    if args.flag("--swarm") {
        app.page = Page::Swarm;
        app.swarm.start(&app.conn, 0.0);
    }
    Ok(())
}

fn tokio_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("agent-rt").build().map_err(|e| e.to_string())
}

/// Starts the in-process mock, if asked for, and returns the URL to use. The whole client path stays real.
fn maybe_mock(args: &Args, handle: &tokio::runtime::Handle) -> Result<Option<(String, chaos::mock::MockServer)>, String> {
    if !args.flag("--mock") {
        return Ok(None);
    }
    let cfg = chaos::mock::MockConfig {
        token_interval: Duration::from_micros(args.num("--mock-token-us", 25_000)?),
        first_byte_delay: Duration::from_millis(args.num("--mock-first-byte-ms", 80)?),
        reasoning_tokens: args.num("--mock-reasoning", 0)?,
        tools: true,
        ..Default::default()
    };
    let server = handle.block_on(chaos::mock::MockServer::spawn(cfg)).map_err(|e| e.to_string())?;
    Ok(Some((format!("http://{}/v1", server.addr), server)))
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
        let s = args.value("--size").unwrap_or("1600x1000");
        let (w, h) = s.split_once('x').ok_or("--size WxH")?;
        (w.parse().map_err(|_| "--size")?, h.parse().map_err(|_| "--size")?)
    };
    let scale: f32 = args.num("--scale", 2.0)?;
    let frames: u32 = args.num("--frames", 120)?;
    let bench_secs: f32 = args.num("--bench", 0.0)?;
    let warmup: f32 = args.num("--warmup", 1.0)?;

    let runtime = tokio_runtime()?;
    let mock = maybe_mock(args, runtime.handle())?;
    let mut settings = build_settings(args)?;
    if let Some((url, _)) = &mock {
        settings.url = url.clone();
    }
    let notifier = Notifier::new(Box::new(|| {}));
    let mut app = App::new(notifier, settings, runtime.handle().clone())?;
    app.system_dark = args.value("--theme") != Some("light");
    apply_startup_args(&mut app, args)?;

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
    let mut core = Core {
        app,
        text: TextCtx::new(scale),
        scene: Scene::default(),
        mem: UiMemory::default(),
        input: Input::default(),
        renderer,
        started: now,
        last_frame: now,
        scale,
        size_px: size,
        timings: Vec::new(),
        keep_timings: false,
    };

    let mut clipboard = NoClipboard;
    let vsync = Duration::from_secs_f64(1.0 / 120.0);
    let run_started = Instant::now();
    let mut gpu_us: Vec<u32> = Vec::new();
    let mut frame_no = 0u32;
    loop {
        let elapsed = run_started.elapsed().as_secs_f32();
        if bench_secs > 0.0 {
            if elapsed >= warmup + bench_secs {
                break;
            }
            core.keep_timings = elapsed >= warmup;
        } else if frame_no >= frames {
            break;
        }
        let frame_begin = Instant::now();
        let outcome = core.frame(&view, &mut clipboard);
        let t = Instant::now();
        let _ = gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(outcome.submission), timeout: Some(Duration::from_secs(5)) });
        if core.keep_timings {
            gpu_us.push(t.elapsed().as_micros() as u32);
        }
        frame_no += 1;
        // Paced like a 120 Hz display, so that this measures work per frame instead of a tight loop.
        let spent = frame_begin.elapsed();
        if spent < vsync {
            std::thread::sleep(vsync - spent);
        }
    }

    if bench_secs > 0.0 {
        let t = &core.timings;
        let col = |f: fn(&FrameTiming) -> u32| {
            let mut v: Vec<u32> = t.iter().map(f).collect();
            v.sort_unstable();
            v
        };
        let total = col(|f| f.drain_us + f.build_us + f.render_us);
        let drain = col(|f| f.drain_us);
        let build = col(|f| f.build_us);
        let render = col(|f| f.render_us);
        gpu_us.sort_unstable();
        let stat = |v: &[u32]| serde_json::json!({"p50_ms": percentile(v, 0.5) as f64 / 1000.0, "p99_ms": percentile(v, 0.99) as f64 / 1000.0, "max_ms": percentile(v, 1.0) as f64 / 1000.0});
        let glyphs = col(|f| f.glyphs);
        let rects = col(|f| f.rects);
        let report = serde_json::json!({
            "frames": t.len(),
            "fps": t.len() as f64 / bench_secs as f64,
            "cpu_total": stat(&total),
            "cpu_drain": stat(&drain),
            "cpu_build": stat(&build),
            "cpu_render_submit": stat(&render),
            "gpu_wait": stat(&gpu_us),
            "glyphs_p50": percentile(&glyphs, 0.5),
            "glyphs_max": percentile(&glyphs, 1.0),
            "rects_p50": percentile(&rects, 0.5),
            "tokens_total": core.app.swarm.total_tokens,
            "swarm_tok_s": core.app.swarm.current_rate(),
            "retries": core.app.swarm.retries,
            "resumes": core.app.swarm.resumes,
            "failed": core.app.swarm.finished_failed,
            "text": format!("{:?}", core.text.stats),
            "rss_mib": rss_mib(),
        });
        println!("{}", serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?);
        if let Some(path) = args.value("--json") {
            std::fs::write(path, report.to_string()).map_err(|e| format!("{path}: {e}"))?;
        }
    }

    if let Some(path) = args.value("--shot") {
        save_png(&gpu, &target, size, path)?;
        println!("wrote {path}");
    }
    drop(mock);
    Ok(())
}

#[cfg(not(unix))]
fn rss_mib() -> f64 {
    0.0
}

#[cfg(unix)]
fn rss_mib() -> f64 {
    // SAFETY: rusage is plain old data and getrusage fully initialises it on success.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return 0.0;
        }
        if cfg!(target_os = "macos") { usage.ru_maxrss as f64 / 1048576.0 } else { usage.ru_maxrss as f64 / 1024.0 }
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

struct WindowState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    core: Core,
    /// The last attempt to get a drawable failed. Until one succeeds, wake-ups drain on a timer.
    present_blocked: bool,
}

struct Shell {
    args: Args,
    proxy: EventLoopProxy<UserEvent>,
    runtime: tokio::runtime::Runtime,
    state: Option<WindowState>,
    modifiers: ModifiersState,
    clipboard: SystemClipboard,
    next_wake: Option<Instant>,
    last_click: Option<Instant>,
    exit_after: Option<Instant>,
    error: Option<String>,
    _mock: Option<chaos::mock::MockServer>,
    frames: u64,
    frame_log: Option<PathBuf>,
    created: Instant,
}

impl Shell {
    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<WindowState, String> {
        let mut attributes = Window::default_attributes().with_title("Fluent Agent").with_inner_size(LogicalSize::new(1280.0, 840.0)).with_min_inner_size(LogicalSize::new(480.0, 420.0));
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attributes = attributes.with_titlebar_transparent(true).with_fullsize_content_view(true).with_title_hidden(true);
        }
        // Floats above other windows without taking keyboard focus from whatever the user is doing.
        if self.args.flag("--on-top") {
            attributes = attributes.with_window_level(winit::window::WindowLevel::AlwaysOnTop);
        }
        let window = Arc::new(event_loop.create_window(attributes).map_err(|e| e.to_string())?);
        window.set_ime_allowed(true);
        #[cfg(target_os = "macos")]
        if self.args.flag("--on-top") {
            float_over_everything(&window);
        }

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
        // One frame in flight: every additional one is another vsync between a token and the screen.
        config.desired_maximum_frame_latency = 1;
        config.view_formats = vec![];
        surface.configure(&gpu.device, &config);

        let mut settings = build_settings(&self.args)?;
        let mock = maybe_mock(&self.args, self.runtime.handle())?;
        if let Some((url, server)) = mock {
            settings.url = url;
            self._mock = Some(server);
        }
        let proxy = self.proxy.clone();
        let notifier = Notifier::new(Box::new(move || {
            let _ = proxy.send_event(UserEvent::Wake);
        }));
        let scale = window.scale_factor() as f32;
        let mut app = App::new(notifier, settings, self.runtime.handle().clone())?;
        app.system_dark = window.theme().is_none_or(|t| t == winit::window::Theme::Dark);
        apply_startup_args(&mut app, &self.args)?;

        let now = Instant::now();
        let core = Core {
            app,
            text: TextCtx::new(scale),
            scene: Scene::default(),
            mem: UiMemory::default(),
            input: Input::default(),
            renderer,
            started: now,
            last_frame: now,
            scale,
            size_px: (config.width, config.height),
            timings: Vec::new(),
            keep_timings: self.frame_log.is_some(),
        };
        // A window the user just launched should come forward, while a floating one stays out of their typing.
        if !self.args.flag("--on-top") {
            window.focus_window();
        }
        window.request_redraw();
        Ok(WindowState { window, surface, config, core, present_blocked: false })
    }

    fn redraw(&mut self) {
        let Some(state) = self.state.as_mut() else { return };
        let gpu = state.core.renderer.gpu.clone();
        let frame = match state.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                state.surface.configure(&gpu.device, &state.config);
                state.window.request_redraw();
                return;
            }
            other => {
                // Occluded or timed out: nothing can be shown right now, but the world goes on.
                if !state.present_blocked && std::env::var_os("FLUENT_TRACE").is_some() {
                    eprintln!("no drawable: {}", match other {
                        wgpu::CurrentSurfaceTexture::Occluded => "occluded",
                        wgpu::CurrentSurfaceTexture::Timeout => "timeout",
                        _ => "other",
                    });
                }
                state.present_blocked = true;
                state.core.drain_only();
                self.next_wake = Some(Instant::now() + BLOCKED_RETRY);
                return;
            }
        };
        state.present_blocked = false;
        let view = frame.texture.create_view(&Default::default());
        let outcome = state.core.frame(&view, &mut self.clipboard);
        state.window.pre_present_notify();
        gpu.queue.present(frame);
        self.frames += 1;

        // File picking is deferred to here, as it blocks and must not happen in the middle of a frame.
        if std::mem::take(&mut state.core.app.chat.pick_files) {
            let files = rfd::FileDialog::new().add_filter("图片或视频", &["png", "jpg", "jpeg", "gif", "bmp", "webp", "mp4", "mov", "mkv", "webm", "avi"]).pick_files();
            if let Some(files) = files {
                let app = &mut state.core.app;
                app.chat.attach_paths(files, &app.conn);
            }
            state.window.request_redraw();
        }

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
        if outcome.animating {
            state.window.request_redraw();
        }
    }

    fn on_key(&mut self, event: winit::event::KeyEvent) {
        let Some(state) = self.state.as_mut() else { return };
        if event.state != ElementState::Pressed {
            return;
        }
        let cmd = if cfg!(target_os = "macos") { self.modifiers.super_key() } else { self.modifiers.control_key() };
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
            WKey::Named(NamedKey::Escape) => Some(Key::Escape),
            WKey::Named(NamedKey::Tab) => Some(Key::Tab),
            WKey::Character(s) if cmd => s.chars().next().map(|c| Key::Char(c.to_ascii_lowercase())),
            _ => None,
        };
        if let Some(key) = key {
            state.core.input.keys.push(KeyPress { key, shift: self.modifiers.shift_key(), cmd, alt: self.modifiers.alt_key(), ctrl: self.modifiers.control_key() });
        }
        if !cmd
            && let Some(text) = &event.text {
                state.core.input.text.push_str(text);
            }
        state.window.request_redraw();
    }
}

impl ApplicationHandler<UserEvent> for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        match self.create_window(event_loop) {
            Ok(state) => self.state = Some(state),
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wake => {
                if let Some(state) = &self.state {
                    // While blocked, the retry timer is already on its way and drains then. That keeps a hidden
                    // window from spinning at the token rate.
                    if !state.present_blocked {
                        state.window.request_redraw();
                    }
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if std::env::var_os("FLUENT_TRACE").is_some() && !matches!(event, WindowEvent::CursorMoved { .. }) {
            eprintln!("[{:.3}] {event:?}", self.created.elapsed().as_secs_f32());
        }
        if let WindowEvent::RedrawRequested = event {
            self.redraw();
            return;
        }
        if let WindowEvent::KeyboardInput { event, .. } = event {
            self.on_key(event);
            return;
        }
        let Some(state) = self.state.as_mut() else { return };
        let input = &mut state.core.input;
        let scale = state.core.scale;
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                state.config.width = size.width.max(1);
                state.config.height = size.height.max(1);
                state.surface.configure(&state.core.renderer.gpu.device, &state.config);
                state.core.size_px = (state.config.width, state.config.height);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => state.core.scale = scale_factor as f32,
            WindowEvent::ThemeChanged(theme) => state.core.app.system_dark = theme == winit::window::Theme::Dark,
            WindowEvent::Occluded(false) | WindowEvent::Focused(true) => state.present_blocked = false,
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => input.pointer = Some((position.x as f32 / scale, position.y as f32 / scale)),
            WindowEvent::CursorLeft { .. } => input.pointer = None,
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
            WindowEvent::MouseWheel { delta, .. } => match delta {
                MouseScrollDelta::LineDelta(_, y) => {
                    input.scroll += y * 48.0;
                    input.scroll_is_pixels = false;
                }
                MouseScrollDelta::PixelDelta(p) => {
                    input.scroll += p.y as f32 / scale;
                    input.scroll_is_pixels = true;
                }
            },
            WindowEvent::Ime(ime) => match ime {
                Ime::Commit(text) => {
                    input.ime_preedit = None;
                    input.ime_commit = Some(text);
                }
                Ime::Preedit(text, _) => input.ime_preedit = if text.is_empty() { Some(String::new()) } else { Some(text) },
                Ime::Disabled => input.ime_preedit = None,
                Ime::Enabled => {}
            },
            WindowEvent::DroppedFile(path) => {
                input.hovered_file = false;
                input.dropped.push(path);
            }
            WindowEvent::HoveredFile(_) => input.hovered_file = true,
            WindowEvent::HoveredFileCancelled => input.hovered_file = false,
            _ => return,
        }
        state.window.request_redraw();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(when) = self.exit_after
            && Instant::now() >= when {
                event_loop.exit();
                return;
            }
        let mut wake = self.next_wake;
        if let Some(exit_at) = self.exit_after {
            wake = Some(wake.map_or(exit_at, |w| w.min(exit_at)));
        }
        match wake {
            Some(when) if when <= Instant::now() => {
                self.next_wake = None;
                if let Some(state) = &mut self.state {
                    if state.present_blocked {
                        // AppKit may send no redraw at all to a hidden window, so try for a drawable directly.
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

/// Shows the window even over another app's full-screen space, without activating this app.
#[cfg(target_os = "macos")]
fn float_over_everything(window: &Window) {
    use objc2::runtime::AnyObject;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return };
    const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
    const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
    let view: *mut AnyObject = handle.ns_view.as_ptr().cast();
    // SAFETY: the view comes from a live winit window, and we are on the main thread that owns it. These are
    // documented NSView and NSWindow selectors, with argument types that match their declarations.
    unsafe {
        let ns_window: *mut AnyObject = objc2::msg_send![view, window];
        if ns_window.is_null() {
            return;
        }
        let _: () = objc2::msg_send![ns_window, setCollectionBehavior: CAN_JOIN_ALL_SPACES | FULL_SCREEN_AUXILIARY];
        let _: () = objc2::msg_send![ns_window, orderFrontRegardless];
        let number: isize = objc2::msg_send![ns_window, windowNumber];
        if std::env::var_os("FLUENT_TRACE").is_some() {
            eprintln!("window-number {number}");
        }
    }
}

/// How often a window that cannot present checks for new data and tries again.
const BLOCKED_RETRY: Duration = Duration::from_millis(100);

fn run_windowed(args: Args) -> Result<(), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| e.to_string())?;
    let proxy = event_loop.create_proxy();
    let runtime = tokio_runtime()?;
    let exit_after = args.value("--exit-after").and_then(|v| v.parse::<f32>().ok()).map(|s| Instant::now() + Duration::from_secs_f32(s));
    let frame_log = args.value("--frame-log").map(PathBuf::from);
    let mut shell = Shell {
        args,
        proxy,
        runtime,
        state: None,
        modifiers: ModifiersState::empty(),
        clipboard: SystemClipboard(arboard::Clipboard::new().ok()),
        next_wake: None,
        last_click: None,
        exit_after,
        error: None,
        _mock: None,
        frames: 0,
        frame_log,
        created: Instant::now(),
    };
    event_loop.run_app(&mut shell).map_err(|e| e.to_string())?;
    if let (Some(path), Some(state)) = (&shell.frame_log, &shell.state) {
        let t = &state.core.timings;
        let mut total: Vec<u32> = t.iter().map(|f| f.drain_us + f.build_us + f.render_us).collect();
        total.sort_unstable();
        let report = serde_json::json!({
            "frames": shell.frames,
            "cpu_p50_ms": percentile(&total, 0.5) as f64 / 1000.0,
            "cpu_p99_ms": percentile(&total, 0.99) as f64 / 1000.0,
            "cpu_max_ms": percentile(&total, 1.0) as f64 / 1000.0,
            "tokens": state.core.app.swarm.total_tokens,
            "retries": state.core.app.swarm.retries,
            "resumes": state.core.app.swarm.resumes,
            "failed": state.core.app.swarm.finished_failed,
            "ok": state.core.app.swarm.finished_ok,
            "rss_mib": rss_mib(),
        });
        std::fs::write(path, report.to_string()).map_err(|e| e.to_string())?;
    }
    match shell.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn main() {
    let args = Args(std::env::args().skip(1).collect());
    if args.flag("--help") || args.flag("-h") {
        println!("{USAGE}");
        return;
    }
    let result = if args.flag("--headless") { run_headless(&args) } else { run_windowed(args) };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
