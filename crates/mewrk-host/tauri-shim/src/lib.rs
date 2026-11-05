//! The part of Tauri's API the Mewrk host is written against.
//!
//! The host stays byte-for-byte what upstream ships wherever it can: its commands are ordinary functions that take
//! an `AppHandle` and `State`, so a native client can call them directly and receive their channels as closures.
//! Windows and menus are the native client's job. The built-in browser's pages are Chromium (`cef_host` in the host);
//! `webview` is the window they go in, and, without the host's `chromium` feature, a `WKWebView` page in their place.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

pub use mewrk_tauri_shim_macros::command;

pub mod async_runtime;
pub mod ipc;
pub mod webview;
pub use webview::Webview;

/// The runtime marker Tauri's generic types carry. There is only one here.
#[derive(Clone, Copy, Debug, Default)]
pub struct Wry;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    Message(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Json(e) => write!(f, "{e}"),
            Error::Message(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// What the application needs to know about itself: where its data goes and how to reach the main thread.
pub struct AppConfig {
    pub identifier: String,
    pub version: String,
    pub data_dir: PathBuf,
    pub local_data_dir: PathBuf,
    pub resource_dir: Option<PathBuf>,
}

type MainThreadRunner = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;
type ExitHook = Arc<dyn Fn(i32) + Send + Sync>;

struct Inner {
    config: AppConfig,
    states: RwLock<HashMap<TypeId, &'static (dyn Any + Send + Sync)>>,
    main_thread: RwLock<Option<MainThreadRunner>>,
    exit: RwLock<Option<ExitHook>>,
}

pub struct AppHandle<R = Wry> {
    inner: Arc<Inner>,
    runtime: PhantomData<fn() -> R>,
}

impl<R> Clone for AppHandle<R> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), runtime: PhantomData }
    }
}

impl AppHandle<Wry> {
    pub fn new(config: AppConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                states: RwLock::new(HashMap::new()),
                main_thread: RwLock::new(None),
                exit: RwLock::new(None),
            }),
            runtime: PhantomData,
        }
    }

    /// Hands the client's way of running a closure on its UI thread to the host.
    pub fn set_main_thread_runner(&self, runner: impl Fn(Box<dyn FnOnce() + Send>) + Send + Sync + 'static) {
        *self.inner.main_thread.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(runner));
    }

    pub fn set_exit_hook(&self, hook: impl Fn(i32) + Send + Sync + 'static) {
        *self.inner.exit.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(hook));
    }

    pub fn run_on_main_thread(&self, f: impl FnOnce() + Send + 'static) -> Result<()> {
        let runner = self.inner.main_thread.read().unwrap_or_else(|p| p.into_inner()).clone();
        match runner {
            Some(run) => run(Box::new(f)),
            None => f(),
        }
        Ok(())
    }

    pub fn exit(&self, code: i32) {
        let hook = self.inner.exit.read().unwrap_or_else(|p| p.into_inner()).clone();
        match hook {
            Some(hook) => hook(code),
            None => std::process::exit(code),
        }
    }

    pub fn package_info(&self) -> PackageInfo {
        PackageInfo { version: self.inner.config.version.clone(), name: "Mewrk".to_owned() }
    }

    pub fn app_identifier(&self) -> &str {
        &self.inner.config.identifier
    }
}

pub struct PackageInfo {
    pub version: String,
    pub name: String,
}

pub struct PathResolver {
    inner: Arc<Inner>,
}

impl PathResolver {
    pub fn app_data_dir(&self) -> Result<PathBuf> {
        Ok(self.inner.config.data_dir.clone())
    }

    pub fn app_local_data_dir(&self) -> Result<PathBuf> {
        Ok(self.inner.config.local_data_dir.clone())
    }

    pub fn resource_dir(&self) -> Result<PathBuf> {
        self.inner
            .config
            .resource_dir
            .clone()
            .ok_or_else(|| Error::Message("no resource directory".to_owned()))
    }

    pub fn app_config_dir(&self) -> Result<PathBuf> {
        Ok(self.inner.config.data_dir.clone())
    }

