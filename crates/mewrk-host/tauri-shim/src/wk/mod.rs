//! One built-in-browser page on macOS: a `WKWebView` whose view is a child of the client's window.
//!
//! `browser.rs` drives pages through a small slice of Tauri's `Webview` API plus the DevTools
//! protocol, and its `cfg(feature = "cef")` paths are written against upstream's CEF handle, which
//! offers that slice under the same names. [`Webview`] offers it too, so the page lifecycle, policy
//! and tool code stays one implementation. What Chromium answers over the protocol is answered by
//! the adapter in [`cdp`]: a real WebKit API where one exists, injected script (`hooks.js`) where
//! not, and a readable error where neither can do it.
//!
//! Threading: WebKit runs every delegate, and accepts every call, on the main thread. Its objects
//! never leave it: they live in the thread-local [`NATIVE`] registry keyed by page label, and
//! everything else reaches them by posting to the main thread through libdispatch. Dispatch rather
//! than the client's mailbox, because the mailbox only turns between frames, and a page has to keep
//! answering during a window resize's modal tracking. The only state shared across threads is
//! [`PageShared`], which holds no Objective-C object.

mod cdp;
mod delegate;
mod snapshot;

pub use cdp::{Support, DEVTOOLS_METHODS};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView};
use objc2_foundation::{ns_string, NSArray, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest};
use objc2_web_kit::{
    WKContentWorld, WKInactiveSchedulingPolicy, WKUserScript, WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration,
    WKWebsiteDataStore,
};

use crate::{AppHandle, PhysicalPosition, PhysicalSize, Position, Size, WebviewUrl, Wry};

/// Every page's first document, as upstream's: real destinations load after it, so no page runs a
/// byte of a remote document before Mewrk's bootstrap has been registered.
const BLANK_URL: &str = "about:blank";
const MAIN_THREAD_TIMEOUT: Duration = Duration::from_secs(15);
/// `https://<mapped host>/x` is served as `mewrk-preview://<mapped host>/x`, the only way to serve a
/// folder at all: WebKit refuses a scheme handler for `https`.
const PREVIEW_SCHEME: &str = "mewrk-preview";
/// Where a parked page goes. A native child view paints over the GPU surface it is a child of, so
/// there is no sibling to sink beneath; out of the window's bounds it shows nothing and takes no
/// pointer input, while WebKit still treats it as visible and keeps it running at full speed.
const PARKED_X: f64 = -65_536.0;
/// What a loopback address is called while the page's connections go through a proxy.
///
/// WebKit never sends a loopback address through a proxy, whatever the configuration says (measured:
/// `localhost`, `localhost.`, `127.x`, `[::1]` and `0.0.0.0`, with match domains too), and a page of a
/// workspace on another machine means that machine's `localhost`. A name under `.localhost` does go
/// through, dev servers accept it as a Host, and the host's tunnel resolves it to the machine's
/// loopback (`preview_tunnel::machine_host`). The host and the person only ever see `localhost`.
const LOOPBACK_ALIASES: [(&str, &str); 3] = [("localhost", "machine.localhost"), ("127.0.0.1", "ipv4.machine.localhost"), ("[::1]", "ipv6.machine.localhost")];

// ----- the main thread ---------------------------------------------------------------------------

unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_after_f(when: u64, queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_time(when: u64, delta: i64) -> u64;
}

extern "C" fn run_boxed(context: *mut c_void) {
    // SAFETY: `context` is the box `boxed` leaked, and libdispatch runs each work item once.
    let job = unsafe { Box::from_raw(context as *mut Box<dyn FnOnce() + Send>) };
    job();
}

fn boxed(f: impl FnOnce() + Send + 'static) -> *mut c_void {
    let job: Box<dyn FnOnce() + Send> = Box::new(f);
    Box::into_raw(Box::new(job)) as *mut c_void
}

pub fn is_main_thread() -> bool {
    MainThreadMarker::new().is_some()
}

/// Queues `f` on the main thread, always later: a caller inside a WebKit callback stays re-entrant.
pub fn post_main(f: impl FnOnce() + Send + 'static) {
    // SAFETY: the main queue is a process-lifetime object, and `run_boxed` owns the context.
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, boxed(f), run_boxed) }
}

pub(crate) fn post_main_after(delay: Duration, f: impl FnOnce() + Send + 'static) {
    // SAFETY: as `post_main`. `DISPATCH_TIME_NOW` is 0.
    unsafe {
        let when = dispatch_time(0, delay.as_nanos() as i64);
        dispatch_after_f(when, &raw const _dispatch_main_q, boxed(f), run_boxed);
    }
}

/// Runs `f` on the main thread and waits: inline when already there, so a caller on it can't deadlock.
pub(crate) fn on_main<T: Send + 'static>(stage: &str, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    if is_main_thread() {
        return Ok(f());
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    post_main(move || {
        let _ = sender.try_send(f());
    });
    receiver.recv_timeout(MAIN_THREAD_TIMEOUT).map_err(|_| format!("timed out waiting for the main thread: {stage}"))
}

// ----- the window ---------------------------------------------------------------------------------

/// The content view pages are children of. Set by the client once its window exists.
static MAIN_VIEW: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Where pages go: the window's content view (`App::native_view`). Null means no window; pages can't
/// be created until there is one, and the host then reports that no page engine is attached.
pub fn attach_main_view(view: *mut c_void) {
    MAIN_VIEW.store(view, Ordering::Release);
}

fn main_view() -> Option<*mut NSView> {
    let view = MAIN_VIEW.load(Ordering::Acquire) as *mut NSView;
    (!view.is_null()).then_some(view)
}

/// The client's window. There is only the one, called what the host calls its main window.
#[derive(Clone)]
pub struct Window {
    app: AppHandle<Wry>,
    label: String,
}

impl Window {
    pub fn label(&self) -> &str {
        &self.label
    }

