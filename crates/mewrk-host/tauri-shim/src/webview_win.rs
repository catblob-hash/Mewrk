//! One built-in-browser page on Windows: a WebView2 controller in a child window of the client's window.
//!
//! This is the part wry and tao did when the host ran on Tauri. The host's `cfg(windows)` paths take the controller and
//! environment from `with_webview` and drive DevTools, sleep and profile data themselves, so what lives here is creating
//! controllers, the builder's options, placing the page, and getting every call onto the right thread.
//!
//! Threading: WebView2 belongs to one STA thread, the one that attached the window, which is winit's event loop thread.
//! Its COM objects never leave it: they live in the thread-local [`NATIVE`] registry keyed by label, and everything else
//! reaches them through a hidden message-only window, so a call still runs while the client's window is minimized, in a
//! modal resize, or not drawing. The only state shared across threads is [`PageShared`], which holds no COM object.
//!
//! Creation is asynchronous all the way down (environment, controller, scripts, first navigation) and blocks nothing on
//! the main thread. `add_child` off the main thread, which is how the host calls it, simply waits for the last step. On
//! the main thread it has to return a finished page too, and there it pumps only what WebView2 needs (see [`pump_until`]).
//!
//! Each page has a container window of its own, as wry's child webviews do, because the host restacks whatever
//! `ICoreWebView2Controller::ParentWindow` says. See [`container_proc`] for what restacking means here.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    take_pwstr, AddScriptToExecuteOnDocumentCreatedCompletedHandler, ClearBrowsingDataCompletedHandler, ContentLoadingEventHandler,
    CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    DocumentTitleChangedEventHandler, DownloadStartingEventHandler, ExecuteScriptCompletedHandler, NavigationCompletedEventHandler,
    NavigationStartingEventHandler, NewWindowRequestedEventHandler,
};
use windows::core::{w, Interface, BOOL, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Globalization::{LCIDToLocaleName, GetUserDefaultUILanguage, LOCALE_ALLOW_NEUTRAL_NAMES, MAX_LOCALE_NAME};
use windows::Win32::Graphics::Gdi::{ClientToScreen, CreateRectRgn, DeleteObject, SetWindowRgn};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, GetClassNameW, GetClientRect, GetWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, MsgWaitForMultipleObjectsEx, PeekMessageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetWindowPos, ShowWindow, CS_HREDRAW, CS_VREDRAW, GWL_STYLE, GW_CHILD, HWND_BOTTOM, HWND_MESSAGE, HWND_TOP,
    MSG, MWMO_INPUTAVAILABLE, PEEK_MESSAGE_REMOVE_TYPE, PM_QS_POSTMESSAGE, PM_QS_SENDMESSAGE, PM_REMOVE, QS_POSTMESSAGE, QS_SENDMESSAGE, QS_TIMER,
    SIZE_MINIMIZED, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWNA, WINDOWPOS, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_DPICHANGED, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOVE, WM_NCDESTROY, WM_POINTERDOWN, WM_QUIT, WM_RBUTTONDOWN, WM_SETFOCUS,
    WM_SIZE, WM_WINDOWPOSCHANGING, WM_XBUTTONDOWN, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_NOPARENTNOTIFY,
    WS_VISIBLE,
};

use crate::{AppHandle, Error, Manager, PhysicalPosition, PhysicalSize, Position, Result, Size, WebviewUrl, WindowEvent, Wry};

const BLANK_URL: &str = "about:blank";
const MAIN_THREAD_TIMEOUT: Duration = Duration::from_secs(15);
/// A first run is a cold browser process making a new profile, possibly behind a virus scanner.
const CREATION_TIMEOUT: Duration = Duration::from_secs(30);
const WM_RUN_TASK: u32 = WM_APP + 1;
const MAIN_SUBCLASS_ID: usize = 0x6d65_7772;
/// wry's own default, which any `additional_browser_args` replaces: no mini menu, no SmartScreen.
const DEFAULT_BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";
const RUNTIME_MISSING: &str = "未找到 WebView2 运行时，无法打开内置浏览器页面 (the Microsoft Edge WebView2 Runtime is not installed)";

fn no_window() -> String {
    "no page engine is attached: the client has no window".to_owned()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn describe(stage: &str, error: windows::core::Error) -> String {
    format!("{stage}: {error}")
}

/// A panic can't unwind out of a window procedure or a COM callback: the process would abort.
fn guarded<T>(stage: &str, f: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(_) => {
            eprintln!("{stage}: a callback panicked");
            None
        }
    }
}

// ----- the main thread ---------------------------------------------------------------------------

static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);
static DISPATCHER: AtomicIsize = AtomicIsize::new(0);

type Task = Box<dyn FnOnce() + Send>;

static TASKS: Mutex<VecDeque<Task>> = Mutex::new(VecDeque::new());
static TASK_MESSAGE_POSTED: AtomicBool = AtomicBool::new(false);

pub fn is_main_thread() -> bool {
    // SAFETY: no arguments, reads the calling thread.
    MAIN_THREAD.load(Ordering::Acquire) == unsafe { GetCurrentThreadId() }
}

fn hwnd_from(value: isize) -> HWND {
    HWND(value as *mut c_void)
}

fn main_window() -> Option<HWND> {
    let window = MAIN_WINDOW.load(Ordering::Acquire);
    (window != 0).then(|| hwnd_from(window))
}

/// At most one wake-up is on the thread's queue however many tasks wait, because the queue has a limit.
fn post_task_message(dispatcher: isize) {
    if !TASK_MESSAGE_POSTED.swap(true, Ordering::AcqRel) {
        // SAFETY: the dispatcher lives as long as the process.
        if unsafe { PostMessageW(Some(hwnd_from(dispatcher)), WM_RUN_TASK, WPARAM(0), LPARAM(0)) }.is_err() {
            TASK_MESSAGE_POSTED.store(false, Ordering::Release);
        }
    }
}

/// Queues `f` on the main thread, always later: a caller inside a WebView2 callback stays re-entrant.
pub fn post_main(f: impl FnOnce() + Send + 'static) -> std::result::Result<(), String> {
    let dispatcher = DISPATCHER.load(Ordering::Acquire);
    if dispatcher == 0 {
        return Err(no_window());
    }
    lock(&TASKS).push_back(Box::new(f));
    post_task_message(dispatcher);
    Ok(())
}

/// Runs `f` on the main thread and waits: inline when already there, so a caller on it can't deadlock.
pub(crate) fn on_main<T: Send + 'static>(stage: &str, f: impl FnOnce() -> T + Send + 'static) -> std::result::Result<T, String> {
    if is_main_thread() {
        return Ok(f());
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    post_main(move || {
        let _ = sender.try_send(f());
    })?;
    receiver.recv_timeout(MAIN_THREAD_TIMEOUT).map_err(|_| format!("timed out waiting for the main thread: {stage}"))
}

/// For what's already on the main thread and is going to stay there: lets it through the cross-thread queue, which is
/// the only thing that ever holds it.
struct StaysOnMain<T>(T);

// SAFETY: built on the main thread by `post_local`, and only opened again by the task it queues there.
unsafe impl<T> Send for StaysOnMain<T> {}

impl<T> StaysOnMain<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

fn post_local(f: impl FnOnce() + 'static) {
    let task = StaysOnMain(f);
    if let Err(error) = post_main(move || (task.into_inner())()) {
        eprintln!("{error}");
    }
}

/// One task a message, so that the thread's input and paint messages keep their turn between tasks.
unsafe extern "system" fn dispatcher_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message != WM_RUN_TASK {
        // SAFETY: the system's default for a window procedure's own arguments.
        return unsafe { DefWindowProcW(window, message, wparam, lparam) };
    }
    TASK_MESSAGE_POSTED.store(false, Ordering::Release);
    let (task, more) = {
        let mut tasks = lock(&TASKS);
        let task = tasks.pop_front();
        (task, !tasks.is_empty())
    };
    if more {
        post_task_message(window.0 as isize);
    }
    if let Some(task) = task {
        guarded("a task on the main thread", task);
    }
    LRESULT(0)
}

fn module_instance() -> HINSTANCE {
    // SAFETY: a null name is this executable, which is always loaded.
    unsafe { GetModuleHandleW(PCWSTR::null()) }.map(Into::into).unwrap_or_default()
}

fn create_dispatcher() -> std::result::Result<isize, String> {
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(dispatcher_proc),
        hInstance: module_instance(),
        lpszClassName: w!("MewrkPageDispatcher"),
        ..Default::default()
    };
    // SAFETY: the class is complete and static. Registering twice fails harmlessly, and the window below says if it matters.
    let window = unsafe {
        RegisterClassExW(&class);
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class.lpszClassName,
            PCWSTR::null(),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(class.hInstance),
            None,
        )
    };
    window.map(|window| window.0 as isize).map_err(|error| describe("create the page dispatcher", error))
}

