//! Runs the WebView2 page engine in a real window, the way the host drives it, and says what it saw.
//!
//!     cargo run -p mewrk-pane-browser --example win_engine -- <screenshot.png> [--slow] [--dx12]
//!
//! It needs a window and the main thread, so it isn't a test. winit runs on the main thread and draws through a wgpu
//! surface as the client does, the first page is created inside a winit callback, and a worker thread drives everything
//! else through the calls `browser.rs` makes. Every step prints PASS or FAIL, and it exits on its own within 60 s. What the
//! pixel checks read off the screen lands beside the screenshot as `*-window.png`. `--slow` adds the main thread being
//! stuck, which takes a minute more. `--dx12` asks for the flip-model swapchain where wgpu would otherwise pick Vulkan.

#[cfg(not(windows))]
fn main() {
    eprintln!("this example needs Windows");
}

#[cfg(windows)]
fn main() {
    driver::main();
}

#[cfg(windows)]
mod driver {
    use std::fmt::Display;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    use base64::Engine as _;
    use serde_json::Value;
    use tauri::webview::{attach_main_view, NewWindowResponse, PageLoadEvent, PlatformWebview, Webview, WebviewBuilder};
    use tauri::{AppConfig, AppHandle, LogicalPosition, LogicalSize, Manager, PhysicalPosition, WebviewUrl, Window};
    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    use webview2_com::{take_pwstr, BrowserProcessExitedEventHandler, CallDevToolsProtocolMethodCompletedHandler};
    use windows::core::{w, Interface, BOOL, HSTRING, PWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, CreateRectRgn, DeleteDC, DeleteObject, GetDC, GetDIBits, GetWindowRgn, InvalidateRect, ReleaseDC, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
    };
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, GetClientRect, GetForegroundWindow, GetWindowRect, PostMessageW, SetWindowPos, ShowWindowAsync, HWND_BOTTOM, HWND_TOP, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_NOZORDER, SW_MINIMIZE, SW_RESTORE, WM_LBUTTONDOWN, WM_LBUTTONUP,
    };
    use winit::application::ApplicationHandler;
    use winit::event::{StartCause, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{WindowId, WindowLevel};

    /// `BROWSER_PAGE_BROWSER_ARGS` of the host: what keeps a covered page running.
    const PAGE_ARGS: &str = "--disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion";
    const PAGE: &str = "<!doctype html><title>Engine fixture</title><style>html,body{margin:0;height:100%;background:#20b070;color:#fff;font:32px sans-serif}</style><h1>WebView2 page engine</h1><script>window.seenByPage=window.fromInit;window.frameCount=0;(function tick(){window.frameCount++;requestAnimationFrame(tick)})();</script>";
    const SERVED: &str = "<!doctype html><title>Served</title><style>html,body{background:#20b070}</style><h1>Served</h1>";

    fn is_page_color([r, g, b]: [u8; 3]) -> bool {
        r.abs_diff(0x20) < 16 && g.abs_diff(0xb0) < 16 && b.abs_diff(0x70) < 16
    }

    fn data_url() -> url::Url {
        let encoded = base64::engine::general_purpose::STANDARD.encode(PAGE);
        url::Url::parse(&format!("data:text/html;base64,{encoded}")).expect("a data URL parses")
    }

    fn blank() -> WebviewUrl {
        WebviewUrl::External(url::Url::parse("about:blank").expect("about:blank parses"))
    }

    // ----- small things the steps share -----------------------------------------------------------

    fn text<E: Display>(error: E) -> String {
        error.to_string()
    }

    fn ensure(condition: bool, what: impl Into<String>) -> Result<(), String> {
        if condition {
            Ok(())
        } else {
            Err(what.into())
        }
    }

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn eval(page: &Webview, script: &str) -> Result<String, String> {
        let (sender, receiver) = mpsc::sync_channel(1);
        page.eval_with_callback(script.to_owned(), move |value| {
            let _ = sender.try_send(value);
        })
        .map_err(text)?;
        receiver.recv_timeout(Duration::from_secs(5)).map_err(|error| format!("{script}: {error}"))
    }

    /// `f` on the main thread with the page's platform objects, as the host's closures run.
    fn on_ui<T: Send + 'static>(page: &Webview, f: impl FnOnce(&PlatformWebview) -> T + Send + 'static) -> Result<T, String> {
        let (sender, receiver) = mpsc::sync_channel(1);
        page.with_webview(move |platform| {
            let _ = sender.try_send(f(&platform));
        })
        .map_err(text)?;
        receiver.recv_timeout(Duration::from_secs(10)).map_err(text)
    }

    fn cdp(page: &Webview, method: &'static str, params: &str) -> Result<String, String> {
        let params = params.to_owned();
        let (sender, receiver) = mpsc::sync_channel(1);
        page.with_webview(move |platform| {
            let completion = sender.clone();
            let sent = (|| -> windows::core::Result<()> {
                // SAFETY: on the main thread, in the closure the page was given.
                unsafe {
                    platform.controller().CoreWebView2()?.CallDevToolsProtocolMethod(
                        &HSTRING::from(method),
                        &HSTRING::from(params),
                        &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |code, json| {
                            let _ = completion.try_send(code.map(|_| json).map_err(text));
                            Ok(())
                        })),
                    )
                }
            })();
            if let Err(error) = sent {
                let _ = sender.try_send(Err(text(error)));
            }
        })
        .map_err(text)?;
        receiver.recv_timeout(Duration::from_secs(10)).map_err(|_| format!("{method} timed out"))?
    }

    /// What the host's `set_page_stacking` does, to the window it does it to. Returns that window.
    fn restack(page: &Webview, bottom: bool) -> Result<isize, String> {
        on_ui(page, move |platform| -> Result<isize, String> {
            let mut parent = HWND::default();
            // SAFETY: on the main thread, in the closure the page was given.
            unsafe {
                platform.controller().ParentWindow(&mut parent).map_err(text)?;
                SetWindowPos(parent, Some(if bottom { HWND_BOTTOM } else { HWND_TOP }), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE).map_err(text)?;
            }
            Ok(parent.0 as isize)
        })?
    }

    /// 1 is an empty region, 0 is no region at all.
    fn region_kind(window: isize) -> i32 {
        // SAFETY: a region of our own, deleted here; only reads the window.
        unsafe {
            let region = CreateRectRgn(0, 0, 0, 0);
            let kind = GetWindowRgn(HWND(window as _), region);
            let _ = DeleteObject(region.into());
            kind.0
        }
    }

    fn frame_count(page: &Webview) -> Result<f64, String> {
        eval(page, "window.frameCount")?.parse().map_err(text)
    }

    fn is_visible(page: &Webview) -> Result<bool, String> {
        on_ui(page, |platform| {
            let mut visible = BOOL::default();
            // SAFETY: on the main thread, in the closure the page was given.
            unsafe { platform.controller().IsVisible(&mut visible) }.map(|_| visible.as_bool()).map_err(text)
        })?
    }

    fn browser_process(page: &Webview) -> Result<u32, String> {
        on_ui(page, |platform| {
            let mut process = 0;
            // SAFETY: on the main thread, in the closure the page was given.
            unsafe { platform.controller().CoreWebView2().and_then(|core| core.BrowserProcessId(&mut process)) }.map(|_| process).map_err(text)
        })?
    }

    fn container_count(main: isize) -> usize {
        let mut count = 0;
        let mut after = None;
        // SAFETY: only walks the children of the window.
        while let Ok(found) = unsafe { FindWindowExW(Some(HWND(main as _)), after, w!("MewrkPageContainer"), None) } {
            count += 1;
            after = Some(found);
        }
        count
    }

    // ----- reading the screen ---------------------------------------------------------------------

    struct Capture {
        width: i32,
        height: i32,
        bgra: Vec<u8>,
    }

    impl Capture {
        /// The client area as DWM composes it, which is the only way to know what a person would see.
        fn of(main: isize) -> Result<Self, String> {
            let window = HWND(main as _);
            // SAFETY: GDI objects made and released here, into a buffer of the size asked for.
            unsafe {
                let mut rect = RECT::default();
                GetClientRect(window, &mut rect).map_err(text)?;
                let mut origin = POINT::default();
                let _ = ClientToScreen(window, &mut origin);
                let (width, height) = (rect.right, rect.bottom);
                let screen = GetDC(None);
                let memory = CreateCompatibleDC(Some(screen));
                let bitmap = CreateCompatibleBitmap(screen, width, height);
                let previous = SelectObject(memory, bitmap.into());
                let blitted = BitBlt(memory, 0, 0, width, height, Some(screen), origin.x, origin.y, SRCCOPY);
                let mut info = BITMAPINFO::default();
                info.bmiHeader = BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                };
                let mut bgra = vec![0u8; width as usize * height as usize * 4];
                let lines = GetDIBits(memory, bitmap, 0, height as u32, Some(bgra.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
                SelectObject(memory, previous);
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(memory);
                ReleaseDC(None, screen);
                blitted.map_err(text)?;
                ensure(lines != 0, "could not read the screen")?;
                Ok(Self { width, height, bgra })
            }
        }

        fn rgb(&self, x: i32, y: i32) -> [u8; 3] {
            let at = (y.clamp(0, self.height - 1) * self.width + x.clamp(0, self.width - 1)) as usize * 4;
            [self.bgra[at + 2], self.bgra[at + 1], self.bgra[at]]
        }

        fn save(&self, path: &Path) -> Result<(), String> {
            let rgba = self.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
            image::RgbaImage::from_raw(self.width as u32, self.height as u32, rgba).ok_or("the capture has the wrong size")?.save(path).map_err(text)
        }
    }

    /// Composition lags a call by a frame or two, so this is a wait. `(x, y)` is logical, within the client area.
    fn pixel_becomes(main: isize, scale: f64, (x, y): (f64, f64), expected: bool, what: &str) -> Result<(), String> {
        let (x, y) = ((x * scale) as i32, (y * scale) as i32);
        let mut last = [0; 3];
        let matched = wait_until(Duration::from_secs(4), || match Capture::of(main) {
            Ok(capture) => {
                last = capture.rgb(x, y);
                is_page_color(last) == expected
            }
            Err(_) => false,
        });
        ensure(matched, format!("{what}: pixel at {x},{y} is {last:?}"))
    }

    // ----- a server that is also a proxy ----------------------------------------------------------

    fn serve(requests: Arc<Mutex<Vec<String>>>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let requests = requests.clone();
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 4096];
                    let Ok(read) = stream.read(&mut buffer) else { return };
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let target = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let (kind, extra, body) = if target.ends_with("/file.bin") {
                        ("application/octet-stream", "Content-Disposition: attachment; filename=x.bin\r\n", "data")
                    } else {
                        ("text/html", "", SERVED)
                    };
                    requests.lock().unwrap().push(target);
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                });
            }
        });
        port
    }

    // ----- pages ----------------------------------------------------------------------------------

    #[derive(Default)]
    struct Seen {
        loads: Vec<(PageLoadEvent, String)>,
        titles: Vec<String>,
        refused: Vec<String>,
        new_windows: Vec<String>,
        downloads: Vec<String>,
    }

    type Log = Arc<Mutex<Seen>>;

    fn observed_page(label: &str, log: &Log, directory: PathBuf) -> WebviewBuilder {
        let (navigation, new_window, download, load, title) = (log.clone(), log.clone(), log.clone(), log.clone(), log.clone());
        WebviewBuilder::new(label, blank())
            .data_directory(directory)
            .additional_browser_args(PAGE_ARGS)
            .initialization_script("window.fromInit = 'from init';")
            .devtools(true)
            .zoom_hotkeys_enabled(true)
            .general_autofill_enabled(true)
            .on_navigation(move |url| {
                let allowed = !url.as_str().contains("blocked.invalid");
                if !allowed {
                    navigation.lock().unwrap().refused.push(url.to_string());
                }
                allowed
            })
            .on_new_window(move |url, _features| {
                new_window.lock().unwrap().new_windows.push(url.to_string());
                NewWindowResponse::Deny
            })
            .on_download(move |_, event| {
                download.lock().unwrap().downloads.push(event.url().to_owned());
                false
            })
            .on_page_load(move |_, payload| {
                load.lock().unwrap().loads.push((payload.event(), payload.url().to_string()));
            })
            .on_document_title_changed(move |_, changed| {
                title.lock().unwrap().titles.push(changed);
            })
    }

    fn finished_at(log: &Log, prefix: &str) -> bool {
        log.lock().unwrap().loads.iter().any(|(event, url)| *event == PageLoadEvent::Finished && url.starts_with(prefix))
    }

    // ----- the steps ------------------------------------------------------------------------------

    #[derive(Default)]
    struct Report {
        failed: usize,
    }

    impl Report {
        fn step(&mut self, name: &str, result: Result<(), String>) {
            match result {
                Ok(()) => println!("PASS {name}"),
                Err(error) => {
                    self.failed += 1;
                    println!("FAIL {name}: {error}");
                }
            }
            let _ = std::io::stdout().flush();
        }
    }

    struct Drive {
        app: AppHandle,
        main: isize,
        png: PathBuf,
        profiles: PathBuf,
        slow: bool,
    }

    /// The worker's whole run, with the first page as it came back from the main thread.
    fn drive(context: Drive, first: Result<Webview, String>, log: Log) -> i32 {
        let Drive { app, main, png, profiles, slow } = context;
        let mut report = Report::default();
        let page = match first {
            Ok(page) => {
                report.step("add_child-in-winit-callback", Ok(()));
                page
            }
            Err(error) => {
                report.step("add_child-in-winit-callback", Err(error));
                return 1;
            }
        };
        let window = app.get_window("main").expect("attached");
        let scale = window.scale_factor().unwrap_or(1.0);

        report.step("getters", (|| {
            ensure(app.get_webview("a").is_some(), "get_webview doesn't find it")?;
            ensure(app.get_window("detached").is_none(), "a detached window exists")?;
            // SAFETY: only reads the window.
            let dpi = unsafe { GetDpiForWindow(HWND(main as _)) };
            ensure(scale == dpi as f64 / 96.0, format!("scale {scale}, dpi {dpi}"))?;
            let (mut client, mut outer, mut origin) = (RECT::default(), RECT::default(), POINT::default());
            // SAFETY: only reads the window.
            unsafe {
                GetClientRect(HWND(main as _), &mut client).map_err(text)?;
                GetWindowRect(HWND(main as _), &mut outer).map_err(text)?;
                let _ = ClientToScreen(HWND(main as _), &mut origin);
            }
            let size = window.inner_size().map_err(text)?;
            ensure((size.width as i32, size.height as i32) == (client.right, client.bottom), "inner_size")?;
            let position = window.outer_position().map_err(text)?;
            ensure((position.x, position.y) == (outer.left, outer.top), "outer_position")?;
            let position = window.inner_position().map_err(text)?;
            ensure((position.x, position.y) == (origin.x, origin.y), "inner_position")?;
            ensure(window.is_visible().map_err(text)?, "is_visible")?;
            ensure(!window.is_minimized().map_err(text)?, "is_minimized")?;
            ensure(window.hwnd().map_err(text)? == main as usize, "hwnd")
        })());

        report.step("data-url-load-events", (|| {
            page.navigate(data_url()).map_err(text)?;
            ensure(wait_until(Duration::from_secs(10), || finished_at(&log, "data:text/html")), "no Finished event")?;
            let seen = log.lock().unwrap();
            ensure(seen.loads.iter().any(|(event, url)| *event == PageLoadEvent::Started && url.starts_with("data:text/html")), "no Started event")
        })());
        report.step("title", ensure(wait_until(Duration::from_secs(5), || log.lock().unwrap().titles.iter().any(|t| t == "Engine fixture")), "no title"));
        report.step("init-script-eval", (|| {
            let value = eval(&page, "window.seenByPage")?;
            ensure(value == "\"from init\"", value)?;
            ensure(eval(&page, "1 + 2")? == "3", "1 + 2")?;
            ensure(eval(&page, "undefined")? == "null", "undefined")
        })());
        report.step("url", page.url().map_err(text).and_then(|url| ensure(url.as_str().starts_with("data:text/html"), url.to_string())));
        report.step("navigation-cancel", (|| {
            page.navigate(url::Url::parse("https://blocked.invalid/").map_err(text)?).map_err(text)?;
            ensure(wait_until(Duration::from_secs(5), || !log.lock().unwrap().refused.is_empty()), "the handler wasn't asked")?;
            std::thread::sleep(Duration::from_millis(200));
            ensure(page.url().map_err(text)?.as_str().starts_with("data:"), "the page left anyway")
        })());
        report.step("new-window-deny", (|| {
            eval(&page, "window.open('https://popup.invalid/'); 0")?;
            ensure(wait_until(Duration::from_secs(5), || log.lock().unwrap().new_windows.iter().any(|u| u.contains("popup.invalid"))), "the handler wasn't asked")?;
            ensure(page.url().map_err(text)?.as_str().starts_with("data:"), "the page left")
        })());
        report.step("no-create-in-callback", (|| {
            let (sender, receiver) = mpsc::sync_channel(1);
            let inner = window.clone();
            page.eval_with_callback("1", move |_| {
                let attempt = inner.add_child(WebviewBuilder::new("inner", blank()), LogicalPosition::new(0.0, 0.0), LogicalSize::new(1.0, 1.0));
                let _ = sender.try_send(attempt.is_err());
            })
            .map_err(text)?;
            ensure(receiver.recv_timeout(Duration::from_secs(5)).map_err(text)?, "it did")
        })());

        report.step("screenshot", (|| {
            // The first frame of a new surface may still be on its way.
            let mut png_bytes = Vec::new();
            ensure(wait_until(Duration::from_secs(5), || {
                let Ok(raw) = cdp(&page, "Page.captureScreenshot", r#"{"format":"png"}"#) else { return false };
                let Ok(json) = serde_json::from_str::<Value>(&raw) else { return false };
                let Some(data) = json["data"].as_str() else { return false };
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else { return false };
                let Ok(picture) = image::load_from_memory(&bytes) else { return false };
                let picture = picture.to_rgb8();
                let center = picture.get_pixel(picture.width() / 2, picture.height() - 8).0;
                png_bytes = bytes;
                is_page_color(center)
            }), "the screenshot doesn't show the page")?;
            std::fs::write(&png, &png_bytes).map_err(text)
        })());

        report.step("over-gpu-surface", (|| {
            pixel_becomes(main, scale, (290.0, 300.0), true, "inside the page")?;
            pixel_becomes(main, scale, (20.0, 20.0), false, "outside the page")?;
            let beside = png.with_file_name(format!("{}-window.png", png.file_stem().unwrap_or_default().to_string_lossy()));
            Capture::of(main)?.save(&beside)
        })());

        report.step("move-resize", (|| {
            page.set_position(LogicalPosition::new(300.0, 100.0)).map_err(text)?;
            page.set_size(LogicalSize::new(400.0, 300.0)).map_err(text)?;
            let (position, size) = (page.position().map_err(text)?, page.size().map_err(text)?);
            let px = |logical: f64| (logical * scale).round() as i32;
            ensure((position.x, position.y) == (px(300.0), px(100.0)), format!("position {position:?}"))?;
            ensure((size.width as i32, size.height as i32) == (px(400.0), px(300.0)), format!("size {size:?}"))?;
            let bounds = on_ui(&page, |platform| {
                let mut rect = RECT::default();
                // SAFETY: on the main thread, in the closure the page was given.
                let _ = unsafe { platform.controller().Bounds(&mut rect) };
                (rect.right - rect.left, rect.bottom - rect.top)
            })?;
            ensure(bounds == (px(400.0), px(300.0)), format!("bounds {bounds:?}"))?;
            ensure(wait_until(Duration::from_secs(3), || eval(&page, "window.innerWidth").is_ok_and(|w| w == "400")), "innerWidth isn't 400")?;
            pixel_becomes(main, scale, (500.0, 300.0), true, "at the new place")?;
            pixel_becomes(main, scale, (100.0, 200.0), false, "at the old place")
        })());
        report.step("physical-position", (|| {
            page.set_position(PhysicalPosition::new(-32_000, -32_000)).map_err(text)?;
            let position = page.position().map_err(text)?;
            ensure((position.x, position.y) == (-32_000, -32_000), format!("{position:?}"))?;
            page.set_position(LogicalPosition::new(300.0, 100.0)).map_err(text)
        })());
        report.step("hide-show", (|| {
            page.hide().map_err(text)?;
            ensure(!is_visible(&page)?, "the controller is visible")?;
            ensure(wait_until(Duration::from_secs(3), || eval(&page, "document.visibilityState").is_ok_and(|v| v == "\"hidden\"")), "still visible to the page")?;
            pixel_becomes(main, scale, (500.0, 300.0), false, "hidden")?;
            page.show().map_err(text)?;
            ensure(is_visible(&page)?, "the controller is hidden")?;
            ensure(wait_until(Duration::from_secs(3), || eval(&page, "document.visibilityState").is_ok_and(|v| v == "\"visible\"")), "still hidden to the page")?;
            pixel_becomes(main, scale, (500.0, 300.0), true, "shown")
        })());
        report.step("window-hidden", (|| {
            page.set_window_hidden(true).map_err(text)?;
            ensure(!is_visible(&page)?, "visible with its window hidden")?;
            page.show().map_err(text)?;
            ensure(!is_visible(&page)?, "show overrode the hidden window")?;
            page.set_window_hidden(false).map_err(text)?;
            ensure(is_visible(&page)?, "still hidden")?;
            page.hide().map_err(text)?;
            page.set_window_hidden(false).map_err(text)?;
            ensure(!is_visible(&page)?, "the window coming back showed a hidden page")?;
            page.show().map_err(text)
        })());
        report.step("parked-still-running", (|| {
            let container = restack(&page, true)?;
            ensure(region_kind(container) == 1, format!("region kind {}", region_kind(container)))?;
            pixel_becomes(main, scale, (500.0, 300.0), false, "parked")?;
            let before = frame_count(&page)?;
            std::thread::sleep(Duration::from_millis(500));
            let frames = frame_count(&page)? - before;
            ensure(frames >= 5.0, format!("{frames} frames in 500 ms"))?;
            ensure(eval(&page, "document.visibilityState")? == "\"visible\"", "reads as hidden")?;
            cdp(&page, "Page.captureScreenshot", r#"{"format":"png"}"#)?;
            page.hide().map_err(text)?;
            page.show().map_err(text)?;
            ensure(region_kind(container) == 1, "show raised a parked page")?;
            let container = restack(&page, false)?;
            ensure(region_kind(container) == 0, format!("region kind {}", region_kind(container)))?;
            pixel_becomes(main, scale, (500.0, 300.0), true, "back on top")
        })());
        // SAFETY: only reads which window is in front.
        if unsafe { GetForegroundWindow() }.0 as isize == main {
            report.step("focus", (|| {
                page.set_focus().map_err(text)?;
                ensure(wait_until(Duration::from_secs(3), || eval(&page, "document.hasFocus()").is_ok_and(|f| f == "true")), "the page has no focus")?;
                // SAFETY: only says the surface was clicked at its corner, which nothing here reacts to.
                unsafe {
                    PostMessageW(Some(HWND(main as _)), WM_LBUTTONDOWN, WPARAM(1), LPARAM(0x0005_0005)).map_err(text)?;
                    PostMessageW(Some(HWND(main as _)), WM_LBUTTONUP, WPARAM(0), LPARAM(0x0005_0005)).map_err(text)?;
                }
                ensure(wait_until(Duration::from_secs(3), || {
                    // SAFETY: this thread's focus is asked about from the main thread.
                    on_ui(&page, move |_| unsafe { GetFocus() }.0 as isize == main).unwrap_or(false)
                }), "the window didn't get it back")
            })());
        } else {
            println!("SKIP focus: not the foreground window");
        }
        report.step("zoom", (|| {
            page.set_zoom(1.5).map_err(text)?;
            let zoom = on_ui(&page, |platform| {
                let mut zoom = 0.0;
                // SAFETY: on the main thread, in the closure the page was given.
                let _ = unsafe { platform.controller().ZoomFactor(&mut zoom) };
                zoom
            })?;
            ensure(zoom == 1.5, format!("zoom {zoom}"))?;
            ensure(page.set_zoom(0.0).is_err() && page.set_zoom(f64::NAN).is_err(), "took an invalid zoom")?;
            page.set_zoom(1.0).map_err(text)
        })());
        report.step("reload", (|| {
            eval(&page, "window.mark = 1")?;
            let before = log.lock().unwrap().loads.len();
            page.reload().map_err(text)?;
            ensure(wait_until(Duration::from_secs(5), || log.lock().unwrap().loads.len() > before && eval(&page, "window.mark").is_ok_and(|m| m == "null")), "the document is the same one")
        })());
        report.step("devtools", (|| {
            page.open_devtools();
            ensure(wait_until(Duration::from_secs(8), || page.is_devtools_open()), "doesn't read as open")?;
            page.close_devtools();
            ensure(wait_until(Duration::from_secs(5), || !page.is_devtools_open()), "doesn't read as closed")
        })());

        let requests = Arc::new(Mutex::new(Vec::new()));
        let port = serve(requests.clone());
        let origin = format!("http://127.0.0.1:{port}");
        report.step("clear-data", (|| {
            page.navigate(url::Url::parse(&format!("{origin}/")).map_err(text)?).map_err(text)?;
            ensure(wait_until(Duration::from_secs(5), || finished_at(&log, &origin)), "the page didn't load")?;
            eval(&page, "document.cookie = 'kept=yes; max-age=3600'; 0")?;
            ensure(eval(&page, "document.cookie")?.contains("kept=yes"), "no cookie to clear")?;
            page.clear_all_browsing_data().map_err(text)?;
            ensure(wait_until(Duration::from_secs(5), || eval(&page, "document.cookie").is_ok_and(|c| c == "\"\"")), "the cookie is still there")
        })());
        report.step("download-cancel", (|| {
            page.navigate(url::Url::parse(&format!("{origin}/file.bin")).map_err(text)?).map_err(text)?;
            ensure(wait_until(Duration::from_secs(5), || log.lock().unwrap().downloads.iter().any(|u| u.ends_with("/file.bin"))), "the handler wasn't asked")?;
            ensure(page.url().map_err(text)?.as_str().starts_with(&origin), "the page left")
        })());
        report.step("window-events", (|| {
            let (moved, resized) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
            let (saw_move, saw_size) = (moved.clone(), resized.clone());
            window.on_window_event(move |event| match event {
                tauri::WindowEvent::Moved(_) => saw_move.store(true, Ordering::SeqCst),
                tauri::WindowEvent::Resized(_) => saw_size.store(true, Ordering::SeqCst),
                _ => {}
            });
            let size = window.inner_size().map_err(text)?;
            let mut outer = RECT::default();
            // SAFETY: asynchronous, so this thread doesn't wait on the window's.
            unsafe {
                GetWindowRect(HWND(main as _), &mut outer).map_err(text)?;
                let (width, height) = (outer.right - outer.left, outer.bottom - outer.top);
                SetWindowPos(HWND(main as _), None, outer.left + 10, outer.top + 10, width + 10, height + 10, SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS).map_err(text)?;
            }
            ensure(wait_until(Duration::from_secs(3), || moved.load(Ordering::SeqCst) && resized.load(Ordering::SeqCst)), "no events")?;
            ensure(window.inner_size().map_err(text)? != size, "the size didn't change")
        })());
        report.step("minimized", (|| {
            // SAFETY: asynchronous, so this thread doesn't wait on the window's.
            let _ = unsafe { ShowWindowAsync(HWND(main as _), SW_MINIMIZE) };
            ensure(wait_until(Duration::from_secs(3), || window.is_minimized().unwrap_or(false)), "it didn't minimize")?;
            let started = Instant::now();
            let (sender, receiver) = mpsc::sync_channel(1);
            page.run_on_main_thread(move || {
                let _ = sender.try_send(());
            })
            .map_err(text)?;
            receiver.recv_timeout(Duration::from_secs(2)).map_err(text)?;
            page.set_size(LogicalSize::new(410.0, 310.0)).map_err(text)?;
            page.set_size(LogicalSize::new(400.0, 300.0)).map_err(text)?;
            page.url().map_err(text)?;
            // SAFETY: as above.
            let _ = unsafe { ShowWindowAsync(HWND(main as _), SW_RESTORE) };
            ensure(wait_until(Duration::from_secs(3), || !window.is_minimized().unwrap_or(true)), "it didn't come back")?;
            ensure(started.elapsed() < Duration::from_secs(8), "slow")
        })());

        let second_profile = profiles.join("b");
        let proxied = format!("--proxy-server=http://127.0.0.1:{port} --proxy-bypass-list=<-loopback>");
        let second = window.add_child(
            WebviewBuilder::new("b", blank()).data_directory(second_profile.clone()).additional_browser_args(&format!("{PAGE_ARGS} {proxied}")),
            LogicalPosition::new(0.0, 420.0),
            LogicalSize::new(300.0, 180.0),
        );
        let second = match second {
            Ok(page) => {
                report.step("add_child-off-main", Ok(()));
                Some(page)
            }
            Err(error) => {
                report.step("add_child-off-main", Err(text(error)));
                None
            }
        };
        if let Some(second) = &second {
            report.step("environment-folder-proxy", (|| {
                let folder = on_ui(second, |platform| {
                    let mut folder = PWSTR::null();
                    // SAFETY: on the main thread, in the closure the page was given, as the host's attestation does it.
                    let read = unsafe { platform.environment().cast::<ICoreWebView2Environment7>().and_then(|environment| environment.UserDataFolder(&mut folder)) };
                    read.map(|_| take_pwstr(folder)).map_err(text)
                })??;
                ensure(Path::new(&folder) == second_profile, format!("{folder}"))?;
                ensure(browser_process(second)? != browser_process(&page)?, "it shares a browser process")?;
                second.navigate(url::Url::parse("http://proxied.invalid/").map_err(text)?).map_err(text)?;
                ensure(wait_until(Duration::from_secs(8), || requests.lock().unwrap().iter().any(|r| r.contains("proxied.invalid"))), "the proxy saw nothing")?;
                ensure(wait_until(Duration::from_secs(5), || eval(second, "document.title").is_ok_and(|t| t == "\"Served\"")), "the page didn't load through it")
            })());
        }

        report.step("builder-options", (|| {
            let options = window.add_child(
                WebviewBuilder::new("options", WebviewUrl::External(data_url()))
                    .data_directory(profiles.join("options"))
                    .user_agent("WinEngine/1")
                    .transparent(true)
                    .incognito(true)
                    .general_autofill_enabled(false)
                    .zoom_hotkeys_enabled(true)
                    .devtools(false)
                    .disable_drag_drop_handler(),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(100.0, 40.0),
            )
            .map_err(text)?;
            let read = on_ui(&options, |platform| -> windows::core::Result<(bool, bool, bool, bool, bool, u8)> {
                let controller = platform.controller();
                let (mut private, mut zoom, mut tools, mut autofill, mut drop) = (BOOL::default(), BOOL::default(), BOOL::default(), BOOL::default(), BOOL::default());
                let mut color = COREWEBVIEW2_COLOR::default();
                // SAFETY: on the main thread, in the closure the page was given.
                unsafe {
                    let core = controller.CoreWebView2()?;
                    core.cast::<ICoreWebView2_13>()?.Profile()?.IsInPrivateModeEnabled(&mut private)?;
                    let settings = core.Settings()?;
                    settings.IsZoomControlEnabled(&mut zoom)?;
                    settings.AreDevToolsEnabled(&mut tools)?;
                    settings.cast::<ICoreWebView2Settings4>()?.IsGeneralAutofillEnabled(&mut autofill)?;
                    controller.cast::<ICoreWebView2Controller4>()?.AllowExternalDrop(&mut drop)?;
                    controller.cast::<ICoreWebView2Controller2>()?.DefaultBackgroundColor(&mut color)?;
                }
                Ok((private.as_bool(), zoom.as_bool(), tools.as_bool(), autofill.as_bool(), drop.as_bool(), color.A))
            })?
            .map_err(text)?;
            ensure(read == (true, true, false, false, false, 0), format!("{read:?}"))?;
            ensure(wait_until(Duration::from_secs(5), || eval(&options, "navigator.userAgent").is_ok_and(|agent| agent == "\"WinEngine/1\"")), "user agent")?;
            // The builder's first address, reached without anyone navigating.
            ensure(wait_until(Duration::from_secs(5), || options.url().is_ok_and(|url| url.as_str().starts_with("data:"))), "the first navigation")?;
            options.close().map_err(text)
        })());

        let shared_profile = profiles.join("shared");
        let open_shared = |label: &'static str, window: Window, directory: PathBuf| {
            std::thread::spawn(move || window.add_child(WebviewBuilder::new(label, blank()).data_directory(directory).additional_browser_args(PAGE_ARGS), LogicalPosition::new(0.0, 0.0), LogicalSize::new(50.0, 50.0)).map_err(text))
        };
        let (first_shared, second_shared) = (open_shared("c1", window.clone(), shared_profile.clone()), open_shared("c2", window.clone(), shared_profile.clone()));
        let shared_pages = (first_shared.join().map_err(|_| "it panicked".to_owned()).and_then(|r| r), second_shared.join().map_err(|_| "it panicked".to_owned()).and_then(|r| r));
        if let (Ok(c1), Ok(c2)) = &shared_pages {
            report.step("shared-environment", browser_process(c1).and_then(|one| browser_process(c2).and_then(|two| ensure(one == two, format!("{one} and {two}")))));
            report.step("mismatched-switches", ensure(
                window.add_child(WebviewBuilder::new("c3", blank()).data_directory(shared_profile.clone()).additional_browser_args("--disable-gpu-vsync"), LogicalPosition::new(0.0, 0.0), LogicalSize::new(50.0, 50.0)).is_err(),
                "WebView2 took it",
            ));
            report.step("browser-process-exit", (|| {
                let exited = Arc::new(AtomicBool::new(false));
                let saw_exit = exited.clone();
                on_ui(c1, move |platform| -> Result<(), String> {
                    let environment = platform.environment().cast::<ICoreWebView2Environment5>().map_err(text)?;
                    let mut token = 0;
                    // SAFETY: on the main thread, in the closure the page was given.
                    unsafe {
                        environment.add_BrowserProcessExited(
                            &BrowserProcessExitedEventHandler::create(Box::new(move |_, _| {
                                saw_exit.store(true, Ordering::SeqCst);
                                Ok(())
                            })),
                            &mut token,
                        )
                    }
                    .map_err(text)?;
                    // Where the event is delivered, as the host's barrier keeps it.
                    std::mem::forget(environment);
                    Ok(())
                })??;
                c1.close().map_err(text)?;
                ensure(eval(c2, "1 + 1")? == "2", "the sibling stopped answering")?;
                c2.close().map_err(text)?;
                ensure(wait_until(Duration::from_secs(10), || exited.load(Ordering::SeqCst)), "BrowserProcessExited didn't come")?;
                ensure(wait_until(Duration::from_secs(5), || std::fs::remove_dir_all(&shared_profile).is_ok()), "the profile folder can't be deleted")
            })());
            report.step("label-reuse", (|| {
                window.add_child(WebviewBuilder::new("c1", blank()).data_directory(shared_profile.clone()).additional_browser_args(PAGE_ARGS), LogicalPosition::new(0.0, 0.0), LogicalSize::new(50.0, 50.0)).map_err(text)?.close().map_err(text)
            })());
        } else {
            report.step("shared-environment", Err(format!("{:?}", (shared_pages.0.err(), shared_pages.1.err()))));
        }

        report.step("no-runtime", (|| {
            // SAFETY: nothing else reads the environment at this moment; the steps run one at a time.
            unsafe { std::env::set_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER", r"C:\does\not\exist") };
            let attempt = window.add_child(WebviewBuilder::new("runtimeless", blank()).data_directory(profiles.join("runtimeless")), LogicalPosition::new(0.0, 0.0), LogicalSize::new(50.0, 50.0));
            // SAFETY: as above.
            unsafe { std::env::remove_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER") };
            let Err(error) = attempt else {
                return Err("a page was created".to_owned());
            };
            let error = error.to_string();
            ensure(error.contains("WebView2") && error.contains("未找到"), error)
        })());

        if slow {
            report.step("main-thread-stuck", (|| {
                page.run_on_main_thread(|| std::thread::sleep(Duration::from_secs(17))).map_err(text)?;
                let started = Instant::now();
                let answer = page.show();
                ensure(answer.is_err(), "it answered")?;
                let waited = started.elapsed();
                ensure(waited > Duration::from_secs(14) && waited < Duration::from_secs(17), format!("after {waited:?}"))?;
                ensure(wait_until(Duration::from_secs(5), || page.show().is_ok()), "the main thread didn't come back")
            })());
            report.step("abandoned-creation", (|| {
                let before = container_count(main);
                let waiter = open_shared("late", window.clone(), profiles.join("late"));
                // Long enough for creation to start on the main thread, and far too short for it to finish there.
                std::thread::sleep(Duration::from_millis(30));
                page.run_on_main_thread(|| std::thread::sleep(Duration::from_secs(33))).map_err(text)?;
                let result = waiter.join().map_err(|_| "it panicked".to_owned())?;
                ensure(result.is_err(), "it was created")?;
                ensure(wait_until(Duration::from_secs(12), || container_count(main) == before), format!("{} containers, was {before}", container_count(main)))?;
                ensure(app.get_webview("late").is_none(), "it's in the registry")?;
                std::thread::sleep(Duration::from_secs(2));
                ensure(container_count(main) == before, "a container came late")?;
                let again = open_shared("late", window.clone(), profiles.join("late")).join().map_err(|_| "it panicked".to_owned())??;
                again.close().map_err(text)
            })());
        }

        report.step("close", (|| {
            page.close().map_err(text)?;
            ensure(app.get_webview("a").is_none(), "get_webview still finds it")?;
            ensure(page.url().is_err() && page.show().is_err(), "it still answers")?;
            let (sender, receiver) = mpsc::sync_channel::<()>(1);
            page.with_webview(move |_| {
                let _ = sender.try_send(());
            })
            .map_err(text)?;
            // A closure that can't run any more is dropped, and that is how its caller hears of it.
            ensure(matches!(receiver.recv_timeout(Duration::from_secs(2)), Err(mpsc::RecvTimeoutError::Disconnected)), "the closure ran or never went")
        })());
        report.failed as i32
    }

    // ----- the window, as the client has it ---------------------------------------------------------

    struct Surface {
        surface: wgpu::Surface<'static>,
        device: wgpu::Device,
        queue: wgpu::Queue,
        config: wgpu::SurfaceConfiguration,
        backend: wgpu::Backend,
    }

    impl Surface {
        fn new(event_loop: &ActiveEventLoop, window: Arc<winit::window::Window>, dx12: bool) -> Result<Self, String> {
            let mut descriptor = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(event_loop.owned_display_handle()));
            if dx12 {
                descriptor.backends = wgpu::Backends::DX12;
            }
            let instance = wgpu::Instance::new(descriptor);
            let surface = instance.create_surface(window.clone()).map_err(text)?;
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { compatible_surface: Some(&surface), ..Default::default() })).map_err(text)?;
            let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).map_err(text)?;
            let size = window.inner_size();
            let mut config = surface.get_default_config(&adapter, size.width.max(1), size.height.max(1)).ok_or("the surface isn't compatible")?;
            config.present_mode = wgpu::PresentMode::AutoVsync;
            surface.configure(&device, &config);
            Ok(Self { surface, device, queue, config, backend: adapter.get_info().backend })
        }

        fn resize(&mut self, width: u32, height: u32) {
            if width > 0 && height > 0 {
                self.config.width = width;
                self.config.height = height;
                self.surface.configure(&self.device, &self.config);
            }
        }

        fn draw(&mut self) {
            let frame = match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
                wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                    self.surface.configure(&self.device, &self.config);
                    return;
                }
                _ => return,
            };
            let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.5, g: 0.2, b: 0.75, a: 1.0 }), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            }));
            self.queue.submit([encoder.finish()]);
            self.queue.present(frame);
        }
    }

    enum UserEvent {
        Finished(i32),
    }

    struct Harness {
        app: AppHandle,
        png: PathBuf,
        profiles: PathBuf,
        slow: bool,
        dx12: bool,
        proxy: EventLoopProxy<UserEvent>,
        window: Option<Arc<winit::window::Window>>,
        surface: Option<Surface>,
        exit_code: i32,
    }

    impl Harness {
        fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
            let attributes = winit::window::Window::default_attributes()
                .with_title("win_engine")
                .with_inner_size(winit::dpi::LogicalSize::new(900.0, 640.0))
                // The pixel checks read the screen.
                .with_window_level(WindowLevel::AlwaysOnTop);
            let window = Arc::new(event_loop.create_window(attributes).map_err(text)?);
            let main = match window.window_handle().map_err(text)?.as_raw() {
                RawWindowHandle::Win32(handle) => handle.hwnd.get(),
                _ => return Err("not a Win32 window".to_owned()),
            };
            window.focus_window();
            let mut surface = Surface::new(event_loop, window.clone(), self.dx12)?;
            println!("INFO renderer: {:?}, scale {}", surface.backend, window.scale_factor());
            surface.draw();
            self.surface = Some(surface);
            self.window = Some(window.clone());
            attach_main_view(main as *mut std::ffi::c_void);

            // Inside a winit callback, with paint pending, which is the hard case.
            let log: Log = Arc::default();
            let builder = observed_page("a", &log, self.profiles.join("a"));
            window.request_redraw();
            // SAFETY: marks this thread's own window as needing paint.
            let _ = unsafe { InvalidateRect(Some(HWND(main as _)), None, false) };
            let first = self.app.get_window("main").ok_or("get_window found no main window")?.add_child(builder, LogicalPosition::new(40.0, 60.0), LogicalSize::new(500.0, 400.0)).map_err(text);
            attach_main_view(main as *mut std::ffi::c_void);

            let context = Drive { app: self.app.clone(), main, png: self.png.clone(), profiles: self.profiles.clone(), slow: self.slow };
            let proxy = self.proxy.clone();
            let limit = if self.slow { 180 } else { 58 };
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(limit));
                println!("FAIL still running after {limit} s");
                std::process::exit(2);
            });
            std::thread::spawn(move || {
                let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drive(context, first, log))).unwrap_or_else(|_| {
                    println!("FAIL the driver panicked");
                    1
                });
                let _ = proxy.send_event(UserEvent::Finished(code));
            });
            Ok(())
        }
    }

    impl ApplicationHandler<UserEvent> for Harness {
        fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
            if matches!(cause, StartCause::ResumeTimeReached { .. }) {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }

        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            if let Err(error) = self.start(event_loop) {
                println!("FAIL the window: {error}");
                self.exit_code = 1;
                event_loop.exit();
            }
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::RedrawRequested => {
                    if let Some(surface) = &mut self.surface {
                        surface.draw();
                    }
                }
                WindowEvent::Resized(size) => {
                    if let Some(surface) = &mut self.surface {
                        surface.resize(size.width, size.height);
                    }
                }
                // Pages close with the window: the last check, once the client has let go of it.
                WindowEvent::Destroyed => {
                    let gone = self.app.get_window("main").is_none() && self.app.get_webview("b").is_none();
                    println!("{} window-close", if gone { "PASS" } else { "FAIL" });
                    if !gone {
                        self.exit_code = 1;
                    }
                    event_loop.exit();
                }
                _ => {}
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            // Drawing all the time, as a busy client does.
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
        }

        fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
            let UserEvent::Finished(code) = event;
            self.exit_code = code;
            self.surface = None;
            self.window = None;
        }
    }

    pub fn main() {
        let mut args = std::env::args().skip(1);
        let png = args.next().map_or_else(|| std::env::temp_dir().join("win_engine.png"), PathBuf::from);
        let flags: Vec<String> = args.collect();
        let (slow, dx12) = (flags.iter().any(|flag| flag == "--slow"), flags.iter().any(|flag| flag == "--dx12"));
        let profiles = std::env::temp_dir().join(format!("mewrk-win-engine-{}", std::process::id()));
        if let Some(parent) = png.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let app = AppHandle::new(AppConfig {
            identifier: "dev.mewrk.win-engine".to_owned(),
            version: "0".to_owned(),
            data_dir: profiles.clone(),
            local_data_dir: profiles.clone(),
            resource_dir: None,
        });
        let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
            Ok(event_loop) => event_loop,
            Err(error) => {
                println!("FAIL the event loop: {error}");
                std::process::exit(1);
            }
        };
        let proxy = event_loop.create_proxy();
        let mut harness = Harness { app, png, profiles: profiles.clone(), slow, dx12, proxy, window: None, surface: None, exit_code: 0 };
        if let Err(error) = event_loop.run_app(&mut harness) {
            println!("FAIL the event loop: {error}");
            harness.exit_code = 1;
        }
        let code = harness.exit_code;
        drop(harness);
        // The browser processes are on their way out; a folder they still hold is left for the system's temp cleanup.
        let _ = wait_until(Duration::from_secs(3), || std::fs::remove_dir_all(&profiles).is_ok());
        println!("{}", if code == 0 { "PASS overall" } else { "FAIL overall" });
        std::process::exit(code);
    }
}