    fn read<T: Send + 'static>(&self, stage: &'static str, f: impl FnOnce(&NSView) -> Option<T> + Send + 'static) -> Result<T, String> {
        on_main(stage, move || {
            let view = main_view()?;
            // SAFETY: the content view lives as long as the window, and this runs on the main thread.
            f(unsafe { &*view })
        })?
        .ok_or_else(|| format!("{stage}: no window"))
    }

    /// The content view pages are children of, which is the client's window's.
    pub fn ns_view(&self) -> Result<*mut c_void, String> {
        Ok(MAIN_VIEW.load(Ordering::Acquire))
    }

    pub fn scale_factor(&self) -> Result<f64, String> {
        self.read("read the window's scale", |view| view.window().map(|window| window.backingScaleFactor()))
    }

    pub fn inner_size(&self) -> Result<PhysicalSize<u32>, String> {
        self.read("read the window's size", |view| {
            let scale = view.window().map_or(1.0, |window| window.backingScaleFactor());
            let size = view.bounds().size;
            Some(PhysicalSize::new((size.width * scale).round() as u32, (size.height * scale).round() as u32))
        })
    }

    pub fn outer_position(&self) -> Result<PhysicalPosition<i32>, String> {
        self.read("read the window's position", |view| {
            let window = view.window()?;
            let scale = window.backingScaleFactor();
            let frame = window.frame();
            Some(PhysicalPosition::new((frame.origin.x * scale).round() as i32, (frame.origin.y * scale).round() as i32))
        })
    }

    pub fn inner_position(&self) -> Result<PhysicalPosition<i32>, String> {
        self.outer_position()
    }

    pub fn is_visible(&self) -> Result<bool, String> {
        self.read("read whether the window is visible", |view| view.window().map(|window| window.isVisible()))
    }

    pub fn is_minimized(&self) -> Result<bool, String> {
        self.read("read whether the window is minimized", |view| view.window().map(|window| window.isMiniaturized()))
    }

    /// The client moves its window; the page follows as its child. Resizes reach the host as pane
    /// bounds (`browser_set_panel_bounds`), so nothing needs these events.
    pub fn on_window_event(&self, _f: impl Fn(&crate::WindowEvent) + Send + 'static) {}

    pub fn add_child(&self, builder: WebviewBuilder<Wry>, position: impl Into<Position>, size: impl Into<Size>) -> Result<Webview, String> {
        add_child(self, builder, position.into(), size.into())
    }

    /// The window outlives every page, and belongs to the client.
    pub fn destroy(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn set_title(&self, _title: &str) -> Result<(), String> {
        Ok(())
    }

    pub fn set_position(&self, _position: impl Into<Position>) -> Result<(), String> {
        Ok(())
    }

    pub fn close(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn show(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn hide(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn set_focus(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn app_handle(&self) -> &AppHandle<Wry> {
        &self.app
    }
}

// ----- the registries -----------------------------------------------------------------------------

/// Live pages by label. A label that resolves here is a page that still exists.
static PAGES: Mutex<Option<HashMap<String, Arc<PageShared>>>> = Mutex::new(None);

fn pages() -> MutexGuard<'static, Option<HashMap<String, Arc<PageShared>>>> {
    PAGES.lock().unwrap_or_else(|p| p.into_inner())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// The page with this label, if it is alive.
pub fn live_page(label: &str) -> Option<Webview> {
    pages().as_ref().and_then(|pages| pages.get(label).cloned()).map(|shared| Webview { shared })
}

pub(crate) fn get_webview(_app: &AppHandle<Wry>, label: &str) -> Option<Webview> {
    live_page(label)
}

/// Every live page's label.
pub fn live_page_labels() -> Vec<String> {
    pages().as_ref().map_or_else(Vec::new, |pages| pages.keys().cloned().collect())
}

/// Only the client's window exists: a detached browser window (the host's other label) has nothing
/// to be, and the host reports the page as unavailable until the client attached its view.
pub(crate) fn get_window(app: &AppHandle<Wry>, label: &str) -> Option<Window> {
    (label == "main" && main_view().is_some()).then(|| Window { app: app.clone(), label: label.to_owned() })
}

/// Main-thread half of a page: the WebKit objects themselves.
struct NativePage {
    web: Retained<WKWebView>,
    delegate: Retained<delegate::PageDelegate>,
    store: Retained<WKWebsiteDataStore>,
    /// Hooks, then the builder's scripts: what every document starts with.
    base_scripts: Vec<String>,
    held_dialogs: HashMap<u64, HeldDialog>,
    file_chooser: Option<RcBlock<dyn Fn(*mut NSArray<NSURL>)>>,
    scheme_tasks: HashMap<usize, Retained<ProtocolObject<dyn objc2_web_kit::WKURLSchemeTask>>>,
}

enum HeldDialog {
    Alert(RcBlock<dyn Fn()>),
    Confirm(RcBlock<dyn Fn(Bool)>),
    Prompt(RcBlock<dyn Fn(*mut NSString)>),
}

impl HeldDialog {
    fn answer(self, accept: bool, text: Option<String>) {
        match self {
            HeldDialog::Alert(done) => done.call(()),
            HeldDialog::Confirm(done) => done.call((Bool::new(accept),)),
            HeldDialog::Prompt(done) => match (accept, text) {
                (true, Some(text)) => {
                    let text = NSString::from_str(&text);
                    done.call((Retained::as_ptr(&text) as *mut NSString,));
                }
                (true, None) => {
                    let text = NSString::new();
                    done.call((Retained::as_ptr(&text) as *mut NSString,));
                }
                (false, _) => done.call((std::ptr::null_mut(),)),
            },
        }
    }
}

thread_local! {
    static NATIVE: RefCell<HashMap<String, NativePage>> = RefCell::new(HashMap::new());
    /// One data store per profile directory, kept while the directory exists. See `store_for`.
    static STORES: RefCell<HashMap<PathBuf, Retained<WKWebsiteDataStore>>> = RefCell::new(HashMap::new());
}

/// A clone of the page's view. Never hold the registry borrow across a WebKit call: closing a page
/// or answering a dialog can re-enter the delegates synchronously.
fn native_web(label: &str) -> Option<Retained<WKWebView>> {
    NATIVE.with(|native| native.borrow().get(label).map(|page| page.web.clone()))
}

fn native_store(label: &str) -> Option<Retained<WKWebsiteDataStore>> {
    NATIVE.with(|native| native.borrow().get(label).map(|page| page.store.clone()))
}

/// The tab's profile: a data store of its own, in memory.
///
/// Upstream keeps a tab's profile on disk only so that it survives a cold suspend, and deletes it
/// with the tab or at exit. An ephemeral store held here for as long as the host keeps the tab's
/// profile directory has exactly that life, leaves nothing on disk to sweep after a crash, and works
/// on every macOS; `WKWebsiteDataStore(forIdentifier:)` would need macOS 14 and a separate, fallible
/// removal step that a page still closing blocks.
fn store_for(directory: Option<&Path>, mtm: MainThreadMarker) -> Retained<WKWebsiteDataStore> {
    // SAFETY: plain constructors on the main thread.
    let fresh = || unsafe { WKWebsiteDataStore::nonPersistentDataStore(mtm) };
    let Some(directory) = directory else {
        return fresh();
    };
    STORES.with(|stores| {
        let mut stores = stores.borrow_mut();
        stores.retain(|profile, _| profile.exists() || profile == directory);
        stores.entry(directory.to_path_buf()).or_insert_with(fresh).clone()
    })
}

// ----- what the builder collects --------------------------------------------------------------------

pub enum NewWindowResponse {
    Allow,
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageLoadEvent {
    Started,
    Finished,
}

/// The payload of a main-frame load.
pub struct PageLoadPayload {
    url: url::Url,
    event: PageLoadEvent,
}

impl PageLoadPayload {
    pub fn url(&self) -> &url::Url {
        &self.url
    }

    pub fn event(&self) -> PageLoadEvent {
        self.event
    }
}

/// `browser.rs` never reads the features of a new window.
pub struct NewWindowFeatures;

/// Every download is refused before it starts, and `browser.rs` never reads the event.
pub struct DownloadEvent;

/// Exists for host paths compiled only on Windows, which take the platform's webview.
pub struct PlatformWebview;

type NavigationCallback = Box<dyn Fn(&url::Url) -> bool + Send>;
type NewWindowCallback = Box<dyn Fn(url::Url, NewWindowFeatures) -> NewWindowResponse + Send>;
type DownloadCallback = Box<dyn Fn(Webview, DownloadEvent) -> bool + Send>;
type PageLoadCallback = Box<dyn Fn(Webview, PageLoadPayload) + Send>;
type TitleCallback = Box<dyn Fn(Webview, String) + Send>;

/// A value only ever used on the main thread, where WebKit calls every delegate. The builder's
/// callbacks are `Send` but, as with Tauri, not required to be `Sync`.
struct MainOnly<T>(T);

// SAFETY: the only access is `get`, which refuses every thread but the main one, so no two threads
// can ever hold a reference at once; moving the value (Send) is all that crosses threads.
unsafe impl<T: Send> Sync for MainOnly<T> {}

impl<T> MainOnly<T> {
    fn get(&self) -> Option<&T> {
        is_main_thread().then_some(&self.0)
    }
}

#[derive(Default)]
struct PageCallbacks {
    navigation: Option<NavigationCallback>,
    new_window: Option<NewWindowCallback>,
    download: Option<DownloadCallback>,
    page_load: Option<PageLoadCallback>,
    title: Option<TitleCallback>,
}

// ----- dialogs ---------------------------------------------------------------------------------------

/// A script dialog the page opened.
pub struct PageDialog {
    /// `alert`, `confirm` or `prompt`. WebKit gives no public way to hold `beforeunload`.
    pub kind: &'static str,
    pub message: String,
    pub default_text: String,
    pub url: String,
}

/// What to do with a dialog as it opens.
pub enum DialogDecision {
    /// Keep it open until [`Webview::answer_dialog`] is called with this id. The page blocks in the
    /// dialog exactly as it would in a browser.
    Hold(u64),
    /// Answer immediately.
    Answer { accept: bool, text: Option<String> },
}

type DialogHandlerFn = Arc<dyn Fn(PageDialog) -> DialogDecision + Send + Sync>;
type CrashHandlerFn = Arc<dyn Fn(&'static str) + Send + Sync>;
type EventListener = Arc<dyn Fn(&str, &str) + Send + Sync>;
pub type CdpCompletion = Box<dyn FnOnce(Result<String, String>) + Send>;

// ----- shared page state ------------------------------------------------------------------------------

/// Logical-pixel frame of the page within its window, top-left origin.
#[derive(Clone, Copy, Debug)]
struct LogicalFrame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// What `Emulation.*` asked for. `Default` is no emulation at all.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Emulation {
    /// The CSS viewport to fit into the pane, scaled down when it doesn't fit.
    viewport: Option<(f64, f64)>,
    device_scale_factor: f64,
    mobile: bool,
    max_touch_points: u32,
    emit_touch_events: bool,
}

struct PageShared {
    label: String,
    app: AppHandle<Wry>,
    window: Window,
    callbacks: MainOnly<PageCallbacks>,
    frame: Mutex<LogicalFrame>,
    /// Asked by `set_stacking`: out of the window (see [`PARKED_X`]).
    parked: AtomicBool,
    bottom_corner_radius: Mutex<Option<f64>>,
    zoom: Mutex<f64>,
    emulation: Mutex<Emulation>,
    closed: AtomicBool,
    listeners: Mutex<Vec<EventListener>>,
    dialog_handler: Mutex<Option<DialogHandlerFn>>,
    crash_handler: Mutex<Option<CrashHandlerFn>>,
    /// The directory the host asked this page's profile to be named by.
    profile: Option<PathBuf>,
    /// `https://<host>/` served from a folder, for this page only.
    virtual_hosts: Mutex<HashMap<String, PathBuf>>,
    /// Requests the page reported as started and hasn't reported as done. A document that goes away
    /// takes them with it, and Chromium reports that as a failure of each.
    open_requests: Mutex<HashSet<String>>,
    next_document: AtomicU64,
    document_request: Mutex<Option<String>>,
    /// The method of the navigation the policy just saw; `didStart` has no way to ask.
    navigation_method: Mutex<String>,
    /// The address a failure page stands in for, so a failure page that fails doesn't repeat.
    showing_failure_for: Mutex<Option<String>>,
    /// `Overlay.setInspectMode`'s armed state, which outlives the document it was armed on.
    inspect_config: Mutex<Option<serde_json::Value>>,
    /// `Page.setInterceptFileChooserDialog`.
    intercept_file_chooser: AtomicBool,
    /// Whether connections go through a proxy, which is what loopback aliases are for.
    proxied: AtomicBool,
}

impl PageShared {
    fn handle(self: &Arc<Self>) -> Webview {
        Webview { shared: self.clone() }
    }

    fn scale_factor(&self) -> f64 {
        self.window.scale_factor().unwrap_or(1.0).max(f64::EPSILON)
    }

    fn dispatch_event(&self, method: &str, params: &str) {
        let listeners = lock(&self.listeners).clone();
        for listener in listeners {
            listener(method, params);
        }
    }

    fn event(&self, method: &str, params: serde_json::Value) {
        self.dispatch_event(method, &params.to_string());
    }

    fn page_load(self: &Arc<Self>, url: &str, event: PageLoadEvent) {
        let Ok(url) = url::Url::parse(&self.public_url(url)) else {
            return;
        };
        if let Some(handler) = self.callbacks.get().and_then(|callbacks| callbacks.page_load.as_ref()) {
            handler(self.handle(), PageLoadPayload { url, event });
        }
    }

    /// Consults the navigation policy. A page without one allows everything, as Tauri does.
    fn allows_navigation(&self, url: &str) -> bool {
        let Ok(url) = url::Url::parse(url) else {
            return false;
        };
        self.callbacks.get().and_then(|callbacks| callbacks.navigation.as_ref()).is_none_or(|handler| handler(&url))
    }

    /// Reports a window the page tried to open. The page is single-view: whatever the handler
    /// answers, no second view is ever created.
    fn report_new_window(&self, url: &str) {
        let Ok(url) = url::Url::parse(url) else {
            return;
        };
        if let Some(handler) = self.callbacks.get().and_then(|callbacks| callbacks.new_window.as_ref()) {
            let _ = handler(url, NewWindowFeatures);
        }
    }

    fn allows_download(self: &Arc<Self>) -> bool {
        self.callbacks.get().and_then(|callbacks| callbacks.download.as_ref()).is_some_and(|handler| handler(self.handle(), DownloadEvent))
    }

    fn title_changed(self: &Arc<Self>, title: String) {
        if let Some(handler) = self.callbacks.get().and_then(|callbacks| callbacks.title.as_ref()) {
            handler(self.handle(), title);
        }
    }

    fn crashed(&self) {
        let handler = lock(&self.crash_handler).clone();
        if let Some(handler) = handler {
            handler("the renderer process exited");
        }
    }

    fn open_dialog(&self, dialog: PageDialog, held: HeldDialog) {
        let handler = lock(&self.dialog_handler).clone();
        let decision = match handler {
            Some(handler) => handler(dialog),
            // Nobody asked to see it: answer as a dismissed dialog, never as a native one.
            None => DialogDecision::Answer { accept: false, text: None },
        };
        match decision {
            DialogDecision::Hold(id) => NATIVE.with(|native| {
                if let Some(page) = native.borrow_mut().get_mut(&self.label) {
                    page.held_dialogs.insert(id, held);
                }
            }),
            DialogDecision::Answer { accept, text } => held.answer(accept, text),
        }
    }

    /// The address the host knows a page by, for what WebKit calls it.
    fn public_url(&self, url: &str) -> String {
        public_url(url)
    }

    /// The address WebKit has to load for `url`.
    fn engine_url(&self, url: &url::Url) -> String {
        match url.host_str() {
            Some(host) if url.scheme() == "https" && lock(&self.virtual_hosts).contains_key(host) => {
                format!("{PREVIEW_SCHEME}://{}", &url.as_str()["https://".len()..])
            }
            _ if self.proxied.load(Ordering::SeqCst) => aliased_url(url),
            _ => url.to_string(),
        }
    }
}

/// The address the host knows a page by, for what WebKit calls it.
fn public_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix(PREVIEW_SCHEME).and_then(|rest| rest.strip_prefix("://")) {
        return format!("https://{rest}");
    }
    if url.contains("machine.localhost") {
        if let Ok(mut parsed) = url::Url::parse(url) {
            let host = parsed.host_str().unwrap_or_default().to_owned();
            if let Some((public, _)) = LOOPBACK_ALIASES.iter().find(|(_, alias)| *alias == host) {
                if parsed.set_host(Some(public)).is_ok() {
                    return parsed.to_string();
                }
            }
        }
    }
    url.to_owned()
}

/// `url` with a loopback host under its alias.
fn aliased_url(url: &url::Url) -> String {
    let alias = match url.host() {
        Some(url::Host::Domain(name)) if name.eq_ignore_ascii_case("localhost") => LOOPBACK_ALIASES[0].1,
        Some(url::Host::Ipv4(address)) if address.is_loopback() || address.is_unspecified() => LOOPBACK_ALIASES[1].1,
        Some(url::Host::Ipv6(address)) if address.is_loopback() || address.is_unspecified() => LOOPBACK_ALIASES[2].1,
        _ => return url.to_string(),
    };
    let mut aliased = url.clone();
    match aliased.set_host(Some(alias)) {
        Ok(()) => aliased.to_string(),
        Err(_) => url.to_string(),
    }
}

// ----- the page handle ----------------------------------------------------------------------------------

/// A live page, addressed the way `browser.rs` addresses a Tauri webview.
#[derive(Clone)]
pub struct Webview {
    shared: Arc<PageShared>,
}

impl Webview {
    pub fn label(&self) -> &str {
        &self.shared.label
    }

    pub fn app_handle(&self) -> &AppHandle<Wry> {
        &self.shared.app
    }

    pub fn window(&self) -> Window {
        self.shared.window.clone()
    }

    pub fn run_on_main_thread(&self, task: impl FnOnce() + Send + 'static) -> Result<(), String> {
        if is_main_thread() {
            task();
        } else {
            post_main(task);
        }
        Ok(())
    }

    /// Runs `operation` on the main thread with the page's view.
    fn with_web<T: Send + 'static>(
        &self,
        stage: &'static str,
        operation: impl FnOnce(&WKWebView, MainThreadMarker) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let label = self.shared.label.clone();
        on_main(stage, move || {
            let web = native_web(&label).ok_or_else(|| format!("{stage}: the page is closed"))?;
            let mtm = MainThreadMarker::new().ok_or_else(|| format!("{stage}: not on the main thread"))?;
            operation(&web, mtm)
        })?
    }

    pub fn hide(&self) -> Result<(), String> {
        self.with_web("hide the page", |web, _| {
            web.setHidden(true);
            Ok(())
        })
    }

    pub fn show(&self) -> Result<(), String> {
        self.with_web("show the page", |web, _| {
            web.setHidden(false);
            Ok(())
        })
    }

    pub fn set_focus(&self) -> Result<(), String> {
        self.with_web("focus the page", |web, _| {
            if let Some(window) = web.window() {
                window.makeFirstResponder(Some(web));
            }
            Ok(())
        })
    }

    pub fn set_zoom(&self, factor: f64) -> Result<(), String> {
        if !factor.is_finite() || factor <= 0.0 {
            return Err("invalid page zoom".into());
        }
        *lock(&self.shared.zoom) = factor;
        self.apply_frame()
    }

    pub fn navigate(&self, url: url::Url) -> Result<(), String> {
        let shared = self.shared.clone();
        self.with_web("navigate the page", move |web, _| load(web, &shared.engine_url(&url)))
    }

    pub fn reload(&self) -> Result<(), String> {
        self.with_web("reload the page", |web, _| {
            // SAFETY: on the main thread with a live view.
            let _ = unsafe { web.reload() };
            Ok(())
        })
    }

    pub fn set_position(&self, position: impl Into<Position>) -> Result<(), String> {
        let position = position.into().to_logical_at(self.shared.scale_factor());
        {
            let mut frame = lock(&self.shared.frame);
            frame.x = position.x;
            frame.y = position.y;
        }
        self.apply_frame()
    }

    pub fn set_size(&self, size: impl Into<Size>) -> Result<(), String> {
        let size = size.into().to_logical_at(self.shared.scale_factor());
        {
            let mut frame = lock(&self.shared.frame);
            frame.width = size.width;
            frame.height = size.height;
        }
        self.apply_frame()
    }

    fn apply_frame(&self) -> Result<(), String> {
        let shared = self.shared.clone();
        self.with_web("lay out the page", move |web, _| {
            place_view(web, &shared);
            Ok(())
        })
    }

    pub fn position(&self) -> Result<PhysicalPosition<i32>, String> {
        let frame = *lock(&self.shared.frame);
        let scale = self.shared.scale_factor();
        Ok(PhysicalPosition::new((frame.x * scale).round() as i32, (frame.y * scale).round() as i32))
    }

    pub fn size(&self) -> Result<PhysicalSize<u32>, String> {
        let frame = *lock(&self.shared.frame);
        let scale = self.shared.scale_factor();
        Ok(PhysicalSize::new((frame.width.max(0.0) * scale).round() as u32, (frame.height.max(0.0) * scale).round() as u32))
    }

    pub fn url(&self) -> Result<url::Url, String> {
        let shared = self.shared.clone();
        let url = self.with_web("read the page's address", move |web, _| Ok(shared.public_url(&current_url(web))))?;
        url::Url::parse(&url).map_err(|error| format!("the page's address is invalid: {error}"))
    }

    /// Evaluates `script` in the main frame and hands `callback` the JSON of its value, as WebView2's
    /// `ExecuteScript` does: `null` for `undefined`, a thrown exception, or a value that cannot be
    /// serialized. Safe to call on the main thread; the callback runs there.
    pub fn eval_with_callback(&self, script: impl Into<String>, callback: impl FnOnce(String) + Send + 'static) -> Result<(), String> {
        let params = serde_json::json!({ "expression": script.into(), "returnByValue": true, "awaitPromise": false }).to_string();
        self.send_devtools_message(
            "Runtime.evaluate",
            &params,
            Box::new(move |result| {
                let value = result
                    .ok()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                    .filter(|response| response.get("exceptionDetails").is_none())
                    .and_then(|response| response.pointer("/result/value").cloned())
                    .unwrap_or(serde_json::Value::Null);
                callback(value.to_string());
            }),
        );
        Ok(())
    }

    pub fn eval(&self, script: impl Into<String>) -> Result<(), String> {
        self.eval_with_callback(script, |_| {})
    }

    /// Exists for host paths compiled only on Windows.
    pub fn with_webview(&self, f: impl FnOnce(PlatformWebview) + Send + 'static) -> Result<(), String> {
        self.run_on_main_thread(move || f(PlatformWebview))
    }

    /// Web Inspector, through the SPI Safari's own "Inspect Element" uses. The page stays inspectable
    /// from Safari's Develop menu either way (`setInspectable` at creation).
    pub fn open_devtools(&self) {
        let _ = self.with_web("open Web Inspector", |web, _| {
            if web.respondsToSelector(sel!(_inspector)) {
                // SAFETY: guarded above; `_inspector` returns the view's `_WKInspector`, on the main thread.
                unsafe {
                    let inspector: *mut AnyObject = msg_send![web, _inspector];
                    if !inspector.is_null() {
                        let _: () = msg_send![inspector, show];
                    }
                }
            }
            Ok(())
        });
    }

    pub fn close_devtools(&self) {
        let _ = self.with_web("close Web Inspector", |web, _| {
            if web.respondsToSelector(sel!(_inspector)) {
                // SAFETY: as `open_devtools`.
                unsafe {
                    let inspector: *mut AnyObject = msg_send![web, _inspector];
                    if !inspector.is_null() {
                        let _: () = msg_send![inspector, close];
                    }
                }
            }
            Ok(())
        });
    }

    pub fn is_devtools_open(&self) -> bool {
        false
    }

    /// The process rendering this page, where WebKit says (SPI). What a check needs to kill it.
    pub fn web_process_id(&self) -> Option<i32> {
        self.with_web("read the page's process", |web, _| {
            if !web.respondsToSelector(sel!(_webProcessIdentifier)) {
                return Ok(None);
            }
            // SAFETY: guarded above; a plain integer property, on the main thread.
            let pid: i32 = unsafe { msg_send![web, _webProcessIdentifier] };
            Ok((pid > 0).then_some(pid))
        })
        .ok()
        .flatten()
    }

    /// Closes the page at once: unlike CEF, WebKit has no asynchronous close to wait out.
    pub fn close(&self) -> Result<(), String> {
        let shared = self.shared.clone();
        on_main("close the page", move || close_native(&shared))
    }

    /// Takes the page out of the window (`parked`) or puts it back, without touching its size or
    /// visibility. Upstream's page goes beneath the React WebView that covers the window; a window
    /// drawn by a GPU surface has nothing to go beneath, so it goes out of bounds instead, where
    /// nothing of it shows and no pointer input reaches it, and it keeps running at full speed.
    pub fn set_stacking(&self, parked: bool) -> Result<(), String> {
        self.shared.parked.store(parked, Ordering::SeqCst);
        self.apply_frame()
    }

    /// Rounds the page's two bottom corners to `radius` and clips the page to its own frame.
    pub fn set_bottom_corner_radius(&self, radius: f64) -> Result<(), String> {
        if *lock(&self.shared.bottom_corner_radius) == Some(radius) {
            return Ok(());
        }
        self.with_web("round the page's corners", move |web, _| {
            web.setWantsLayer(true);
            // SAFETY: `layer` is the view's CALayer, on the main thread; these are plain CALayer
            // properties, so nothing here depends on QuartzCore's bindings.
            unsafe {
                let layer: *mut AnyObject = msg_send![web, layer];
                if layer.is_null() {
                    return Err("the page has no layer".into());
                }
                let flipped: bool = msg_send![layer, contentsAreFlipped];
                // `CACornerMask` bits, by the corner of the layer's bounds each one rounds. Which
                // edge is the bottom depends on whether the layer is drawn flipped.
                let bottom: usize = if flipped { 4 | 8 } else { 1 | 2 };
                let _: () = msg_send![layer, setMasksToBounds: true];
                let _: () = msg_send![layer, setCornerRadius: radius];
                let _: () = msg_send![layer, setMaskedCorners: bottom];
            }
            Ok(())
        })?;
        *lock(&self.shared.bottom_corner_radius) = Some(radius);
        Ok(())
    }

    /// Hides the page and lets WebKit suspend its process, which is what a sleeping tab is; its exact
    /// state comes back with it.
    /// WebKit already stops a page in a window that can't be seen.
    pub fn set_window_hidden(&self, _hidden: bool) -> Result<(), String> {
        Ok(())
    }

    pub fn set_sleeping(&self, sleeping: bool) -> Result<(), String> {
        self.with_web("put the page to sleep", move |web, _| {
            set_inactive_policy(web, if sleeping { WKInactiveSchedulingPolicy::Suspend } else { WKInactiveSchedulingPolicy::None });
            web.setHidden(sleeping);
            Ok(())
        })
    }

    /// Points every connection the page makes at `proxy`, or back at this computer's own network.
    pub fn set_network_proxy(&self, proxy: Option<String>) -> Result<(), String> {
        let shared = self.shared.clone();
        on_main("set the page's network", move || {
            let store = native_store(&shared.label).ok_or_else(|| "the page is closed".to_owned())?;
            set_store_proxy(&store, proxy.as_deref())?;
            shared.proxied.store(proxy.is_some(), Ordering::SeqCst);
            cdp::network_changed(&shared);
            Ok(())
        })?
    }

    /// The directory the page's profile is named by. The store itself is in memory (see `store_for`).
    pub fn cache_path(&self) -> Result<PathBuf, String> {
        self.shared.profile.clone().ok_or_else(|| "the page has no profile of its own".to_owned())
    }

    /// Serves `folder` at `https://<host>/` for this page only.
    pub fn map_virtual_host_folder(&self, host: &str, folder: &Path) -> Result<(), String> {
        lock(&self.shared.virtual_hosts).insert(host.to_owned(), folder.to_path_buf());
        Ok(())
    }

    pub fn clear_virtual_host_folder(&self, host: &str) -> Result<(), String> {
        lock(&self.shared.virtual_hosts).remove(host);
        Ok(())
    }

    // ----- DevTools protocol ------------------------------------------------------------------

    /// One DevTools method call, answered by the adapter; `completion` receives the JSON of its
    /// `result` or the text of its error. Any thread. A method the adapter can't answer completes
    /// with an error saying so, at once, so nothing waits on it.
    pub fn send_devtools_message(&self, method: &str, params: &str, completion: CdpCompletion) {
        let shared = self.shared.clone();
        let method = method.to_owned();
        let params = zeroize::Zeroizing::new(params.to_owned());
        let send = move || cdp::dispatch(&shared, &method, &params, completion);
        if is_main_thread() {
            send();
        } else {
            post_main(send);
        }
    }

    /// A DevTools call that waits for its result. Off the main thread only.
    pub fn call_devtools_blocking(&self, method: &str, params: &str, timeout: Duration) -> Result<String, String> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.send_devtools_message(
            method,
            params,
            Box::new(move |result| {
                let _ = sender.try_send(result);
            }),
        );
        receiver.recv_timeout(timeout).map_err(|_| format!("timed out waiting for DevTools {method}"))?
    }

    /// Registers a listener for every DevTools event the page emits. Listeners run on the main thread
    /// and must not wait for a DevTools result themselves.
    pub fn set_event_listener(&self, listener: Option<EventListener>) {
        let mut listeners = lock(&self.shared.listeners);
        listeners.clear();
        listeners.extend(listener);
    }

    pub fn set_dialog_handler(&self, handler: Option<DialogHandlerFn>) {
        *lock(&self.shared.dialog_handler) = handler;
    }

    pub fn set_crash_handler(&self, handler: Option<CrashHandlerFn>) {
        *lock(&self.shared.crash_handler) = handler;
    }

    /// Answers a dialog [`DialogDecision::Hold`] kept open.
    pub fn answer_dialog(&self, id: u64, accept: bool, text: Option<String>) -> Result<(), String> {
        let label = self.shared.label.clone();
        on_main("answer the page's dialog", move || {
            let held = NATIVE.with(|native| native.borrow_mut().get_mut(&label).and_then(|page| page.held_dialogs.remove(&id)));
            let held = held.ok_or_else(|| "the dialog is no longer open".to_owned())?;
            held.answer(accept, text);
            Ok(())
        })?
    }
}

fn load(web: &WKWebView, url: &str) -> Result<(), String> {
    let ns_url = NSURL::URLWithString(&NSString::from_str(url)).ok_or_else(|| format!("WebKit refused the address {url}"))?;
    let request = NSURLRequest::requestWithURL(&ns_url);
    // SAFETY: on the main thread with a live view.
    let _ = unsafe { web.loadRequest(&request) };
    Ok(())
}

fn current_url(web: &WKWebView) -> String {
    // SAFETY: a property read on the main thread.
    unsafe { web.URL() }.and_then(|url| url.absoluteString()).map_or_else(|| BLANK_URL.to_owned(), |url| url.to_string())
}

fn set_inactive_policy(web: &WKWebView, policy: WKInactiveSchedulingPolicy) {
    // SAFETY: a property read on the main thread.
    let preferences = unsafe { web.configuration().preferences() };
    if preferences.respondsToSelector(sel!(setInactiveSchedulingPolicy:)) {
        // SAFETY: guarded above: macOS 14 has it, and before that a hidden page is merely throttled.
        unsafe { preferences.setInactiveSchedulingPolicy(policy) };
    }
}

/// Places the view at the page's frame, which is in the client's coordinates: logical pixels from the
/// top-left of the content view. An emulated viewport keeps its CSS size by scaling down with page
/// zoom: `innerWidth` is the frame's width over the zoom, so a frame `w * z` wide at zoom `z` is a
/// viewport `w` wide, however little of the pane it takes.
fn place_view(web: &WKWebView, shared: &PageShared) {
    let Some(parent) = (unsafe { web.superview() }) else {
        return;
    };
    let frame = *lock(&shared.frame);
    let emulation = *lock(&shared.emulation);
    let user_zoom = *lock(&shared.zoom);
    let (width, height, zoom) = fitted_frame(frame.width, frame.height, emulation.viewport, user_zoom);
    // SAFETY: plain property writes on the main thread.
    unsafe {
        if web.pageZoom() != zoom {
            web.setPageZoom(zoom);
        }
    }
    let x = if shared.parked.load(Ordering::SeqCst) { PARKED_X } else { frame.x };
    let y = if parent.isFlipped() { frame.y } else { parent.bounds().size.height - frame.y - height };
    web.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(width, height)));
}

/// The view's size and page zoom for a pane of `pane_width` by `pane_height`.
///
/// WebKit truncates a view's size to whole points and floors the viewport: `innerWidth` is
/// `floor(floor(frame) / zoom)`. So an emulated viewport needs whole frame sizes and one zoom that
/// lands on both of its dimensions at once, which the plain scale rarely does.
fn fitted_frame(pane_width: f64, pane_height: f64, viewport: Option<(f64, f64)>, user_zoom: f64) -> (f64, f64, f64) {
    let pane_width = pane_width.max(0.0);
    let pane_height = pane_height.max(0.0);
    let Some((width, height)) = viewport.filter(|(width, height)| *width > 0.0 && *height > 0.0) else {
        return (pane_width, pane_height, user_zoom);
    };
    let scale = (pane_width / width).min(pane_height / height).min(1.0).max(0.01);
    if scale >= 1.0 {
        return (width, height, 1.0);
    }
    // A frame height admits zooms in `[frame / (height + 1), frame / height]`, and likewise for the
    // width. Walking down from the largest frame that fits finds one both admit within a few steps.
    let tallest = (height * scale).ceil().min(pane_height.floor()).max(1.0);
    for step in 0..64 {
        let frame_height = tallest - step as f64;
        if frame_height < 1.0 {
            break;
        }
        let (low, high) = (frame_height / (height + 1.0), frame_height / height);
        let frame_width = (low * width).ceil().max(1.0);
        let (low, high) = (low.max(frame_width / (width + 1.0)), high.min(frame_width / width));
        if low < high && frame_width <= pane_width.ceil() {
            return (frame_width, frame_height, (low + high) / 2.0);
        }
    }
    ((width * scale).ceil(), (height * scale).ceil(), scale)
}

// ----- proxies ---------------------------------------------------------------------------------------

/// What `socks5://host:port` and `http://host:port` say.
fn parse_proxy(proxy: &str) -> Result<(bool, String, String), String> {
    let url = url::Url::parse(proxy).map_err(|error| format!("invalid proxy {proxy}: {error}"))?;
    let socks = match url.scheme() {
        "socks5" | "socks5h" | "socks" => true,
        "http" | "https" => false,
        other => return Err(format!("unsupported proxy scheme {other}")),
    };
    let host = url.host_str().ok_or_else(|| format!("invalid proxy {proxy}: no host"))?.trim_matches(['[', ']']).to_owned();
    let port = url.port_or_known_default().ok_or_else(|| format!("invalid proxy {proxy}: no port"))?;
    Ok((socks, host, port.to_string()))
}

/// macOS 14's per-store proxy. The Network framework symbols are looked up at run time: linking them
/// would stop the app from starting on an older system over one feature of one kind of page.
fn set_store_proxy(store: &WKWebsiteDataStore, proxy: Option<&str>) -> Result<(), String> {
    use std::ffi::CString;
    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
    }
    // `RTLD_DEFAULT`: WebKit links Network, so its symbols are already loaded.
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
    let selector = sel!(setProxyConfigurations:);
    if !store.respondsToSelector(selector) {
        return Err("a page on another machine's network needs macOS 14 or later".into());
    }
    let configurations: Retained<NSArray<AnyObject>> = match proxy {
        None => NSArray::new(),
        Some(proxy) => {
            let (socks, host, port) = parse_proxy(proxy)?;
            type CreateHost = unsafe extern "C" fn(*const std::ffi::c_char, *const std::ffi::c_char) -> *mut AnyObject;
            type CreateConfig = unsafe extern "C" fn(*mut AnyObject, *mut AnyObject) -> *mut AnyObject;
            type CreateSocks = unsafe extern "C" fn(*mut AnyObject) -> *mut AnyObject;
            // SAFETY: the names and signatures are Network.framework's public C API. Each returned
            // object is owned by the caller (`os_object`, an Objective-C object), hence `from_raw`.
            unsafe {
                let create_host = dlsym(RTLD_DEFAULT, c"nw_endpoint_create_host".as_ptr());
                if create_host.is_null() {
                    return Err("the Network framework has no endpoints on this system".into());
                }
                let create_host: CreateHost = std::mem::transmute(create_host);
                let host = CString::new(host).map_err(|e| e.to_string())?;
                let port = CString::new(port).map_err(|e| e.to_string())?;
                let endpoint = create_host(host.as_ptr(), port.as_ptr());
                let endpoint = Retained::from_raw(endpoint).ok_or_else(|| "the proxy's address is invalid".to_owned())?;
                let config = if socks {
                    let create = dlsym(RTLD_DEFAULT, c"nw_proxy_config_create_socksv5".as_ptr());
                    if create.is_null() {
                        return Err("a page on another machine's network needs macOS 14 or later".into());
                    }
                    let create: CreateSocks = std::mem::transmute(create);
                    create(Retained::as_ptr(&endpoint) as *mut AnyObject)
                } else {
                    let create = dlsym(RTLD_DEFAULT, c"nw_proxy_config_create_http_connect".as_ptr());
                    if create.is_null() {
                        return Err("a page on another machine's network needs macOS 14 or later".into());
                    }
                    let create: CreateConfig = std::mem::transmute(create);
                    create(Retained::as_ptr(&endpoint) as *mut AnyObject, std::ptr::null_mut())
                };
                let config = Retained::from_raw(config).ok_or_else(|| "the proxy's address is invalid".to_owned())?;
                NSArray::from_retained_slice(&[config])
            }
        }
    };
    // SAFETY: guarded above; the array holds `nw_proxy_config_t`s as the property documents.
    unsafe {
        let _: () = msg_send![store, setProxyConfigurations: &*configurations];
    }
    Ok(())
}