/// Where pages go: the client's window, `App::native_view`, an HWND here. Idempotent, and views call it every frame. The
/// thread that owns the window becomes the main thread, on which all of WebView2 runs.
pub fn attach_main_view(view: *mut c_void) {
    let handle = view as isize;
    if handle == 0 || MAIN_WINDOW.load(Ordering::Acquire) == handle {
        return;
    }
    let window = HWND(view);
    // SAFETY: both only read; a stale handle reads as owned by no thread.
    let (thread, owner) = unsafe { (GetCurrentThreadId(), GetWindowThreadProcessId(window, None)) };
    let known = MAIN_THREAD.load(Ordering::Acquire);
    if owner != thread || (known != 0 && known != thread) {
        eprintln!("the browser's page engine can only attach a window on the thread that owns it");
        return;
    }
    // winit's `OleInitialize` has normally done this already, unless the client turned off its drag and drop. A thread
    // in the wrong apartment stays that way, and creating a page then reports WebView2's own error.
    // SAFETY: on the window's own thread, for the life of the thread.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if DISPATCHER.load(Ordering::Acquire) == 0 {
        match create_dispatcher() {
            Ok(dispatcher) => DISPATCHER.store(dispatcher, Ordering::Release),
            Err(error) => {
                eprintln!("{error}");
                return;
            }
        }
    }
    MAIN_THREAD.store(thread, Ordering::Release);
    // SAFETY: the window belongs to this thread, and the subclass goes away with it (WM_NCDESTROY).
    if !unsafe { SetWindowSubclass(window, Some(main_window_proc), MAIN_SUBCLASS_ID, 0) }.as_bool() {
        eprintln!("could not follow the client's window: pages won't track its moves and DPI");
    }
    MAIN_WINDOW.store(handle, Ordering::Release);
}

/// Pumps until `receiver` has an answer. A plain pump (wry's `wait_with_pump`) inside a winit callback hands winit its own
/// messages with its handler on the stack. It buffers events then, but a paint message re-arms itself while buffering and
/// spins the pump, input can start a modal move loop that doesn't return until the mouse is let go, and the closures in
/// its posted messages run nested. WebView2 completes through posted and sent messages to its own windows, so those are
/// all that is dispatched here. Messages for winit's windows wait in `held`, and go back on the queue afterwards.
fn pump_until<T>(receiver: &mpsc::Receiver<T>, timeout: Duration) -> std::result::Result<T, mpsc::RecvTimeoutError> {
    let deadline = Instant::now() + timeout;
    let mut held = Vec::new();
    let mut quit = None;
    let filter = PEEK_MESSAGE_REMOVE_TYPE(PM_REMOVE.0 | PM_QS_POSTMESSAGE.0 | PM_QS_SENDMESSAGE.0);
    let outcome = loop {
        match receiver.try_recv() {
            Ok(value) => break Ok(value),
            Err(mpsc::TryRecvError::Disconnected) => break Err(mpsc::RecvTimeoutError::Disconnected),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            break Err(mpsc::RecvTimeoutError::Timeout);
        }
        let mut message = MSG::default();
        // SAFETY: on this thread's own queue, with a message of our own.
        if !unsafe { PeekMessageW(&mut message, None, 0, 0, filter) }.as_bool() {
            // SAFETY: waits on the queue alone, with a timeout.
            unsafe { MsgWaitForMultipleObjectsEx(None, 20, QS_POSTMESSAGE | QS_SENDMESSAGE | QS_TIMER, MWMO_INPUTAVAILABLE) };
            continue;
        }
        if message.message == WM_QUIT {
            quit = Some(message.wParam.0 as i32);
            break Err(mpsc::RecvTimeoutError::Disconnected);
        }
        if is_winit_window(message.hwnd) {
            held.push(message);
        } else {
            // SAFETY: a message taken from the queue just now.
            unsafe { DispatchMessageW(&message) };
        }
    };
    for message in held {
        // SAFETY: as it was posted in the first place. If its window has gone since, it has nobody to reach anyway.
        let _ = unsafe { PostMessageW(Some(message.hwnd), message.message, message.wParam, message.lParam) };
    }
    if let Some(code) = quit {
        // SAFETY: gives the loop that owns the thread what it was told.
        unsafe { PostQuitMessage(code) };
    }
    outcome
}

fn is_winit_window(window: HWND) -> bool {
    if window.is_invalid() {
        return false;
    }
    if main_window() == Some(window) {
        return true;
    }
    let mut class = [0u16; 32];
    // SAFETY: into a buffer of our own; a window that's gone has an empty name.
    let length = unsafe { GetClassNameW(window, &mut class) };
    String::from_utf16_lossy(&class[..length.max(0) as usize]).starts_with("Winit")
}

// ----- the window ---------------------------------------------------------------------------------

type WindowListener = Rc<dyn Fn(&WindowEvent)>;

thread_local! {
    /// Leaked, like the registries below: their COM objects must not be released from a thread-local destructor while
    /// the process goes down, when the runtime that owns them may already be gone.
    static WINDOW_LISTENERS: &'static RefCell<Vec<WindowListener>> = Box::leak(Box::default());
}

fn fire_window_event(event: WindowEvent) {
    let listeners = WINDOW_LISTENERS.with(|listeners| listeners.borrow().clone());
    for listener in listeners {
        guarded("a window listener", || listener(&event));
    }
}

/// What pages need from the client's window, which draws through a GPU surface and knows nothing of them.
unsafe extern "system" fn main_window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM, id: usize, _data: usize) -> LRESULT {
    match message {
        // The surface doesn't take focus when it's clicked, as the sibling webview it replaces did: keys would go on
        // reaching the page, however much the person had moved on to the rest of the window.
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN | WM_POINTERDOWN => take_focus_back(window),
        WM_DESTROY => {
            guarded("the window closing", || {
                MAIN_WINDOW.store(0, Ordering::Release);
                close_all_pages();
                fire_window_event(WindowEvent::Destroyed);
            });
        }
        WM_NCDESTROY => {
            // SAFETY: what `attach_main_view` added, on the thread that added it.
            let _ = unsafe { RemoveWindowSubclass(window, Some(main_window_proc), id) };
        }
        _ => {}
    }
    // SAFETY: on to the window's own procedure, with its own arguments.
    let result = unsafe { DefSubclassProc(window, message, wparam, lparam) };
    guarded("following the window", || match message {
        WM_MOVE => {
            // WebView2 places its popups, menus and dialogs from this.
            for page in all_native_pages() {
                // SAFETY: on the main thread with a live controller.
                let _ = unsafe { page.controller.NotifyParentWindowPositionChanged() };
            }
            let (x, y) = (lparam.0 as i16 as i32, (lparam.0 >> 16) as i16 as i32);
            fire_window_event(WindowEvent::Moved(PhysicalPosition::new(x, y)));
        }
        // Minimized is not a size anything should lay out for: the same size comes back with the window.
        WM_SIZE if wparam.0 as u32 != SIZE_MINIMIZED => {
            let (width, height) = (lparam.0 as u16 as u32, (lparam.0 >> 16) as u16 as u32);
            fire_window_event(WindowEvent::Resized(PhysicalSize::new(width, height)));
        }
        // A page's frame is logical, and the pane's rectangle doesn't change when only the monitor did, so nobody else
        // says so. By now the window has its size at the new scale.
        WM_DPICHANGED => {
            for page in all_native_pages() {
                if let Err(error) = place(&page) {
                    eprintln!("{error}");
                }
            }
            if let Some(scale) = scale_factor_of(window) {
                let new_inner_size = client_size(window).unwrap_or_default();
                fire_window_event(WindowEvent::ScaleFactorChanged { scale_factor: scale, new_inner_size });
            }
        }
        WM_SETFOCUS => fire_window_event(WindowEvent::Focused(true)),
        WM_KILLFOCUS => fire_window_event(WindowEvent::Focused(false)),
        _ => {}
    });
    result
}

fn take_focus_back(window: HWND) {
    if NATIVE.with(|native| native.borrow().is_empty()) {
        return;
    }
    // SAFETY: both are about this thread's focus, on the window of this thread that was just clicked.
    unsafe {
        if GetFocus() != window {
            let _ = SetFocus(Some(window));
        }
    }
}

fn scale_factor_of(window: HWND) -> Option<f64> {
    // SAFETY: only reads; any thread may ask, and a stale handle answers 0.
    let dpi = unsafe { GetDpiForWindow(window) };
    (dpi != 0).then(|| dpi as f64 / 96.0)
}

fn client_size(window: HWND) -> Option<PhysicalSize<u32>> {
    let mut rect = RECT::default();
    // SAFETY: only reads; any thread may ask.
    unsafe { GetClientRect(window, &mut rect) }.ok()?;
    Some(PhysicalSize::new((rect.right - rect.left).max(0) as u32, (rect.bottom - rect.top).max(0) as u32))
}