    pub fn home_dir(&self) -> Result<PathBuf> {
        dirs::home_dir().ok_or_else(|| Error::Message("no home directory".to_owned()))
    }
}

/// A managed value. They live as long as the process, so a reference to one is simply `'static`.
pub struct State<'r, T: Send + Sync + 'static>(&'r T);

impl<'r, T: Send + Sync + 'static> State<'r, T> {
    pub fn inner(&self) -> &'r T {
        self.0
    }
}

impl<T: Send + Sync + 'static> std::ops::Deref for State<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.0
    }
}

impl<T: Send + Sync + 'static> Clone for State<'_, T> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl<T: Send + Sync + 'static> Copy for State<'_, T> {}

pub trait Manager<R = Wry> {
    #[doc(hidden)]
    fn handle_inner(&self) -> &AppHandle<Wry>;

    fn state<T: Send + Sync + 'static>(&self) -> State<'_, T> {
        self.try_state::<T>().unwrap_or_else(|| panic!("state not managed: {}", std::any::type_name::<T>()))
    }

    fn try_state<T: Send + Sync + 'static>(&self) -> Option<State<'_, T>> {
        let states = self.handle_inner().inner.states.read().unwrap_or_else(|p| p.into_inner());
        states.get(&TypeId::of::<T>()).and_then(|any| any.downcast_ref::<T>()).map(State)
    }

    /// Returns false when a value of this type is already managed, which is left as it was.
    fn manage<T: Send + Sync + 'static>(&self, value: T) -> bool {
        let mut states = self.handle_inner().inner.states.write().unwrap_or_else(|p| p.into_inner());
        if states.contains_key(&TypeId::of::<T>()) {
            return false;
        }
        let leaked: &'static T = Box::leak(Box::new(value));
        states.insert(TypeId::of::<T>(), leaked);
        true
    }

    fn path(&self) -> PathResolver {
        PathResolver { inner: self.handle_inner().inner.clone() }
    }

    fn get_webview_window(&self, _label: &str) -> Option<WebviewWindow> {
        None
    }

    fn get_webview(&self, label: &str) -> Option<webview::Webview> {
        webview::get_webview(self.handle_inner(), label)
    }

    fn get_window(&self, label: &str) -> Option<Window> {
        webview::get_window(self.handle_inner(), label)
    }
}

impl Manager<Wry> for AppHandle<Wry> {
    fn handle_inner(&self) -> &AppHandle<Wry> {
        self
    }
}

// ---------------------------------------------------------------- what a page engine would plug into

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LogicalPosition<T> {
    pub x: T,
    pub y: T,
}