// ----- the builder -------------------------------------------------------------------------------------

/// `WebviewBuilder`'s surface as `browser.rs` uses it.
pub struct WebviewBuilder<R = Wry> {
    label: String,
    url: url::Url,
    data_directory: Option<PathBuf>,
    initialization_scripts: Vec<String>,
    callbacks: PageCallbacks,
    network_proxy: Option<String>,
    runtime: std::marker::PhantomData<fn() -> R>,
}

impl WebviewBuilder<Wry> {
    pub fn new(label: impl Into<String>, url: WebviewUrl) -> Self {
        let url = match url {
            WebviewUrl::External(url) => url,
            _ => url::Url::parse(BLANK_URL).expect("about:blank parses"),
        };
        Self {
            label: label.into(),
            url,
            data_directory: None,
            initialization_scripts: Vec::new(),
            callbacks: PageCallbacks::default(),
            network_proxy: None,
            runtime: std::marker::PhantomData,
        }
    }

    /// The proxy every connection of the page goes through; `None` is the computer's own network.
    pub fn network_proxy(mut self, proxy: Option<String>) -> Self {
        self.network_proxy = proxy;
        self
    }

    pub fn data_directory(mut self, directory: PathBuf) -> Self {
        self.data_directory = Some(directory);
        self
    }

