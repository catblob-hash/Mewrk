//! Where no page engine exists: every call reports that.

use std::marker::PhantomData;

use crate::{AppHandle, Error, LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, Position, Result, Size, WebviewUrl, Wry};

pub enum NewWindowResponse {
    Allow,
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageLoadEvent {
    Started,
    Finished,
}

pub struct PageLoadPayload;

impl PageLoadPayload {
    pub fn event(&self) -> PageLoadEvent {
        PageLoadEvent::Finished
    }

    pub fn url(&self) -> &url::Url {
        panic!("no page engine is attached")
    }
}

fn no_webview<T>() -> Result<T> {
    Err(Error::Message("no page engine is attached".to_owned()))
}

#[derive(Clone)]
pub struct Webview;

impl Webview {
    pub fn label(&self) -> &str {
        ""
    }

    pub fn app_handle(&self) -> &AppHandle<Wry> {
        panic!("no page engine is attached")
    }

    pub fn close(&self) -> Result<()> {
        Ok(())
    }

    pub fn show(&self) -> Result<()> {
        no_webview()
    }

    pub fn hide(&self) -> Result<()> {
        no_webview()
    }

    pub fn set_focus(&self) -> Result<()> {
        no_webview()
    }

    pub fn set_position(&self, _position: impl Into<Position>) -> Result<()> {
        no_webview()
    }

    pub fn set_size(&self, _size: impl Into<Size>) -> Result<()> {
        no_webview()
    }

    pub fn position(&self) -> Result<PhysicalPosition<i32>> {
        no_webview()
    }

    pub fn size(&self) -> Result<PhysicalSize<u32>> {
        no_webview()
    }

    pub fn url(&self) -> Result<url::Url> {
        no_webview()
    }

    pub fn navigate(&self, _url: url::Url) -> Result<()> {
        no_webview()
    }

    pub fn eval(&self, _script: impl Into<String>) -> Result<()> {
        no_webview()
    }

    pub fn run_on_main_thread(&self, _f: impl FnOnce() + Send + 'static) -> Result<()> {
        no_webview()
    }

    pub fn eval_with_callback(&self, _script: impl Into<String>, _callback: impl Fn(String) + Send + 'static) -> Result<()> {
        no_webview()
    }

    pub fn reload(&self) -> Result<()> {
        no_webview()
    }

    pub fn window(&self) -> Window {
        Window
    }

    pub fn with_webview(&self, _f: impl FnOnce(PlatformWebview) + Send + 'static) -> Result<()> {
        no_webview()
    }

    pub fn is_devtools_open(&self) -> bool {
        false
    }

    pub fn open_devtools(&self) {}

    pub fn close_devtools(&self) {}

    pub fn set_zoom(&self, _factor: f64) -> Result<()> {
        no_webview()
    }

    pub fn clear_all_browsing_data(&self) -> Result<()> {
        no_webview()
    }
}

pub struct PlatformWebview;

pub struct WebviewBuilder<R = Wry>(PhantomData<fn() -> R>);

impl WebviewBuilder<Wry> {
    pub fn new(_label: impl Into<String>, _url: WebviewUrl) -> Self {
        Self(PhantomData)
    }

    pub fn on_navigation(self, _f: impl Fn(&url::Url) -> bool + Send + 'static) -> Self {
        self
    }

    pub fn on_new_window(self, _f: impl Fn(url::Url, NewWindowFeatures) -> NewWindowResponse + Send + Sync + 'static) -> Self {
        self
    }

    pub fn on_page_load(self, _f: impl Fn(Webview, PageLoadPayload) + Send + Sync + 'static) -> Self {
        self
    }

    pub fn on_document_title_changed(self, _f: impl Fn(Webview, String) + Send + 'static) -> Self {
        self
    }

    pub fn on_download(self, _f: impl Fn(Webview, DownloadEvent) -> bool + Send + Sync + 'static) -> Self {
        self
    }

    pub fn general_autofill_enabled(self, _on: bool) -> Self {
        self
    }

    pub fn initialization_script(self, _script: impl Into<String>) -> Self {
        self
    }

    pub fn data_directory(self, _path: std::path::PathBuf) -> Self {
        self
    }

    pub fn additional_browser_args(self, _args: &str) -> Self {
        self
    }

    pub fn user_agent(self, _agent: &str) -> Self {
        self
    }

    pub fn disable_drag_drop_handler(self) -> Self {
        self
    }

    pub fn transparent(self, _on: bool) -> Self {
        self
    }

    pub fn devtools(self, _on: bool) -> Self {
        self
    }

    pub fn incognito(self, _on: bool) -> Self {
        self
    }

    pub fn zoom_hotkeys_enabled(self, _on: bool) -> Self {
        self
    }

    pub fn auto_resize(self) -> Self {
        self
    }
}

pub struct NewWindowFeatures;

pub struct DownloadEvent;

#[allow(unused)]
fn _unused(_: LogicalPosition<f64>, _: LogicalSize<f64>) {}

/// The native window: there is none here.
#[derive(Clone)]
pub struct Window;

impl Window {
    pub fn label(&self) -> &str {
        "main"
    }

    pub fn scale_factor(&self) -> Result<f64> {
        no_webview()
    }

    pub fn inner_size(&self) -> Result<PhysicalSize<u32>> {
        no_webview()
    }

    pub fn outer_position(&self) -> Result<PhysicalPosition<i32>> {
        no_webview()
    }

    pub fn inner_position(&self) -> Result<PhysicalPosition<i32>> {
        no_webview()
    }

    pub fn is_visible(&self) -> Result<bool> {
        no_webview()
    }

    pub fn is_minimized(&self) -> Result<bool> {
        no_webview()
    }

    pub fn hwnd(&self) -> Result<usize> {
        no_webview()
    }

    pub fn on_window_event(&self, _f: impl Fn(&crate::WindowEvent) + Send + 'static) {}

    pub fn add_child(&self, _builder: WebviewBuilder<Wry>, _position: impl Into<Position>, _size: impl Into<Size>) -> Result<Webview> {
        no_webview()
    }

    pub fn destroy(&self) -> Result<()> {
        Ok(())
    }

    pub fn set_title(&self, _title: &str) -> Result<()> {
        Ok(())
    }

    pub fn set_position(&self, _position: impl Into<Position>) -> Result<()> {
        Ok(())
    }

    pub fn close(&self) -> Result<()> {
        Ok(())
    }

    pub fn show(&self) -> Result<()> {
        Ok(())
    }

    pub fn hide(&self) -> Result<()> {
        Ok(())
    }

    pub fn set_focus(&self) -> Result<()> {
        Ok(())
    }

    pub fn app_handle(&self) -> &'static AppHandle<Wry> {
        panic!("no page engine is attached")
    }
}

pub(crate) fn get_webview(_app: &AppHandle<Wry>, _label: &str) -> Option<Webview> {
    None
}

pub(crate) fn get_window(_app: &AppHandle<Wry>, _label: &str) -> Option<Window> {
    None
}

/// Where a page's view goes. Nothing can use it here.
pub fn attach_main_view(_view: *mut std::ffi::c_void) {}
