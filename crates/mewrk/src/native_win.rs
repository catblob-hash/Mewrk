//! What Windows needs of the shell beyond winit: the frameless window's resize edges, the notification-area icon, and a few
//! things to ask the system. `native.rs` is the same for macOS.

use std::cell::RefCell;
use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, GetCursorPos, GetWindowRect, IsZoomed, LoadIconW, PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SystemParametersInfoW, TrackPopupMenu, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IDI_APPLICATION, MF_STRING, SPI_GETCLIENTAREAANIMATION, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP, WM_NCCALCSIZE, WM_NCHITTEST, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

/// How far in from the window's edge a press resizes it, in pixels at 96 DPI: the system's own frame is about this wide.
const RESIZE_BORDER: i32 = 6;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

const SUBCLASS_ID: usize = 0x4D57;

/// What was the frame is client area, so nothing else tells Windows where the edges are. A press there is the system's own
/// resize, which snaps and shows the right cursor.
unsafe extern "system" fn frame_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if message == WM_NCHITTEST {
        // SAFETY: `hwnd` is the window this procedure was installed on.
        let hit = unsafe { edge_at(hwnd, lparam) };
        if let Some(hit) = hit {
            return hit as LRESULT;
        }
    }
    // winit leaves the top row of a frameless window to the shadow's glass, through which the window behind shows. A swap chain
    // is opaque, so the client area can have that row too.
    // SAFETY: the caller's window.
    if message == WM_NCCALCSIZE && wparam != 0 && unsafe { IsZoomed(hwnd) } == 0 {
        return 0;
    }
    // SAFETY: passing on the message this procedure was given.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

/// # Safety
/// `hwnd` must be a live window.
unsafe fn edge_at(hwnd: HWND, lparam: LPARAM) -> Option<u32> {
    // SAFETY: the caller's window, and out-parameters that live for the calls.
    unsafe {
        if IsZoomed(hwnd) != 0 {
            return None;
        }
        let mut frame = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetWindowRect(hwnd, &mut frame) == 0 {
            return None;
        }
        let border = RESIZE_BORDER * GetDpiForWindow(hwnd).max(96) as i32 / 96;
        // Signed words: a second monitor can be left of or above the first.
        let (x, y) = ((lparam & 0xffff) as i16 as i32, ((lparam >> 16) & 0xffff) as i16 as i32);
        let (left, right) = (x < frame.left + border, x >= frame.right - border);
        let (top, bottom) = (y < frame.top + border, y >= frame.bottom - border);
        let hit = match (left, right, top, bottom) {
            (true, _, true, _) => HTTOPLEFT,
            (_, true, true, _) => HTTOPRIGHT,
            (true, _, _, true) => HTBOTTOMLEFT,
            (_, true, _, true) => HTBOTTOMRIGHT,
            (true, ..) => HTLEFT,
            (_, true, ..) => HTRIGHT,
            (_, _, true, _) => HTTOP,
            (_, _, _, true) => HTBOTTOM,
            _ => HTCLIENT,
        };
        (hit != HTCLIENT).then_some(hit)
    }
}

/// Gives a frameless window its resize edges.
pub fn install_frame(view: *mut c_void) {
    if view.is_null() {
        return;
    }
    // SAFETY: winit's window, on the thread that owns it. The subclass goes when the window does.
    unsafe {
        SetWindowSubclass(view as HWND, Some(frame_proc), SUBCLASS_ID, 0);
    }
}

/// Where the pointer is over the window, in the window's logical pixels. A drag of files sends the window no moves.
pub fn pointer_in_view(view: *mut c_void) -> Option<(f32, f32)> {
    if view.is_null() {
        return None;
    }
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: the window's own handle, and a point that lives for the calls.
    unsafe {
        if GetCursorPos(&mut point) == 0 || ScreenToClient(view as HWND, &mut point) == 0 {
            return None;
        }
        let scale = GetDpiForWindow(view as HWND).max(96) as f32 / 96.0;
        Some((point.x as f32 / scale, point.y as f32 / scale))
    }
}

/// Whether the system asks for less animation: Settings → Accessibility → Visual effects → Animation effects.
pub fn reduce_motion() -> bool {
    let mut animate: i32 = 1;
    // SAFETY: this action writes one BOOL through the pointer it is given.
    let read = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut animate).cast(), 0) };
    read != 0 && animate == 0
}

/// The peak of the process's working set, in MiB.
pub fn peak_memory_mib() -> f64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain old data the call fills, with its size given, for this process's pseudo-handle.
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) == 0 {
            return 0.0;
        }
        counters.PeakWorkingSetSize as f64 / 1048576.0
    }
}

/// Whoever started this process may have started it ignoring Ctrl+C, as a shell's background job or a new process group is, and
/// that passes down to every child. A program in Mewrk's terminal could then not be interrupted.
pub fn hear_ctrl_c() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // SAFETY: no handler, FALSE: this only clears the inherited flag.
    unsafe {
        SetConsoleCtrlHandler(None, 0);
    }
}

/// A build without a console says nothing to the terminal it was started from, unless it asks to join it. Only for what prints.
pub fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: no pointers; failing only means there is nothing to join.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