    pub fn initialization_script(mut self, script: impl Into<String>) -> Self {
        self.initialization_scripts.push(script.into());
        self
    }

    /// Chromium switches: there is no Chromium. A hidden page is kept running by the store's
    /// scheduling policy (`set_inactive_policy`) instead.
    pub fn additional_browser_args(self, _args: &str) -> Self {
        self
    }

    pub fn zoom_hotkeys_enabled(self, _enabled: bool) -> Self {
        self
    }

    pub fn devtools(self, _enabled: bool) -> Self {
        self
    }

    pub fn general_autofill_enabled(self, _enabled: bool) -> Self {
        self
    }

    pub fn on_navigation(mut self, handler: impl Fn(&url::Url) -> bool + Send + 'static) -> Self {
        self.callbacks.navigation = Some(Box::new(handler));
        self
    }

    pub fn on_new_window(mut self, handler: impl Fn(url::Url, NewWindowFeatures) -> NewWindowResponse + Send + 'static) -> Self {
        self.callbacks.new_window = Some(Box::new(handler));
        self
    }

    pub fn on_download(mut self, handler: impl Fn(Webview, DownloadEvent) -> bool + Send + 'static) -> Self {
        self.callbacks.download = Some(Box::new(handler));
        self
    }

    pub fn on_page_load(mut self, handler: impl Fn(Webview, PageLoadPayload) + Send + 'static) -> Self {
        self.callbacks.page_load = Some(Box::new(handler));
        self
    }