/// The client's window. There is only the one, called what the host calls its main window. Its getters only read the
/// handle, which is allowed from any thread, so they answer even while the main thread is busy.
#[derive(Clone)]
pub struct Window {
    app: AppHandle<Wry>,
    label: String,
}

impl Window {
    pub fn label(&self) -> &str {
        &self.label
    }

    fn handle(&self) -> Result<HWND> {
        main_window().ok_or_else(|| Error::Message(no_window()))
    }

    pub fn scale_factor(&self) -> Result<f64> {
        scale_factor_of(self.handle()?).ok_or_else(|| Error::Message(no_window()))
    }

    pub fn inner_size(&self) -> Result<PhysicalSize<u32>> {
        client_size(self.handle()?).ok_or_else(|| Error::Message(no_window()))
    }

    pub fn outer_position(&self) -> Result<PhysicalPosition<i32>> {
        let mut rect = RECT::default();
        // SAFETY: only reads; any thread may ask.
        unsafe { GetWindowRect(self.handle()?, &mut rect) }.map_err(|error| Error::Message(describe("read the window's position", error)))?;
        Ok(PhysicalPosition::new(rect.left, rect.top))
    }

    pub fn inner_position(&self) -> Result<PhysicalPosition<i32>> {
        let mut origin = POINT::default();
        // SAFETY: only reads; any thread may ask.
        if !unsafe { ClientToScreen(self.handle()?, &mut origin) }.as_bool() {
            return Err(Error::Message(no_window()));
        }
        Ok(PhysicalPosition::new(origin.x, origin.y))
    }

    pub fn is_visible(&self) -> Result<bool> {
        // SAFETY: only reads; any thread may ask.
        Ok(unsafe { IsWindowVisible(self.handle()?) }.as_bool())
    }

    pub fn is_minimized(&self) -> Result<bool> {
        // SAFETY: only reads; any thread may ask.
        Ok(unsafe { IsIconic(self.handle()?) }.as_bool())
    }

    pub fn hwnd(&self) -> Result<usize> {
        Ok(self.handle()?.0 as usize)
    }

    /// Listeners live as long as the window. They run on the main thread, from the window's own messages.
    pub fn on_window_event(&self, f: impl Fn(&WindowEvent) + Send + 'static) {
        let add = move || WINDOW_LISTENERS.with(|listeners| listeners.borrow_mut().push(Rc::new(f)));
        if is_main_thread() {
            add();
        } else if let Err(error) = post_main(add) {
            eprintln!("{error}");
        }
    }

    pub fn add_child(&self, builder: WebviewBuilder<Wry>, position: impl Into<Position>, size: impl Into<Size>) -> Result<Webview> {
        add_child(self, builder, position.into(), size.into()).map_err(Error::Message)
    }

    /// The window outlives every page, and belongs to the client.
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

    pub fn app_handle(&self) -> &AppHandle<Wry> {
        &self.app
    }
}

/// Only the client's window exists: a detached browser window (the host's other label) has nothing to be, as on macOS, and
/// `WindowBuilder::build` says so if the host asks for one because there is no main window.
pub(crate) fn get_window(app: &AppHandle<Wry>, label: &str) -> Option<Window> {
    (label == "main" && main_window().is_some()).then(|| Window { app: app.clone(), label: label.to_owned() })
}

// ----- the registries -----------------------------------------------------------------------------

/// Live pages by label. A label that resolves here is a page that still exists.
static PAGES: Mutex<Option<HashMap<String, Arc<PageShared>>>> = Mutex::new(None);

fn pages() -> MutexGuard<'static, Option<HashMap<String, Arc<PageShared>>>> {
    lock(&PAGES)
}

pub(crate) fn get_webview(_app: &AppHandle<Wry>, label: &str) -> Option<Webview> {
    pages().as_ref().and_then(|pages| pages.get(label).cloned()).map(|shared| Webview { shared })
}

/// Main-thread half of a page.
struct NativePage {
    shared: Arc<PageShared>,
    handles: NativeHandles,
    environment_key: EnvironmentKey,
    callbacks: Rc<PageCallbacks>,
}

/// The COM side of a page, cloned out of the registry for each call. Never hold the registry borrow across a WebView2 call:
/// closing a page can re-enter.
#[derive(Clone)]
struct NativeHandles {
    shared: Arc<PageShared>,
    controller: ICoreWebView2Controller,
    core: ICoreWebView2,
    environment: ICoreWebView2Environment,
    container: HWND,
    navigations: Rc<PageNavigations>,
}

thread_local! {
    static NATIVE: &'static RefCell<HashMap<String, NativePage>> = Box::leak(Box::default());
    static ENVIRONMENTS: &'static RefCell<HashMap<EnvironmentKey, EnvironmentSlot>> = Box::leak(Box::default());
    static NEXT_ATTEMPT: Cell<u64> = const { Cell::new(0) };
    /// Inside a WebView2 event or completion, where the runtime forbids a nested message loop and closing is unsafe.
    static CALLBACK_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Set around this file's own `ShowWindow`, which may restack the container on its way; see `container_proc`.
    static SHOWING_CONTAINER: Cell<bool> = const { Cell::new(false) };
}

fn native_handles(label: &str) -> Option<NativeHandles> {
    NATIVE.with(|native| native.borrow().get(label).map(|page| page.handles.clone()))
}

fn all_native_pages() -> Vec<NativeHandles> {
    NATIVE.with(|native| native.borrow().values().map(|page| page.handles.clone()).collect())
}

/// Runs `f` as a WebView2 callback of this page. A page already closed gets no more of them.
fn with_callbacks<T>(shared: &Arc<PageShared>, f: impl FnOnce(&PageCallbacks) -> T) -> Option<T> {
    let callbacks = NATIVE.with(|native| native.borrow().get(&shared.label).filter(|page| Arc::ptr_eq(&page.shared, shared)).map(|page| page.callbacks.clone()))?;
    in_callback(|| f(&callbacks))
}

fn in_callback<T>(f: impl FnOnce() -> T) -> Option<T> {
    CALLBACK_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let result = guarded("a page callback", f);
    CALLBACK_DEPTH.with(|depth| depth.set(depth.get() - 1));
    result
}

// ----- environments -------------------------------------------------------------------------------

/// WebView2 takes its user data folder and browser switches per environment, and refuses a second environment on a folder
/// with other switches. The host gives each tab a folder of its own, and the proxy of a remote workspace goes in the switches.
#[derive(Clone, PartialEq, Eq, Hash)]
struct EnvironmentKey {
    directory: PathBuf,
    args: String,
}

type EnvironmentWaiter = Box<dyn FnOnce(std::result::Result<ICoreWebView2Environment, String>)>;

enum EnvironmentState {
    Creating { waiters: Vec<EnvironmentWaiter>, started: Instant, attempt: u64 },
    Ready(ICoreWebView2Environment),
}

/// Held only while a page uses it. The browser process goes with the last page, so an environment kept for later would have
/// nothing behind it, and a later page gets a new one.
struct EnvironmentSlot {
    users: usize,
    state: EnvironmentState,
}

fn acquire_environment(key: &EnvironmentKey, waiter: EnvironmentWaiter) {
    let mut waiter = Some(waiter);
    let attempt = NEXT_ATTEMPT.with(|next| {
        next.set(next.get() + 1);
        next.get()
    });
    let (ready, create) = ENVIRONMENTS.with(|environments| {
        let mut environments = environments.borrow_mut();
        let fresh = |waiter: Option<EnvironmentWaiter>| EnvironmentSlot {
            users: 1,
            state: EnvironmentState::Creating { waiters: waiter.into_iter().collect(), started: Instant::now(), attempt },
        };
        match environments.get_mut(key) {
            None => {
                environments.insert(key.clone(), fresh(waiter.take()));
                (None, true)
            }
            Some(slot) => match &mut slot.state {
                EnvironmentState::Ready(environment) => {
                    slot.users += 1;
                    (Some(environment.clone()), false)
                }
                EnvironmentState::Creating { waiters, started, .. } if started.elapsed() < CREATION_TIMEOUT => {
                    slot.users += 1;
                    waiters.extend(waiter.take());
                    (None, false)
                }
                // WebView2 never answered. Everyone waiting has given up by now, and a late answer finds another attempt.
                EnvironmentState::Creating { .. } => {
                    *slot = fresh(waiter.take());
                    (None, true)
                }
            },
        }
    });
    if let (Some(environment), Some(waiter)) = (ready, waiter) {
        waiter(Ok(environment));
    }
    if create {
        create_environment(key.clone(), attempt);
    }
}

