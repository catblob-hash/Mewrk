//! The window `ErrorView` lies in (`loadError` in `App.tsx`), and the reset behind it.
//!
//! A failed load leaves this process holding the data folder's lease, with a host that cannot be started twice. So a reset is a
//! fresh process: this one starts `--reset-document` and leaves, that one resets under its own lease and starts the app again.

use std::sync::Arc;
use std::time::Instant;

use fluent::gpu::SURFACE_FORMAT;
use fluent::ui::{Input, NoClipboard, Ui, UiMemory};
use fluent::{Gpu, Renderer, Scene, TextCtx, Theme};
use mewrk_client::{Host, HostConfig, StartError};
use mewrk_shell::dialogs::error_view::{error_view, FatalApp};
use serde_json::json;

pub const RESET_FLAG: &str = "--reset-document";
pub const RESET_FAILED_FLAG: &str = "--reset-failed";

/// A `Contended` lease is the one failure the host words with a hint under a blank line. Anything it misjudges stays safe: the
/// reset process reads the document again first and leaves it alone if it loads.
pub fn reset_can_help(message: &str) -> bool {
    !message.contains("\n\n")
}

/// What the next process is started with: this one's arguments, less the ones that said what this one was for.
fn relaunch_args(extra: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    let mut given = std::env::args().skip(1);
    while let Some(arg) = given.next() {
        if arg == RESET_FAILED_FLAG {
            given.next();
        } else if arg != RESET_FLAG {
            args.push(arg);
        }
    }
    args.extend_from_slice(extra);
    args
}

fn relaunch(extra: &[String]) -> ! {
    let result = std::env::current_exe().and_then(|exe| std::process::Command::new(exe).args(relaunch_args(extra)).spawn());
    match result {
        // The next process waits out this one's lease, and this one's is gone once it leaves.
        Ok(_) => std::process::exit(0),
        Err(error) => {
            eprintln!("could not start Mewrk again: {error}");
            std::process::exit(1);
        }
    }
}

pub fn begin_reset() -> ! {
    relaunch(&[RESET_FLAG.to_owned()])
}

fn reset_document(config: HostConfig) -> Result<(), String> {
    let host = match Host::start(config, Arc::new(|_, _| {})) {
        Ok(host) => host,
        Err(StartError::AnotherInstanceAnswered) => return Err(mewrk_client::t("另一个 Mewrk 正在使用这些数据", "Another Mewrk is using this data").to_owned()),
        Err(StartError::Unavailable(message)) => return Err(message),
    };
    // Between the question and now, the document may have become readable again: another Mewrk may have repaired or replaced
    // it. A reset deletes everything, so it runs only against a document that really cannot be read.
    if host.invoke_blocking("load_document", json!({})).is_ok() {
        host.finalize();
        return Ok(());
    }
    let result = host.invoke_blocking("reset_document", json!({})).map(|_| ());
    host.finalize();
    result
}

/// `--reset-document`: resets, then starts the app, which shows the window again.
pub fn run_reset(config: HostConfig) -> ! {
    match reset_document(config) {
        Ok(()) => relaunch(&[]),
        Err(error) => {
            eprintln!("reset failed: {error}");
            relaunch(&[RESET_FAILED_FLAG.to_owned(), error])
        }
    }
}

/// One frame's worth of the error view. The window driver owns the surface and the input.
pub struct FatalCore {
    pub app: FatalApp,
    text: TextCtx,
    scene: Scene,
    mem: UiMemory,
    pub input: Input,
    renderer: Renderer,
    started: Instant,
    pub scale: f32,
    pub size_px: (u32, u32),
    pub dark: bool,
}

impl FatalCore {
    pub fn new(app: FatalApp, gpu: Arc<Gpu>, scale: f32, size_px: (u32, u32), dark: bool) -> Self {
        Self { app, text: TextCtx::new(scale), scene: Scene::default(), mem: UiMemory::default(), input: Input::default(), renderer: Renderer::new(gpu), started: Instant::now(), scale, size_px, dark }
    }

    pub fn gpu(&self) -> Arc<Gpu> {
        self.renderer.gpu.clone()
    }

    /// Returns whether another frame is wanted right away, and the submission to wait on.
    pub fn frame(&mut self, target: &wgpu::TextureView) -> (bool, wgpu::SubmissionIndex) {
        let theme = if self.dark { Theme::dark() } else { Theme::light() };
        let size = (self.size_px.0 as f32 / self.scale, self.size_px.1 as f32 / self.scale);
        self.text.set_scale(self.scale);
        self.scene.clear();
        let time = self.started.elapsed().as_secs_f32();
        let mut clipboard = NoClipboard;
        let mut ui = Ui::new(&mut self.scene, &mut self.text, theme, &self.input, &mut self.mem, &mut clipboard, self.scale, size, 0.016, time);
        fluent::el::set_theme(theme);
        fluent::el::set_window_size(size.0, size.1);
        let view = error_view(&mut self.app);
        let again = fluent::el::render(view, &mut self.app, &mut ui);
        let animating = ui.animating || again;
        drop(ui);
        self.input.end_frame();
        self.text.flush_atlases(&self.renderer);
        let submission = self.renderer.render(&self.scene, &theme, target, self.size_px);
        (animating, submission)
    }
}

pub const FORMAT: wgpu::TextureFormat = SURFACE_FORMAT;