    pub fn on_document_title_changed(mut self, handler: impl Fn(Webview, String) + Send + 'static) -> Self {
        self.callbacks.title = Some(Box::new(handler));
        self
    }
}

/// Creates the page as a child of `window` at `position`/`size` and returns once its first navigation
/// has been issued, the moment `Window::add_child` returns a WebView2 page.
pub fn add_child(window: &Window, builder: WebviewBuilder<Wry>, position: Position, size: Size) -> Result<Webview, String> {
    let scale = window.scale_factor().unwrap_or(1.0).max(f64::EPSILON);
    let position = position.to_logical_at(scale);
    let size = size.to_logical_at(scale);
    let WebviewBuilder { label, url, data_directory, initialization_scripts, callbacks, network_proxy, .. } = builder;
    if pages().as_ref().is_some_and(|pages| pages.contains_key(&label)) {
        return Err(format!("the page {label} already exists"));
    }
    let shared = Arc::new(PageShared {
        label: label.clone(),
        app: window.app.clone(),
        window: window.clone(),
        callbacks: MainOnly(callbacks),
        frame: Mutex::new(LogicalFrame { x: position.x, y: position.y, width: size.width, height: size.height }),
        parked: AtomicBool::new(false),
        bottom_corner_radius: Mutex::new(None),
        zoom: Mutex::new(1.0),
        emulation: Mutex::new(Emulation::default()),
        closed: AtomicBool::new(false),
        listeners: Mutex::new(Vec::new()),
        dialog_handler: Mutex::new(None),
        crash_handler: Mutex::new(None),
        profile: data_directory,
        virtual_hosts: Mutex::new(HashMap::new()),
        open_requests: Mutex::new(HashSet::new()),
        next_document: AtomicU64::new(1),
        document_request: Mutex::new(None),
        navigation_method: Mutex::new("GET".to_owned()),
        showing_failure_for: Mutex::new(None),
        inspect_config: Mutex::new(None),
        intercept_file_chooser: AtomicBool::new(false),
        proxied: AtomicBool::new(network_proxy.is_some()),
    });
    {
        let shared = shared.clone();
        on_main("create the page", move || create_native(&shared, &initialization_scripts, network_proxy.as_deref(), &url))??;
    }
    Ok(shared.handle())
}