fn release_environment(key: &EnvironmentKey) {
    let released = ENVIRONMENTS.with(|environments| {
        let mut environments = environments.borrow_mut();
        let slot = environments.get_mut(key)?;
        slot.users = slot.users.saturating_sub(1);
        if slot.users == 0 {
            environments.remove(key)
        } else {
            None
        }
    });
    drop(released);
}

fn system_language() -> String {
    let mut name = [0u16; MAX_LOCALE_NAME as usize];
    // SAFETY: into a buffer of the documented size.
    let length = unsafe { LCIDToLocaleName(GetUserDefaultUILanguage() as u32, Some(&mut name), LOCALE_ALLOW_NEUTRAL_NAMES) };
    String::from_utf16_lossy(&name[..(length.max(1) as usize - 1).min(name.len())])
}

/// Asks the loader first, for a message that says what to do about it. An override such as
/// `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER` counts; it's what the loader will use too.
fn require_runtime() -> std::result::Result<(), String> {
    let mut version = PWSTR::null();
    // SAFETY: the out-parameter is a string we take ownership of.
    let result = unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) };
    let version = take_pwstr(version);
    if result.is_err() || version.is_empty() {
        return Err(RUNTIME_MISSING.to_owned());
    }
    Ok(())
}

fn create_environment(key: EnvironmentKey, attempt: u64) {
    let settled_key = key.clone();
    let started = (|| -> std::result::Result<(), String> {
        require_runtime()?;
        let options = CoreWebView2EnvironmentOptions::default();
        // SAFETY: nothing else has this object yet.
        unsafe {
            options.set_additional_browser_arguments(key.args.clone());
            options.set_language(system_language());
        }
        let options: ICoreWebView2EnvironmentOptions = options.into();
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |code, environment| {
            let result = code.map_err(|error| describe("create the WebView2 environment", error)).and_then(|_| environment.ok_or_else(|| "WebView2 created no environment".to_owned()));
            guarded("the WebView2 environment arriving", || environment_settled(&settled_key, attempt, result));
            Ok(())
        }));
        // SAFETY: on the main thread; the arguments outlive the call, and the loader copies what it keeps.
        unsafe { CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &HSTRING::from(key.directory.as_path()), &options, &handler) }
            .map_err(|error| describe("create the WebView2 environment", error))
    })();
    if let Err(error) = started {
        environment_settled(&key, attempt, Err(error));
    }
}