impl<T> LogicalPosition<T> {
    pub fn new(x: T, y: T) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LogicalSize<T> {
    pub width: T,
    pub height: T,
}

impl<T> LogicalSize<T> {
    pub fn new(width: T, height: T) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PhysicalPosition<T> {
    pub x: T,
    pub y: T,
}

impl<T> PhysicalPosition<T> {
    pub fn new(x: T, y: T) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PhysicalSize<T> {
    pub width: T,
    pub height: T,
}

impl<T> PhysicalSize<T> {
    pub fn new(width: T, height: T) -> Self {
        Self { width, height }
    }
}

impl PhysicalSize<u32> {
    pub fn to_logical<X: From<f64>>(&self, scale: f64) -> LogicalSize<X> {
        LogicalSize { width: X::from(self.width as f64 / scale), height: X::from(self.height as f64 / scale) }
    }
}

impl LogicalPosition<f64> {
    /// This position in physical pixels, at `scale` physical pixels to one logical pixel.
    pub fn to_physical(self, scale: f64) -> PhysicalPosition<i32> {
        PhysicalPosition { x: (self.x * scale).round() as i32, y: (self.y * scale).round() as i32 }
    }
}

impl LogicalSize<f64> {
    /// This size in physical pixels, at `scale` physical pixels to one logical pixel.
    pub fn to_physical(self, scale: f64) -> PhysicalSize<u32> {
        PhysicalSize { width: (self.width * scale).round().max(0.0) as u32, height: (self.height * scale).round().max(0.0) as u32 }
    }
}

impl Position {
    /// This position in logical pixels, at `scale` physical pixels to one logical pixel.
    pub fn to_logical_at(self, scale: f64) -> LogicalPosition<f64> {
        match self {
            Position::Logical(position) => position,
            Position::Physical(position) => LogicalPosition { x: position.x as f64 / scale, y: position.y as f64 / scale },
        }
    }
}

impl Size {
    /// This size in logical pixels, at `scale` physical pixels to one logical pixel.
    pub fn to_logical_at(self, scale: f64) -> LogicalSize<f64> {
        match self {
            Size::Logical(size) => size,
            Size::Physical(size) => LogicalSize { width: size.width as f64 / scale, height: size.height as f64 / scale },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Position {
    Physical(PhysicalPosition<i32>),
    Logical(LogicalPosition<f64>),
}

impl From<LogicalPosition<f64>> for Position {
    fn from(p: LogicalPosition<f64>) -> Self {
        Position::Logical(p)
    }
}

impl From<PhysicalPosition<i32>> for Position {
    fn from(p: PhysicalPosition<i32>) -> Self {
        Position::Physical(p)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Size {
    Physical(PhysicalSize<u32>),
    Logical(LogicalSize<f64>),
}

impl From<LogicalSize<f64>> for Size {
    fn from(s: LogicalSize<f64>) -> Self {
        Size::Logical(s)
    }
}

impl From<PhysicalSize<u32>> for Size {
    fn from(s: PhysicalSize<u32>) -> Self {
        Size::Physical(s)
    }
}

#[derive(Clone, Debug)]
pub enum WebviewUrl {
    External(url::Url),
    App(PathBuf),
    CustomProtocol(url::Url),
}

impl Default for WebviewUrl {
    fn default() -> Self {
        WebviewUrl::App(PathBuf::new())
    }
}

#[derive(Debug)]
pub enum WindowEvent {
    Resized(PhysicalSize<u32>),
    Moved(PhysicalPosition<i32>),
    Destroyed,
    Focused(bool),
    ScaleFactorChanged { scale_factor: f64, new_inner_size: PhysicalSize<u32> },
}

fn no_webview<T>() -> Result<T> {
    Err(Error::Message("no page engine is attached".to_owned()))
}

pub use webview::Window;

pub mod window {
    use super::*;

    pub struct WindowBuilder<R = Wry>(PhantomData<fn() -> R>);

    impl WindowBuilder<Wry> {
        pub fn new(_app: &AppHandle<Wry>, _label: impl Into<String>) -> Self {
            Self(PhantomData)
        }
    }

    macro_rules! chain {
        ($($name:ident($($arg:ty),*)),* $(,)?) => {
            impl WindowBuilder<Wry> {
                $(pub fn $name(self, $(_: $arg),*) -> Self { self })*

                pub fn build(self) -> Result<Window> {
                    no_webview()
                }
            }
        };
    }

    chain!(visible(bool), decorations(bool), skip_taskbar(bool), inner_size(f64, f64), position(f64, f64), title(&str), focused(bool), resizable(bool), shadow(bool), transparent(bool), always_on_top(bool), min_inner_size(f64, f64));
}

/// The application's main view, which only exists as far as the host needs a parent for a dialog.
#[derive(Clone)]
pub struct WebviewWindow;

impl WebviewWindow {
    pub fn show(&self) -> Result<()> {
        Ok(())
    }

    pub fn hide(&self) -> Result<()> {
        Ok(())
    }

    pub fn set_focus(&self) -> Result<()> {
        Ok(())
    }

    pub fn unminimize(&self) -> Result<()> {
        Ok(())
    }

    pub fn label(&self) -> &str {
        "main"
    }

    pub fn is_visible(&self) -> Result<bool> {
        Ok(true)
    }

    pub fn as_ref(&self) -> &Self {
        self
    }
}

/// Keeps a process-wide value that the shim's other modules may need.
#[doc(hidden)]
pub static PROCESS_NAME: OnceLock<String> = OnceLock::new();