/// A new name for the page's message handler. See `hooks.js`.
fn random_name() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
        // Only ever a defense in depth; a time-based name is still not one a page can guess in advance.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        bytes.copy_from_slice(&now.to_le_bytes());
    }
    let mut name = String::from("mewrk");
    for byte in bytes {
        name.push_str(&format!("{byte:02x}"));
    }
    name
}

fn add_user_script(controller: &objc2_web_kit::WKUserContentController, source: &str, mtm: MainThreadMarker) {
    // SAFETY: constructors on the main thread; a user script is immutable once made.
    unsafe {
        let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
            WKUserScript::alloc(mtm),
            &NSString::from_str(source),
            WKUserScriptInjectionTime::AtDocumentStart,
            false,
        );
        controller.addUserScript(&script);
    }
}

/// Main-thread half of [`add_child`].
fn create_native(shared: &Arc<PageShared>, scripts: &[String], proxy: Option<&str>, url: &url::Url) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or_else(|| "pages are created on the main thread".to_owned())?;
    let parent = main_view().ok_or_else(|| "no page engine is attached: the client has no window".to_owned())?;
    let store = store_for(shared.profile.as_deref(), mtm);
    set_store_proxy_if(&store, proxy)?;
    let handler = random_name();
    let delegate = delegate::PageDelegate::new(shared.clone(), mtm);
    let mut base_scripts = vec![include_str!("hooks.js").replace("__MEWRK_HANDLER__", &handler)];
    base_scripts.extend(scripts.iter().cloned());
    // SAFETY: everything below is on the main thread, on objects created here and the live content
    // view of the client's window. The delegate is retained by `NativePage` for the page's life.
    let web = unsafe {
        let config = WKWebViewConfiguration::new(mtm);
        config.setWebsiteDataStore(&store);
        config.preferences().setJavaScriptCanOpenWindowsAutomatically(true);
        let controller = config.userContentController();
        for script in &base_scripts {
            add_user_script(&controller, script, mtm);
        }
        controller.addScriptMessageHandler_contentWorld_name(
            ProtocolObject::from_ref(&*delegate),
            &WKContentWorld::pageWorld(mtm),
            &NSString::from_str(&handler),
        );
        config.setURLSchemeHandler_forURLScheme(Some(ProtocolObject::from_ref(&*delegate)), &NSString::from_str(PREVIEW_SCHEME));
        let frame = NSRect::new(NSPoint::new(PARKED_X, 0.0), NSSize::new(1.0, 1.0));
        let web = WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config);
        web.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        web.setUIDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        if web.respondsToSelector(sel!(setInspectable:)) {
            web.setInspectable(true);
        }
        let _: () = msg_send![&*web, addObserver: &*delegate, forKeyPath: ns_string!("title"), options: 1usize, context: std::ptr::null_mut::<c_void>()];
        (*parent).addSubview(&web);
        web
    };
    // Upstream starts Chromium with `--disable-backgrounding-occluded-windows`, because a page the Agent is
    // driving behind another window has to keep running, and a remote screen's viewer stops its stream the
    // moment `document.hidden` says so. This is WebKit's own switch for that, as SPI: where it is missing a
    // covered window's page simply reads as hidden.
    if web.respondsToSelector(sel!(_setWindowOcclusionDetectionEnabled:)) {
        // SAFETY: guarded above; a plain boolean property, on the main thread.
        unsafe {
            let _: () = msg_send![&*web, _setWindowOcclusionDetectionEnabled: false];
        }
    }
    // Keep the distance to the window's top edge as the window resizes; the pane's bounds follow.
    web.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
    set_inactive_policy(&web, WKInactiveSchedulingPolicy::None);
    place_view(&web, shared);
    if proxy.is_some() {
        // The page's own scripts have to know before the first document runs.
        // SAFETY: on the main thread, with the controller of the view made above.
        let controller = unsafe { web.configuration().userContentController() };
        add_user_script(&controller, cdp::LOOPBACK_SCRIPT, mtm);
    }
    NATIVE.with(|native| {
        native.borrow_mut().insert(
            shared.label.clone(),
            NativePage {
                web: web.clone(),
                delegate,
                store,
                base_scripts,
                held_dialogs: HashMap::new(),
                file_chooser: None,
                scheme_tasks: HashMap::new(),
            },
        )
    });
    pages().get_or_insert_with(HashMap::new).insert(shared.label.clone(), shared.clone());
    // No page shows anything until the host places it, so this first load is inert until then.
    load(&web, &shared.engine_url(url))?;
    Ok(())
}