fn environment_settled(key: &EnvironmentKey, attempt: u64, result: std::result::Result<ICoreWebView2Environment, String>) {
    let waiters = ENVIRONMENTS.with(|environments| {
        let mut environments = environments.borrow_mut();
        let slot = environments.get_mut(key)?;
        let EnvironmentState::Creating { waiters, attempt: current, .. } = &mut slot.state else {
            return None;
        };
        if *current != attempt {
            return None;
        }
        let waiters = std::mem::take(waiters);
        match &result {
            Ok(environment) => slot.state = EnvironmentState::Ready(environment.clone()),
            Err(_) => {
                environments.remove(key);
            }
        }
        Some(waiters)
    });
    for waiter in waiters.into_iter().flatten() {
        waiter(result.clone());
    }
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

pub struct DownloadEvent {
    url: String,
}

impl DownloadEvent {
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Given to `with_webview` closures, on the main thread. It holds COM objects, so it stays there.
pub struct PlatformWebview {
    controller: ICoreWebView2Controller,
    environment: ICoreWebView2Environment,
}

impl PlatformWebview {
    pub fn controller(&self) -> ICoreWebView2Controller {
        self.controller.clone()
    }

    pub fn environment(&self) -> ICoreWebView2Environment {
        self.environment.clone()
    }
}

type NavigationCallback = Box<dyn Fn(&url::Url) -> bool + Send>;
type NewWindowCallback = Box<dyn Fn(url::Url, NewWindowFeatures) -> NewWindowResponse + Send>;
type DownloadCallback = Box<dyn Fn(Webview, DownloadEvent) -> bool + Send>;
type PageLoadCallback = Box<dyn Fn(Webview, PageLoadPayload) + Send>;
type TitleCallback = Box<dyn Fn(Webview, String) + Send>;

/// Kept in the page's registry entry rather than the handlers WebView2 holds, so that closing a page lets go of what the
/// callbacks captured, however long the runtime holds on to its handlers.
#[derive(Default)]
struct PageCallbacks {
    navigation: Option<NavigationCallback>,
    new_window: Option<NewWindowCallback>,
    download: Option<DownloadCallback>,
    page_load: Option<PageLoadCallback>,
    title: Option<TitleCallback>,
}

struct PageSettings {
    user_agent: Option<String>,
    autofill: bool,
    transparent: bool,
    devtools: bool,
    incognito: bool,
    zoom_hotkeys: bool,
}

/// `WebviewBuilder`'s surface as `browser.rs` uses it.
pub struct WebviewBuilder<R = Wry> {
    label: String,
    url: WebviewUrl,
    data_directory: Option<PathBuf>,
    browser_args: Option<String>,
    scripts: Vec<String>,
    callbacks: PageCallbacks,
    settings: PageSettings,
    runtime: PhantomData<fn() -> R>,
}

impl WebviewBuilder<Wry> {
    pub fn new(label: impl Into<String>, url: WebviewUrl) -> Self {
        Self {
            label: label.into(),
            url,
            data_directory: None,
            browser_args: None,
            scripts: Vec::new(),
            callbacks: PageCallbacks::default(),
            // wry's defaults.
            settings: PageSettings { user_agent: None, autofill: true, transparent: false, devtools: cfg!(debug_assertions), incognito: false, zoom_hotkeys: false },
            runtime: PhantomData,
        }
    }

    pub fn on_navigation(mut self, f: impl Fn(&url::Url) -> bool + Send + 'static) -> Self {
        self.callbacks.navigation = Some(Box::new(f));
        self
    }

    pub fn on_new_window(mut self, f: impl Fn(url::Url, NewWindowFeatures) -> NewWindowResponse + Send + 'static) -> Self {
        self.callbacks.new_window = Some(Box::new(f));
        self
    }

    pub fn on_page_load(mut self, f: impl Fn(Webview, PageLoadPayload) + Send + 'static) -> Self {
        self.callbacks.page_load = Some(Box::new(f));
        self
    }

    pub fn on_document_title_changed(mut self, f: impl Fn(Webview, String) + Send + 'static) -> Self {
        self.callbacks.title = Some(Box::new(f));
        self
    }

    pub fn on_download(mut self, f: impl Fn(Webview, DownloadEvent) -> bool + Send + 'static) -> Self {
        self.callbacks.download = Some(Box::new(f));
        self
    }

    pub fn general_autofill_enabled(mut self, on: bool) -> Self {
        self.settings.autofill = on;
        self
    }

    pub fn initialization_script(mut self, script: impl Into<String>) -> Self {
        self.scripts.push(script.into());
        self
    }

    pub fn data_directory(mut self, path: PathBuf) -> Self {
        self.data_directory = Some(path);
        self
    }

    pub fn additional_browser_args(mut self, args: &str) -> Self {
        self.browser_args = Some(args.to_owned());
        self
    }

    pub fn user_agent(mut self, agent: &str) -> Self {
        self.settings.user_agent = Some(agent.to_owned());
        self
    }

    /// Already the case. Nothing here takes a page's external drops, which is also what a page got with wry's default
    /// handler: dropped files stop at the page, so none of them reaches a remote document or navigates it to `file:`.
    pub fn disable_drag_drop_handler(self) -> Self {
        self
    }

    pub fn transparent(mut self, on: bool) -> Self {
        self.settings.transparent = on;
        self
    }

    pub fn devtools(mut self, on: bool) -> Self {
        self.settings.devtools = on;
        self
    }

    pub fn incognito(mut self, on: bool) -> Self {
        self.settings.incognito = on;
        self
    }

    pub fn zoom_hotkeys_enabled(mut self, on: bool) -> Self {
        self.settings.zoom_hotkeys = on;
        self
    }

    /// The host places every page itself, from the pane's rectangle, and doesn't call this.
    pub fn auto_resize(self) -> Self {
        self
    }
}

// ----- shared page state ------------------------------------------------------------------------------

/// Logical-pixel frame of the page within its window, top-left origin. Logical so that it survives a DPI change, when the
/// pane's rectangle doesn't change and nobody tells the page where it goes.
#[derive(Clone, Copy, Debug)]
struct LogicalFrame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

const CREATION_PENDING: u8 = 0;
const CREATION_PUBLISHED: u8 = 1;
const CREATION_ABANDONED: u8 = 2;

struct PageShared {
    label: String,
    app: AppHandle<Wry>,
    window: Window,
    frame: Mutex<LogicalFrame>,
    /// `show`/`hide`.
    shown: AtomicBool,
    /// `set_window_hidden`: the client's window can't be seen, so neither can the page, whatever `shown` says.
    window_hidden: AtomicBool,
    /// Whether the caller of `add_child` got this page or gave up on it; whichever comes first wins.
    creation: AtomicU8,
}

impl PageShared {
    fn handle(self: &Arc<Self>) -> Webview {
        Webview { shared: self.clone() }
    }

    fn scale_factor(&self) -> f64 {
        self.window.scale_factor().unwrap_or(1.0).max(f64::EPSILON)
    }

    /// The frame in physical pixels, `(x, y, width, height)`.
    fn physical_frame(&self) -> (i32, i32, i32, i32) {
        let frame = *lock(&self.frame);
        let scale = self.scale_factor();
        ((frame.x * scale).round() as i32, (frame.y * scale).round() as i32, (frame.width.max(0.0) * scale).round() as i32, (frame.height.max(0.0) * scale).round() as i32)
    }
}

/// Puts a page where its frame says.
fn place(page: &NativeHandles) -> std::result::Result<(), String> {
    let (x, y, width, height) = page.shared.physical_frame();
    // SAFETY: on the main thread with a live controller and its container, which the controller fills.
    unsafe {
        SetWindowPos(page.container, None, x, y, width, height, SWP_NOACTIVATE | SWP_NOZORDER).map_err(|error| describe("move the page", error))?;
        page.controller.SetBounds(RECT { left: 0, top: 0, right: width, bottom: height }).map_err(|error| describe("size the page", error))?;
        page.controller.NotifyParentWindowPositionChanged().map_err(|error| describe("move the page", error))
    }
}

/// `shown` is the container and the controller, as wry's `set_visible`. `window_hidden` is the controller alone: its page
/// throttles, and comes back where it was the moment the window can be seen again.
fn apply_visibility(page: &NativeHandles) -> std::result::Result<(), String> {
    let shown = page.shared.shown.load(Ordering::SeqCst);
    let visible = shown && !page.shared.window_hidden.load(Ordering::SeqCst);
    SHOWING_CONTAINER.with(|showing| showing.set(true));
    // SAFETY: on the main thread with a live container. The result is whether it was visible before, not an error.
    let _ = unsafe { ShowWindow(page.container, if shown { SW_SHOWNA } else { SW_HIDE }) };
    SHOWING_CONTAINER.with(|showing| showing.set(false));
    // SAFETY: on the main thread with a live controller.
    unsafe { page.controller.SetIsVisible(visible) }.map_err(|error| describe("show or hide the page", error))
}

// ----- the page handle ----------------------------------------------------------------------------------

/// A live page, addressed the way `browser.rs` addresses a Tauri webview. Cheap to clone, and free to cross threads.
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

    /// Runs `operation` on the main thread with the page's COM objects, and waits for its answer.
    fn with_native<T: Send + 'static>(&self, stage: &'static str, operation: impl FnOnce(NativeHandles) -> std::result::Result<T, String> + Send + 'static) -> Result<T> {
        let label = self.shared.label.clone();
        on_main(stage, move || {
            let page = native_handles(&label).ok_or_else(|| format!("{stage}: the page is closed"))?;
            operation(page)
        })
        .and_then(|result| result)
        .map_err(Error::Message)
    }

    /// Closes the controller at once. The label stops resolving before this returns, so the host's wait for that is short,
    /// and the browser process goes when the environment's last reference does.
    pub fn close(&self) -> Result<()> {
        let shared = self.shared.clone();
        on_main("close the page", move || close_native(&shared)).map_err(Error::Message)
    }

    pub fn show(&self) -> Result<()> {
        self.with_native("show the page", |page| {
            page.shared.shown.store(true, Ordering::SeqCst);
            apply_visibility(&page)
        })
    }

    pub fn hide(&self) -> Result<()> {
        self.with_native("hide the page", |page| {
            page.shared.shown.store(false, Ordering::SeqCst);
            apply_visibility(&page)
        })
    }

    pub fn set_window_hidden(&self, hidden: bool) -> Result<()> {
        self.with_native("hide the page with its window", move |page| {
            page.shared.window_hidden.store(hidden, Ordering::SeqCst);
            apply_visibility(&page)
        })
    }

    pub fn set_focus(&self) -> Result<()> {
        self.with_native("focus the page", |page| {
            // SAFETY: on the main thread with a live controller.
            unsafe { page.controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) }.map_err(|error| describe("focus the page", error))
        })
    }

    pub fn set_position(&self, position: impl Into<Position>) -> Result<()> {
        let position = position.into().to_logical_at(self.shared.scale_factor());
        {
            let mut frame = lock(&self.shared.frame);
            frame.x = position.x;
            frame.y = position.y;
        }
        self.with_native("move the page", |page| place(&page))
    }

    pub fn set_size(&self, size: impl Into<Size>) -> Result<()> {
        let size = size.into().to_logical_at(self.shared.scale_factor());
        {
            let mut frame = lock(&self.shared.frame);
            frame.width = size.width;
            frame.height = size.height;
        }
        self.with_native("size the page", |page| place(&page))
    }

    pub fn position(&self) -> Result<PhysicalPosition<i32>> {
        let (x, y, _, _) = self.shared.physical_frame();
        Ok(PhysicalPosition::new(x, y))
    }

    pub fn size(&self) -> Result<PhysicalSize<u32>> {
        let (_, _, width, height) = self.shared.physical_frame();
        Ok(PhysicalSize::new(width as u32, height as u32))
    }

    pub fn url(&self) -> Result<url::Url> {
        let source = self.with_native("read the page's address", |page| Ok(page.navigations.address(&page.core, None)))?;
        url::Url::parse(&source).map_err(|error| Error::Message(format!("the page's address is invalid: {error}")))
    }

    pub fn navigate(&self, url: url::Url) -> Result<()> {
        self.with_native("navigate the page", move |page| {
            // SAFETY: on the main thread with a live page.
            unsafe { page.core.Navigate(&HSTRING::from(url.as_str())) }.map_err(|error| describe("navigate the page", error))
        })
    }

    pub fn reload(&self) -> Result<()> {
        self.with_native("reload the page", |page| {
            // SAFETY: on the main thread with a live page.
            unsafe { page.core.Reload() }.map_err(|error| describe("reload the page", error))
        })
    }

    pub fn eval(&self, script: impl Into<String>) -> Result<()> {
        self.eval_with_callback(script, |_| {})
    }

    /// Evaluates `script` in the main frame and hands `callback` the JSON of its value: `null` for `undefined`, a thrown
    /// exception or a value that can't be serialized. Never waits; the callback runs on the main thread when the page
    /// answers, which a page may never do. A callback that can't run any more is dropped, which is how a caller hears of it.
    pub fn eval_with_callback(&self, script: impl Into<String>, callback: impl FnOnce(String) + Send + 'static) -> Result<()> {
        let script = script.into();
        let label = self.shared.label.clone();
        let run = move || {
            let page = native_handles(&label).ok_or_else(|| format!("run a script in {label}: the page is closed"))?;
            let handler = ExecuteScriptCompletedHandler::create(Box::new(move |_code, value| {
                in_callback(|| callback(value));
                Ok(())
            }));
            // SAFETY: on the main thread with a live page; the runtime holds the handler until it answers.
            unsafe { page.core.ExecuteScript(&HSTRING::from(script), &handler) }.map_err(|error| describe("run a script", error))
        };
        if is_main_thread() {
            run().map_err(Error::Message)
        } else {
            post_main(move || {
                if let Err(error) = run() {
                    eprintln!("{error}");
                }
            })
            .map_err(Error::Message)
        }
    }

    /// Inline on the main thread, queued elsewhere. Neither waits for an answer.
    pub fn run_on_main_thread(&self, f: impl FnOnce() + Send + 'static) -> Result<()> {
        if is_main_thread() {
            f();
            Ok(())
        } else {
            post_main(f).map_err(Error::Message)
        }
    }

    /// `f` on the main thread with the page's platform objects. If the page has closed by the time it gets there, `f` is
    /// dropped unrun, and whatever it captured is how its caller finds out.
    pub fn with_webview(&self, f: impl FnOnce(PlatformWebview) + Send + 'static) -> Result<()> {
        let label = self.shared.label.clone();
        self.run_on_main_thread(move || {
            if let Some(page) = native_handles(&label) {
                f(PlatformWebview { controller: page.controller, environment: page.environment });
            }
        })
    }

    pub fn set_zoom(&self, factor: f64) -> Result<()> {
        if !factor.is_finite() || factor <= 0.0 {
            return Err(Error::Message("invalid page zoom".to_owned()));
        }
        self.with_native("zoom the page", move |page| {
            // SAFETY: on the main thread with a live controller.
            unsafe { page.controller.SetZoomFactor(factor) }.map_err(|error| describe("zoom the page", error))
        })
    }

    pub fn open_devtools(&self) {
        if let Err(error) = self.with_native("open DevTools", |page| {
            // SAFETY: on the main thread with a live page.
            unsafe { page.core.OpenDevToolsWindow() }.map_err(|error| describe("open DevTools", error))
        }) {
            eprintln!("{error}");
        }
    }

    /// WebView2 has an API to open DevTools and none to close it or ask. Its window belongs to the page's browser process, so
    /// that's where to find it; with a profile per tab, which is how the host makes them, that process has only this page.
    pub fn close_devtools(&self) {
        let closed = self.with_native("close DevTools", |page| {
            for window in devtools_windows(&page.core) {
                // SAFETY: asks a window of the page's own browser process to close, as its close button would.
                let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
            Ok(())
        });
        if let Err(error) = closed {
            eprintln!("{error}");
        }
    }

    pub fn is_devtools_open(&self) -> bool {
        self.with_native("read whether DevTools is open", |page| Ok(!devtools_windows(&page.core).is_empty())).unwrap_or(false)
    }

    pub fn clear_all_browsing_data(&self) -> Result<()> {
        self.with_native("clear the page's browsing data", |page| {
            // SAFETY: on the main thread with a live page.
            unsafe {
                page.core
                    .cast::<ICoreWebView2_13>()
                    .and_then(|core| core.Profile())
                    .and_then(|profile| profile.cast::<ICoreWebView2Profile2>())
                    .and_then(|profile| profile.ClearBrowsingDataAll(&ClearBrowsingDataCompletedHandler::create(Box::new(|_| Ok(())))))
                    .map_err(|error| describe("clear the page's browsing data", error))
            }
        })
    }
}