// ---------------------------------------------------------------- the notification area

#[derive(Debug)]
pub enum NativeEvent {
    Quit,
    Show,
}

const TRAY_MESSAGE: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;
const COMMAND_SHOW: usize = 1;
const COMMAND_QUIT: usize = 2;

struct TrayState {
    on_event: Box<dyn Fn(NativeEvent)>,
    chinese: bool,
    /// Sent to every top-level window when Explorer starts again, which forgets every icon.
    taskbar_created: u32,
}

thread_local! {
    static TRAY: RefCell<Option<TrayState>> = const { RefCell::new(None) };
}

fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    // SAFETY: an all-zero NOTIFYICONDATAW is valid: no flags, so no field is read.
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_ID;
    data
}

fn add_icon(hwnd: HWND) -> bool {
    let mut data = notify_data(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = TRAY_MESSAGE;
    // SAFETY: the executable's own icon, by the ordinal `assets/mewrk.rc` gives it, or the system's when a test binary has none.
    data.hIcon = unsafe {
        let own = LoadIconW(GetModuleHandleW(std::ptr::null()), 1 as *const u16);
        if own.is_null() { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) } else { own }
    };
    for (slot, unit) in data.szTip.iter_mut().zip("Mewrk".encode_utf16()) {
        *slot = unit;
    }
    // SAFETY: a filled-in structure that lives for the call.
    unsafe { Shell_NotifyIconW(NIM_ADD, &data) != 0 }
}

fn emit(event: NativeEvent) {
    TRAY.with(|tray| {
        if let Some(tray) = tray.borrow().as_ref() {
            (tray.on_event)(event);
        }
    });
}

fn show_menu(hwnd: HWND) {
    let chinese = TRAY.with(|tray| tray.borrow().as_ref().is_some_and(|tray| tray.chinese));
    let (open, quit) = if chinese { ("打开窗口", "关闭 Mewrk") } else { ("Open window", "Quit Mewrk") };
    let (open, quit) = (wide(open), wide(quit));
    // SAFETY: a menu made and destroyed here, around strings that outlive it. Windows wants the window in front for the
    // menu to close on a click elsewhere, and a posted message after it so that the click is not swallowed.
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        AppendMenuW(menu, MF_STRING, COMMAND_SHOW, open.as_ptr());
        AppendMenuW(menu, MF_STRING, COMMAND_QUIT, quit.as_ptr());
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, at.x, at.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match chosen as usize {
            COMMAND_SHOW => emit(NativeEvent::Show),
            COMMAND_QUIT => emit(NativeEvent::Quit),
            _ => {}
        }
    }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == TRAY_MESSAGE {
        match (lparam & 0xffff) as u32 {
            WM_LBUTTONUP => emit(NativeEvent::Show),
            WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
            _ => {}
        }
        return 0;
    }
    if TRAY.with(|tray| tray.borrow().as_ref().is_some_and(|tray| tray.taskbar_created != 0 && tray.taskbar_created == message)) {
        add_icon(hwnd);
        return 0;
    }
    // SAFETY: passing on the message this procedure was given.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// The icon and the hidden window it speaks to. Dropping it takes the icon off the taskbar: Windows would otherwise leave it
/// there until the pointer passes over it.
pub struct NativeShell {
    hwnd: HWND,
    installed: bool,
}

impl NativeShell {
    pub fn tray_installed(&self) -> bool {
        self.installed
    }

    pub fn sync_language(&mut self, chinese: bool) {
        TRAY.with(|tray| {
            if let Some(tray) = tray.borrow_mut().as_mut() {
                tray.chinese = chinese;
            }
        });
    }
}

impl Drop for NativeShell {
    fn drop(&mut self) {
        // SAFETY: the window made in `install`, on the thread that made it.
        unsafe {
            if self.installed {
                let data = notify_data(self.hwnd);
                Shell_NotifyIconW(NIM_DELETE, &data);
            }
            DestroyWindow(self.hwnd);
        }
        TRAY.with(|tray| *tray.borrow_mut() = None);
    }
}

/// `chinese` is the menu's language until `sync_language` says otherwise. `None` if the window could not be made: closing the
/// window then quits, since nothing would bring it back.
pub fn install(on_event: impl Fn(NativeEvent) + 'static, chinese: bool) -> Option<NativeShell> {
    let class = wide("MewrkTray");
    // SAFETY: the class name outlives the calls that read it, and the procedure is a plain function. A second registration
    // fails harmlessly, and creating the window is what decides.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let description = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(tray_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&description);
        // Top-level and never shown: the message that Explorer has restarted only goes to top-level windows.
        CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null())
    };
    if hwnd.is_null() {
        return None;
    }
    let name = wide("TaskbarCreated");
    // SAFETY: a NUL-terminated name that lives for the call.
    let taskbar_created = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    TRAY.with(|tray| *tray.borrow_mut() = Some(TrayState { on_event: Box::new(on_event), chinese, taskbar_created }));
    let installed = add_icon(hwnd);
    Some(NativeShell { hwnd, installed })
}