fn set_store_proxy_if(store: &WKWebsiteDataStore, proxy: Option<&str>) -> Result<(), String> {
    match proxy {
        Some(_) => set_store_proxy(store, proxy),
        // Clear what an earlier page of this profile may have set, where that is possible at all.
        None => {
            if store.respondsToSelector(sel!(setProxyConfigurations:)) {
                set_store_proxy(store, None)
            } else {
                Ok(())
            }
        }
    }
}

/// Main-thread half of [`Webview::close`].
fn close_native(shared: &Arc<PageShared>) {
    shared.closed.store(true, Ordering::SeqCst);
    {
        let mut pages = pages();
        if let Some(pages) = pages.as_mut() {
            if pages.get(&shared.label).is_some_and(|current| Arc::ptr_eq(current, shared)) {
                pages.remove(&shared.label);
            }
        }
    }
    let native = NATIVE.with(|native| native.borrow_mut().remove(&shared.label));
    lock(&shared.listeners).clear();
    *lock(&shared.dialog_handler) = None;
    *lock(&shared.crash_handler) = None;
    let Some(native) = native else {
        return;
    };
    // WebKit requires every completion handler to be called before it is released.
    for (_, held) in native.held_dialogs {
        held.answer(false, None);
    }
    if let Some(chooser) = native.file_chooser {
        chooser.call((std::ptr::null_mut(),));
    }
    // SAFETY: on the main thread, with the objects this page registered at creation.
    unsafe {
        let web = &native.web;
        let delegate = &native.delegate;
        let _: () = msg_send![&**web, removeObserver: &**delegate, forKeyPath: ns_string!("title")];
        web.stopLoading();
        web.setNavigationDelegate(None);
        web.setUIDelegate(None);
        web.configuration().userContentController().removeAllScriptMessageHandlers();
        web.removeFromSuperview();
    }
}