/// Where a page's navigations were going. `Source` is an empty string on a `data:` document, and the host must not take such
/// a page for `about:blank`, its start page. What `NavigationStarting` was asked for is the address then, by the navigation
/// id the later events share.
#[derive(Default)]
struct PageNavigations {
    starting: RefCell<VecDeque<(u64, String)>>,
    /// The address of the document on screen, when `Source` won't say.
    committed: RefCell<Option<String>>,
}

impl PageNavigations {
    /// Only a few, because a cancelled navigation doesn't have to say it finished. A redirect keeps its id: newest wins.
    fn started(&self, id: u64, uri: String) {
        let mut starting = self.starting.borrow_mut();
        if starting.len() >= 8 {
            starting.pop_front();
        }
        starting.push_back((id, uri));
    }

    fn uri_of(&self, id: u64) -> Option<String> {
        self.starting.borrow().iter().rev().find(|(known, _)| *known == id).map(|(_, uri)| uri.clone())
    }

    /// The page's address, `about:blank` before it has been anywhere. `committing` is the navigation that has just reached
    /// the current document, if one has.
    fn address(&self, core: &ICoreWebView2, committing: Option<u64>) -> String {
        let mut source = PWSTR::null();
        // SAFETY: on the main thread with a live page; the out-parameter is a string we take ownership of.
        let read = unsafe { core.Source(&mut source) };
        let source = take_pwstr(source);
        if read.is_ok() && !source.is_empty() {
            *self.committed.borrow_mut() = None;
            return source;
        }
        if let Some(uri) = committing.and_then(|id| self.uri_of(id)) {
            *self.committed.borrow_mut() = Some(uri);
        }
        self.committed.borrow().clone().unwrap_or_else(|| BLANK_URL.to_owned())
    }
}

/// A browser process's top-level windows with a title bar. Its page contents are children of their containers, and its
/// menus and popups have no caption, which leaves DevTools.
fn devtools_windows(core: &ICoreWebView2) -> Vec<HWND> {
    struct Search {
        process: u32,
        found: Vec<HWND>,
    }
    unsafe extern "system" fn visit(window: HWND, search: LPARAM) -> BOOL {
        // SAFETY: `search` is the local below, for the length of the `EnumWindows` call.
        let search = unsafe { &mut *(search.0 as *mut Search) };
        let mut process = 0;
        // SAFETY: only reads.
        unsafe { GetWindowThreadProcessId(window, Some(&mut process)) };
        if process == search.process {
            // SAFETY: only reads.
            let visible = unsafe { IsWindowVisible(window) }.as_bool();
            // SAFETY: only reads.
            let style = WINDOW_STYLE(unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32);
            if visible && style.contains(WS_CAPTION) {
                search.found.push(window);
            }
        }
        true.into()
    }
    let mut search = Search { process: 0, found: Vec::new() };
    // SAFETY: on the main thread with a live page.
    if unsafe { core.BrowserProcessId(&mut search.process) }.is_err() || search.process == 0 {
        return Vec::new();
    }
    // SAFETY: `visit` runs only during this call, while `search` is alive.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
    search.found
}

/// Main-thread half of [`Webview::close`].
fn close_native(shared: &Arc<PageShared>) {
    {
        let mut pages = pages();
        if let Some(pages) = pages.as_mut() {
            if pages.get(&shared.label).is_some_and(|current| Arc::ptr_eq(current, shared)) {
                pages.remove(&shared.label);
            }
        }
    }
    let native = NATIVE.with(|native| {
        let mut native = native.borrow_mut();
        if native.get(&shared.label).is_some_and(|page| Arc::ptr_eq(&page.shared, shared)) {
            native.remove(&shared.label)
        } else {
            None
        }
    });
    let Some(native) = native else {
        return;
    };
    // The runtime is on the stack inside a callback; it gets its controller closed when it has left.
    if CALLBACK_DEPTH.with(Cell::get) > 0 {
        post_local(move || release_native(native));
    } else {
        release_native(native);
    }
}

fn release_native(page: NativePage) {
    // SAFETY: on the main thread. Close first, as wry does: the controller's window is a child of the container.
    unsafe {
        let _ = page.handles.controller.Close();
        let _ = DestroyWindow(page.handles.container);
    }
    release_environment(&page.environment_key);
}

/// Runs with the client's window going away, which destroys every container anyway.
fn close_all_pages() {
    let all: Vec<_> = NATIVE.with(|native| native.borrow().values().map(|page| page.shared.clone()).collect());
    for shared in all {
        close_native(&shared);
    }
}

// ----- the page's container --------------------------------------------------------------------------

/// A page's own window, which the controller fills.
///
/// `browser_window_region` covers a page by stacking this window at the bottom of its siblings, because upstream's renderer
/// was a sibling webview over the whole window. This client's renderer is the GPU surface of the parent itself, and no child
/// goes beneath its parent: the page would stay in front of everything the renderer draws. What covering has to mean is
/// unchanged though, nothing shows, no pointer input arrives, and Chromium still sees an on-screen window that keeps
/// compositing (moved or hidden it stops, and input then waits on a 15 s timeout, as `AttestedPage::park` explains). An empty
/// window region is exactly that, so the bottom of the z-order is read as one, and the top as none, and the host's own code
/// path stays what it was. Its geometry and its visibility are left alone.
unsafe extern "system" fn container_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_SETFOCUS => {
            // SAFETY: only finds the controller's window, a child of this one on this thread's input queue.
            if let Ok(child) = unsafe { GetWindow(window, GW_CHILD) } {
                // SAFETY: as above.
                let _ = unsafe { SetFocus(Some(child)) };
            }
        }
        WM_WINDOWPOSCHANGING if !SHOWING_CONTAINER.with(Cell::get) => {
            // SAFETY: for this message `lparam` is the system's WINDOWPOS, valid until it returns.
            let change = unsafe { &*(lparam.0 as *const WINDOWPOS) };
            if !change.flags.contains(SWP_NOZORDER) {
                if change.hwndInsertAfter == HWND_BOTTOM {
                    set_page_clipped(window, true);
                } else if change.hwndInsertAfter == HWND_TOP {
                    set_page_clipped(window, false);
                }
            }
        }
        _ => {}
    }
    // SAFETY: the system's default for a window procedure's own arguments.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn set_page_clipped(window: HWND, clipped: bool) {
    // SAFETY: on the thread that owns the window. On success the system owns the region; on failure it stays ours to delete.
    unsafe {
        if clipped {
            let region = CreateRectRgn(0, 0, 0, 0);
            if SetWindowRgn(window, Some(region), true) == 0 {
                let _ = DeleteObject(region.into());
            }
        } else {
            SetWindowRgn(window, None, true);
        }
    }
}

fn create_container(parent: HWND, shared: &PageShared) -> std::result::Result<HWND, String> {
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(container_proc),
        hInstance: module_instance(),
        lpszClassName: w!("MewrkPageContainer"),
        ..Default::default()
    };
    let (x, y, width, height) = shared.physical_frame();
    // SAFETY: on the thread that owns the parent. The class is complete and static, and registering twice fails harmlessly.
    // No parent notification: it would be a message into winit's window procedure, whichever callback is on the stack.
    unsafe {
        RegisterClassExW(&class);
        CreateWindowExW(
            WS_EX_NOPARENTNOTIFY,
            class.lpszClassName,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            x,
            y,
            width,
            height,
            Some(parent),
            None,
            Some(class.hInstance),
            None,
        )
    }
    .map_err(|error| describe("create the page's window", error))
}

// ----- creation ------------------------------------------------------------------------------------------

/// One `add_child` on its way through WebView2's completion handlers, on the main thread. Whoever finds it can't go on calls
/// `fail`, which undoes as far as it got.
struct Creation {
    shared: Arc<PageShared>,
    environment_key: EnvironmentKey,
    url: String,
    settings: PageSettings,
    scripts: RefCell<VecDeque<String>>,
    callbacks: RefCell<Option<PageCallbacks>>,
    reply: RefCell<Option<mpsc::SyncSender<std::result::Result<(), String>>>>,
    has_environment: Cell<bool>,
    container: Cell<Option<HWND>>,
    controller: RefCell<Option<ICoreWebView2Controller>>,
    registered: Cell<bool>,
}

impl Creation {
    fn abandoned(&self) -> bool {
        self.shared.creation.load(Ordering::SeqCst) == CREATION_ABANDONED
    }

    fn check_abandoned(&self) -> std::result::Result<(), String> {
        if self.abandoned() {
            return Err("the caller gave up on this page".to_owned());
        }
        Ok(())
    }

    fn fail(&self, error: String) {
        if self.registered.take() {
            NATIVE.with(|native| native.borrow_mut().remove(&self.shared.label));
        }
        // SAFETY: on the main thread. The controller, if there is one yet, is still only this creation's.
        unsafe {
            if let Some(controller) = self.controller.borrow_mut().take() {
                let _ = controller.Close();
            }
            if let Some(container) = self.container.take() {
                let _ = DestroyWindow(container);
            }
        }
        if self.has_environment.take() {
            release_environment(&self.environment_key);
        }
        if let Some(reply) = self.reply.borrow_mut().take() {
            let _ = reply.try_send(Err(error));
        }
    }
}

fn start_creation(creation: Rc<Creation>) {
    if creation.abandoned() {
        return;
    }
    let key = creation.environment_key.clone();
    acquire_environment(
        &key,
        Box::new(move |environment| {
            let created = environment.and_then(|environment| {
                creation.has_environment.set(true);
                create_controller(&creation, environment)
            });
            if let Err(error) = created {
                creation.fail(error);
            }
        }),
    );
}

fn create_controller(creation: &Rc<Creation>, environment: ICoreWebView2Environment) -> std::result::Result<(), String> {
    creation.check_abandoned()?;
    let parent = main_window().ok_or_else(no_window)?;
    let container = create_container(parent, &creation.shared)?;
    creation.container.set(Some(container));
    let next = creation.clone();
    let page_environment = environment.clone();
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |code, controller| {
        let ready = code
            .map_err(|error| describe("create the WebView2 controller", error))
            .and_then(|_| controller.ok_or_else(|| "WebView2 created no controller".to_owned()))
            .and_then(|controller| guarded("the WebView2 controller arriving", || controller_ready(&next, page_environment, controller)).unwrap_or_else(|| Err("a callback panicked".to_owned())));
        if let Err(error) = ready {
            next.fail(error);
        }
        Ok(())
    }));
    let stage = "create the WebView2 controller";
    // SAFETY: on the main thread, with a window of this thread and an environment of this apartment.
    unsafe {
        let environment = environment.cast::<ICoreWebView2Environment10>().map_err(|error| describe(stage, error))?;
        let options = environment.CreateCoreWebView2ControllerOptions().map_err(|error| describe(stage, error))?;
        options.SetIsInPrivateModeEnabled(creation.settings.incognito).map_err(|error| describe(stage, error))?;
        if creation.settings.transparent {
            // From the first frame, which `apply_settings` is too late for.
            let options = options.cast::<ICoreWebView2ControllerOptions3>().map_err(|error| describe(stage, error))?;
            options.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 0, R: 0, G: 0, B: 0 }).map_err(|error| describe(stage, error))?;
        }
        environment.CreateCoreWebView2ControllerWithOptions(container, &options, &handler).map_err(|error| describe(stage, error))
    }
}

fn controller_ready(creation: &Rc<Creation>, environment: ICoreWebView2Environment, controller: ICoreWebView2Controller) -> std::result::Result<(), String> {
    *creation.controller.borrow_mut() = Some(controller.clone());
    creation.check_abandoned()?;
    let shared = &creation.shared;
    if NATIVE.with(|native| native.borrow().contains_key(&shared.label)) {
        return Err(format!("the page {} already exists", shared.label));
    }
    let container = creation.container.get().ok_or_else(no_window)?;
    // SAFETY: on the main thread, with the controller WebView2 just handed over.
    let core = unsafe { controller.CoreWebView2() }.map_err(|error| describe("read the WebView2 core", error))?;
    apply_settings(&core, &controller, &creation.settings).map_err(|error| describe("configure the page", error))?;
    let navigations = Rc::<PageNavigations>::default();
    attach_handlers(shared, &core, creation.callbacks.borrow().as_ref(), &navigations).map_err(|error| describe("configure the page", error))?;
    let handles = NativeHandles { shared: shared.clone(), controller, core: core.clone(), environment, container, navigations };
    place(&handles)?;
    apply_visibility(&handles)?;
    let callbacks = Rc::new(creation.callbacks.borrow_mut().take().unwrap_or_default());
    NATIVE.with(|native| {
        native.borrow_mut().insert(shared.label.clone(), NativePage { shared: shared.clone(), handles, environment_key: creation.environment_key.clone(), callbacks })
    });
    creation.registered.set(true);
    add_next_script(creation, core)
}

/// One at a time, and the first navigation only after the last has registered: no document may run before them.
fn add_next_script(creation: &Rc<Creation>, core: ICoreWebView2) -> std::result::Result<(), String> {
    creation.check_abandoned()?;
    let script = creation.scripts.borrow_mut().pop_front();
    let Some(script) = script else {
        return first_navigation(creation, &core);
    };
    let next = creation.clone();
    let next_core = core.clone();
    let handler = AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(move |code, _id| {
        let added = code
            .map_err(|error| describe("add the page's script", error))
            .and_then(|_| guarded("the page's script registering", || add_next_script(&next, next_core)).unwrap_or_else(|| Err("a callback panicked".to_owned())));
        if let Err(error) = added {
            next.fail(error);
        }
        Ok(())
    }));
    // SAFETY: on the main thread with a live page; the runtime copies the script.
    unsafe { core.AddScriptToExecuteOnDocumentCreated(&HSTRING::from(script), &handler) }.map_err(|error| describe("add the page's script", error))
}

fn first_navigation(creation: &Rc<Creation>, core: &ICoreWebView2) -> std::result::Result<(), String> {
    // SAFETY: on the main thread with a live page.
    unsafe { core.Navigate(&HSTRING::from(creation.url.as_str())) }.map_err(|error| describe("navigate the new page", error))?;
    let shared = &creation.shared;
    if shared.creation.compare_exchange(CREATION_PENDING, CREATION_PUBLISHED, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Err("the caller gave up on this page".to_owned());
    }
    pages().get_or_insert_with(HashMap::new).insert(shared.label.clone(), shared.clone());
    if let Some(reply) = creation.reply.borrow_mut().take() {
        let _ = reply.try_send(Ok(()));
    }
    Ok(())
}

fn apply_settings(core: &ICoreWebView2, controller: &ICoreWebView2Controller, settings: &PageSettings) -> windows::core::Result<()> {
    // SAFETY: on the main thread with a page no document has reached yet. Newer interfaces are optional, as in wry: the
    // loader already refused a runtime too old for what the page's safety depends on.
    unsafe {
        let page = core.Settings()?;
        page.SetIsStatusBarEnabled(false)?;
        page.SetIsZoomControlEnabled(settings.zoom_hotkeys)?;
        page.SetAreDevToolsEnabled(settings.devtools)?;
        if let Some(agent) = &settings.user_agent {
            page.cast::<ICoreWebView2Settings2>()?.SetUserAgent(&HSTRING::from(agent))?;
        }
        if let Ok(page) = page.cast::<ICoreWebView2Settings4>() {
            page.SetIsGeneralAutofillEnabled(settings.autofill)?;
        }
        if let Ok(page) = page.cast::<ICoreWebView2Settings5>() {
            page.SetIsPinchZoomEnabled(settings.zoom_hotkeys)?;
        }
        if let Ok(page) = page.cast::<ICoreWebView2Settings6>() {
            page.SetIsSwipeNavigationEnabled(false)?;
        }
        if let Ok(controller) = controller.cast::<ICoreWebView2Controller4>() {
            controller.SetAllowExternalDrop(false)?;
        }
        if settings.transparent {
            controller.cast::<ICoreWebView2Controller2>()?.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 0, R: 0, G: 0, B: 0 })?;
        }
    }
    Ok(())
}