/// Re-registers what every document starts with, plus what the page's current state adds to it.
fn reinstall_user_scripts(label: &str, extra: &[String]) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let found = NATIVE.with(|native| native.borrow().get(label).map(|page| (page.web.clone(), page.base_scripts.clone())));
    let Some((web, scripts)) = found else {
        return;
    };
    // SAFETY: on the main thread with a live view.
    unsafe {
        let controller = web.configuration().userContentController();
        controller.removeAllUserScripts();
        for script in &scripts {
            add_user_script(&controller, script, mtm);
        }
        for script in extra {
            add_user_script(&controller, script, mtm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_emulated_viewport_scales_down_to_fit_and_never_up() {
        assert_eq!(fitted_frame(800.0, 600.0, None, 1.25), (800.0, 600.0, 1.25));
        // Fits as it is: its own size, and the user's zoom doesn't apply to an emulated viewport.
        assert_eq!(fitted_frame(800.0, 900.0, Some((375.0, 812.0)), 1.25), (375.0, 812.0, 1.0));
        let (width, height, zoom) = fitted_frame(800.0, 406.0, Some((375.0, 812.0)), 1.0);
        assert!((zoom - 0.5).abs() < 0.01, "{zoom}");
        assert!(width <= 188.0 && height <= 406.0, "{width}x{height}");
    }

    #[test]
    fn an_emulated_viewport_comes_out_exactly_the_size_asked_for() {
        // What WebKit does with a frame and a zoom, measured: 351 / 0.90047 reads as 389.
        let viewport_of = |frame: f64, zoom: f64| (frame.floor() / zoom).floor();
        for (pane_width, pane_height) in [(800.0, 600.0), (1000.0, 760.0), (640.0, 480.0), (333.0, 777.0), (1920.0, 300.0)] {
            for (width, height) in [(375.0, 812.0), (390.0, 844.0), (768.0, 1024.0), (1280.0, 720.0), (2560.0, 1440.0), (9999.0, 9999.0), (321.0, 123.0)] {
                let (frame_width, frame_height, zoom) = fitted_frame(pane_width, pane_height, Some((width, height)), 1.0);
                assert_eq!(viewport_of(frame_width, zoom), width, "{width}x{height} in {pane_width}x{pane_height}: {frame_width} at {zoom}");
                assert_eq!(viewport_of(frame_height, zoom), height, "{width}x{height} in {pane_width}x{pane_height}: {frame_height} at {zoom}");
                assert!(frame_width <= pane_width.max(width) && frame_height <= pane_height.max(height), "{frame_width}x{frame_height} overflows {pane_width}x{pane_height}");
            }
        }
    }

    #[test]
    fn loopback_goes_by_an_alias_only_webkit_sees() {
        let parse = |text: &str| url::Url::parse(text).unwrap();
        assert_eq!(aliased_url(&parse("http://localhost:5173/a?b=1#c")), "http://machine.localhost:5173/a?b=1#c");
        assert_eq!(aliased_url(&parse("http://127.0.0.1:8080/")), "http://ipv4.machine.localhost:8080/");
        assert_eq!(aliased_url(&parse("http://127.1:8080/")), "http://ipv4.machine.localhost:8080/");
        assert_eq!(aliased_url(&parse("http://0.0.0.0:8080/")), "http://ipv4.machine.localhost:8080/");
        assert_eq!(aliased_url(&parse("http://[::1]:8080/")), "http://ipv6.machine.localhost:8080/");
        assert_eq!(aliased_url(&parse("https://example.com/")), "https://example.com/");
        assert_eq!(aliased_url(&parse("http://192.168.1.2/")), "http://192.168.1.2/");
        assert_eq!(public_url("http://machine.localhost:5173/a?b=1"), "http://localhost:5173/a?b=1");
        assert_eq!(public_url("http://ipv4.machine.localhost:8080/"), "http://127.0.0.1:8080/");
        assert_eq!(public_url("http://ipv6.machine.localhost:8080/"), "http://[::1]:8080/");
        assert_eq!(public_url("https://example.com/machine.localhost"), "https://example.com/machine.localhost");
        assert_eq!(public_url("mewrk-preview://mewrk-file-preview.invalid/a.html"), "https://mewrk-file-preview.invalid/a.html");
    }

    #[test]
    fn proxies_are_read_the_way_the_tunnel_writes_them() {
        assert_eq!(parse_proxy("socks5://127.0.0.1:40001").unwrap(), (true, "127.0.0.1".to_owned(), "40001".to_owned()));
        assert_eq!(parse_proxy("http://127.0.0.1:40001").unwrap(), (false, "127.0.0.1".to_owned(), "40001".to_owned()));
        assert!(parse_proxy("ftp://127.0.0.1:1").is_err());
    }
}