/// Registered for the life of the page, so their tokens aren't kept. What each one calls is looked up at the time
/// (`with_callbacks`). `window.close()` is left to do nothing: the host decides when a page closes.
fn attach_handlers(shared: &Arc<PageShared>, core: &ICoreWebView2, callbacks: Option<&PageCallbacks>, navigations: &Rc<PageNavigations>) -> windows::core::Result<()> {
    let Some(callbacks) = callbacks else {
        return Ok(());
    };
    let mut token = 0i64;
    // SAFETY: on the main thread with a live page, which holds each handler until it goes.
    unsafe {
        {
            let shared = shared.clone();
            let navigations = navigations.clone();
            let has_policy = callbacks.navigation.is_some();
            core.add_NavigationStarting(
                &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else {
                        return Ok(());
                    };
                    let (mut uri, mut id) = (PWSTR::null(), 0u64);
                    args.Uri(&mut uri)?;
                    args.NavigationId(&mut id)?;
                    let uri = take_pwstr(uri);
                    navigations.started(id, uri.clone());
                    if has_policy {
                        // An address that doesn't parse, and a page closing, are refused like a policy would.
                        let allowed = url::Url::parse(&uri).ok().and_then(|url| with_callbacks(&shared, |callbacks| callbacks.navigation.as_ref().is_none_or(|allow| allow(&url))));
                        args.SetCancel(!allowed.unwrap_or(false))?;
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }

        // With or without a handler: these are what keeps the page's address.
        {
            let started = shared.clone();
            let started_navigations = navigations.clone();
            core.add_ContentLoading(
                &ContentLoadingEventHandler::create(Box::new(move |core, args| {
                    if let (Some(core), Some(args)) = (core, args) {
                        let mut id = 0u64;
                        args.NavigationId(&mut id)?;
                        report_page_load(&started, &started_navigations.address(&core, Some(id)), PageLoadEvent::Started);
                    }
                    Ok(())
                })),
                &mut token,
            )?;
            let finished = shared.clone();
            let finished_navigations = navigations.clone();
            core.add_NavigationCompleted(
                &NavigationCompletedEventHandler::create(Box::new(move |core, args| {
                    if let (Some(core), Some(args)) = (core, args) {
                        let (mut id, mut succeeded) = (0u64, BOOL::default());
                        args.NavigationId(&mut id)?;
                        args.IsSuccess(&mut succeeded)?;
                        // A refused navigation finishes too, on the document that was already there.
                        let committing = succeeded.as_bool().then_some(id);
                        report_page_load(&finished, &finished_navigations.address(&core, committing), PageLoadEvent::Finished);
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }

        if callbacks.title.is_some() {
            let shared = shared.clone();
            core.add_DocumentTitleChanged(
                &DocumentTitleChangedEventHandler::create(Box::new(move |core, _| {
                    let Some(core) = core else {
                        return Ok(());
                    };
                    let mut title = PWSTR::null();
                    core.DocumentTitle(&mut title)?;
                    let title = take_pwstr(title);
                    let handle = shared.handle();
                    with_callbacks(&shared, |callbacks| {
                        if let Some(changed) = &callbacks.title {
                            changed(handle, title);
                        }
                    });
                    Ok(())
                })),
                &mut token,
            )?;
        }

        // Without a handler WebView2 would open its own window for a popup, outside everything the host controls. Handled
        // is what keeps it from doing that.
        {
            let shared = shared.clone();
            core.add_NewWindowRequested(
                &NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else {
                        return Ok(());
                    };
                    let mut uri = PWSTR::null();
                    args.Uri(&mut uri)?;
                    let uri = take_pwstr(uri);
                    let response = url::Url::parse(&uri).ok().and_then(|url| with_callbacks(&shared, |callbacks| callbacks.new_window.as_ref().map(|asked| asked(url, NewWindowFeatures)))).flatten();
                    args.SetHandled(!matches!(response, Some(NewWindowResponse::Allow)))?;
                    Ok(())
                })),
                &mut token,
            )?;
        }

        if callbacks.download.is_some() {
            let shared = shared.clone();
            // The host's policy is to refuse every download, and this is the only thing enforcing it: no handler, no page.
            core.cast::<ICoreWebView2_4>()?.add_DownloadStarting(
                &DownloadStartingEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else {
                        return Ok(());
                    };
                    let mut uri = PWSTR::null();
                    args.DownloadOperation()?.Uri(&mut uri)?;
                    let event = DownloadEvent { url: take_pwstr(uri) };
                    let handle = shared.handle();
                    let allowed = with_callbacks(&shared, |callbacks| callbacks.download.as_ref().is_some_and(|allow| allow(handle, event)));
                    args.SetCancel(!allowed.unwrap_or(false))?;
                    Ok(())
                })),
                &mut token,
            )?;
        }
    }
    Ok(())
}

fn report_page_load(shared: &Arc<PageShared>, address: &str, event: PageLoadEvent) {
    let Ok(url) = url::Url::parse(address) else {
        return;
    };
    let handle = shared.handle();
    with_callbacks(shared, |callbacks| {
        if let Some(loaded) = &callbacks.page_load {
            loaded(handle, PageLoadPayload { url, event });
        }
    });
}

/// Creates the page as a child of the client's window, and returns once its scripts are registered and its first navigation
/// has been issued, the moment real Tauri's `add_child` returned.
fn add_child(window: &Window, builder: WebviewBuilder<Wry>, position: Position, size: Size) -> std::result::Result<Webview, String> {
    main_window().ok_or_else(no_window)?;
    let on_main_thread = is_main_thread();
    if on_main_thread && CALLBACK_DEPTH.with(Cell::get) > 0 {
        return Err("a page can't be created from inside a WebView2 callback, where the runtime forbids waiting for it".to_owned());
    }
    let scale = window.scale_factor().map_err(|error| error.to_string())?;
    let position = position.to_logical_at(scale);
    let size = size.to_logical_at(scale);
    let WebviewBuilder { label, url, data_directory, browser_args, scripts, callbacks, settings, .. } = builder;
    if pages().as_ref().is_some_and(|pages| pages.contains_key(&label)) {
        return Err(format!("the page {label} already exists"));
    }
    let directory = match data_directory {
        Some(directory) => directory,
        // Where Tauri put it: beside the executable, WebView2's own default, may not be writable.
        None => window.app.path().app_local_data_dir().map_err(|error| error.to_string())?.join("EBWebView"),
    };
    let url = match url {
        WebviewUrl::External(url) | WebviewUrl::CustomProtocol(url) => url.to_string(),
        WebviewUrl::App(_) => BLANK_URL.to_owned(),
    };
    let shared = Arc::new(PageShared {
        label,
        app: window.app.clone(),
        window: window.clone(),
        frame: Mutex::new(LogicalFrame { x: position.x, y: position.y, width: size.width, height: size.height }),
        shown: AtomicBool::new(true),
        window_hidden: AtomicBool::new(false),
        creation: AtomicU8::new(CREATION_PENDING),
    });
    let (reply, receiver) = mpsc::sync_channel(1);
    let start = {
        let shared = shared.clone();
        let environment_key = EnvironmentKey { directory, args: browser_args.unwrap_or_else(|| DEFAULT_BROWSER_ARGS.to_owned()) };
        move || {
            start_creation(Rc::new(Creation {
                shared,
                environment_key,
                url,
                settings,
                scripts: RefCell::new(scripts.into()),
                callbacks: RefCell::new(Some(callbacks)),
                reply: RefCell::new(Some(reply)),
                has_environment: Cell::new(false),
                container: Cell::new(None),
                controller: RefCell::new(None),
                registered: Cell::new(false),
            }))
        }
    };
    let waited = if on_main_thread {
        start();
        pump_until(&receiver, CREATION_TIMEOUT)
    } else {
        post_main(start)?;
        receiver.recv_timeout(CREATION_TIMEOUT)
    };
    let result = match waited {
        Ok(result) => result,
        Err(reason) => {
            // Whichever comes first: this giving up, which leaves the creation to undo itself at its next step however late
            // that comes, or the page going live a moment ago, whose answer is then already on its way.
            match shared.creation.compare_exchange(CREATION_PENDING, CREATION_ABANDONED, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => Err(match reason {
                    mpsc::RecvTimeoutError::Timeout => format!("WebView2 未能在 {} 秒内创建页面 (timed out creating the page)", CREATION_TIMEOUT.as_secs()),
                    mpsc::RecvTimeoutError::Disconnected => "WebView2 未能创建页面 (the page's creation was dropped)".to_owned(),
                }),
                Err(_) => Ok(()),
            }
        }
    };
    result.map(|()| shared.handle())
}
