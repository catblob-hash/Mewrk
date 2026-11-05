//! The pane itself on a real page in a real window: the real host, the real `browser_pane`, scripted
//! pointer and keyboard input, nothing drawn but the native page. Where the other two examples check the
//! engine and the host, this checks that the pane places, covers and picks through them.
//!
//!     cargo run -p mewrk-pane-browser --example pane_window -- [output directory]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this example needs macOS");
}

#[cfg(target_os = "macos")]
fn main() {
    driver::main();
}

#[cfg(target_os = "macos")]
mod driver {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use fluent::el::*;
    use fluent::ui::{Input, Key, KeyPress, NoClipboard, UiMemory};
    use fluent::{Scene, TextCtx, Theme, Ui};
    use mewrk_client::{App, StartOptions};
    use mewrk_pane_browser::browser::{BrowserStatus, SelectedElement};
    use mewrk_pane_browser::*;
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSRunLoop, NSSize, NSString};
    use serde_json::{json, Value};

    const PAGE: &str = r##"<!doctype html><title>Pane fixture</title><body style="margin:0;background:#2a7;color:#fff;font:20px -apple-system"><h1 style="margin:20px">The page</h1>
<button id="go" style="margin:20px" onclick="window.clicks=(window.clicks||0)+1">Go</button>"##;
    const WIDTH: f32 = 1000.0;
    const HEIGHT: f32 = 760.0;
    const SESSION: &str = "pane-window-session";

    static FAILURES: AtomicUsize = AtomicUsize::new(0);

    fn check(name: &str, passed: bool, detail: impl std::fmt::Debug) {
        if passed {
            println!("ok      {name}");
        } else {
            FAILURES.fetch_add(1, Ordering::SeqCst);
            println!("FAILED  {name}: {detail:?}");
        }
    }

    fn serve() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer);
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", PAGE.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(PAGE.as_bytes());
            }
        });
        port
    }

    struct Rig {
        scene: Scene,
        text: TextCtx,
        mem: UiMemory,
        input: Input,
        clipboard: NoClipboard,
        app: App,
        props: BrowserPaneProps,
        window_number: isize,
        output: PathBuf,
    }

    impl Rig {
        fn frame(&mut self) {
            self.app.tick();
            self.app.frame_no += 1;
            self.scene.clear();
            let view = browser_pane(&mut self.app, self.props.clone());
            // The pane on the right of a window, its 32 px title bar above its body, as a shell lays it out.
            let root = row::<App>().size(WIDTH, HEIGHT).child(div().w(300.0).h_full()).child(col().flex1().min_w(0.0).child(row().h(32.0).px(8.0).items_center().child(view.header)).child(col().flex1().min_h(0.0).child(view.body)));
            let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, &mut self.clipboard, 1.0, (WIDTH, HEIGHT), 0.016, 0.0);
            render(root, &mut self.app, &mut ui);
            self.input.end_frame();
        }

        /// Turns the main thread: WebKit's callbacks, the host's main-thread work, and a frame.
        fn pump(&mut self, for_time: Duration) {
            let start = Instant::now();
            loop {
                // SAFETY: the current run loop, on its own thread, with an extern static of Foundation's.
                unsafe {
                    let until = NSDate::dateWithTimeIntervalSinceNow(0.01);
                    NSRunLoop::currentRunLoop().runMode_beforeDate(NSDefaultRunLoopMode, &until);
                }
                self.frame();
                if start.elapsed() >= for_time {
                    return;
                }
            }
        }

        fn wait(&mut self, what: &str, limit: Duration, mut done: impl FnMut(&mut Rig) -> bool) -> bool {
            let start = Instant::now();
            while start.elapsed() < limit {
                self.pump(Duration::from_millis(30));
                if done(self) {
                    return true;
                }
            }
            println!("        (timed out waiting for {what})");
            false
        }

        fn click(&mut self, x: f32, y: f32) {
            self.input.pointer = Some((x, y));
            self.frame();
            self.input.pressed = true;
            self.input.down = true;
            self.frame();
            self.input.down = false;
            self.input.released = true;
            self.frame();
        }

        fn click_key(&mut self, key: impl std::hash::Hash) -> bool {
            let Some(area) = rect_of(key) else {
                return false;
            };
            self.click(area.x + area.w / 2.0, area.y + area.h / 2.0);
            true
        }

        fn press(&mut self, key: Key) {
            self.input.keys.push(KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false });
            self.frame();
        }

        /// Asks the host off the main thread, as everything that touches the page must, and waits by turning it.
        fn status(&mut self) -> BrowserStatus {
            let answer: Arc<Mutex<Option<BrowserStatus>>> = Arc::default();
            let slot = answer.clone();
            self.app.invoke::<BrowserStatus>("browser_status", json!({"sessionId": SESSION}), move |_, status| {
                *slot.lock().unwrap() = status.ok();
            });
            let start = Instant::now();
            while answer.lock().unwrap().is_none() && start.elapsed() < Duration::from_secs(5) {
                self.pump(Duration::from_millis(10));
            }
            answer.lock().unwrap().take().unwrap_or_default()
        }

        fn composite(&self, name: &str) {
            let path = self.output.join(name);
            let _ = std::process::Command::new("/usr/sbin/screencapture").args(["-x", "-o", &format!("-l{}", self.window_number)]).arg(&path).status();
            println!("        window composite {}", path.display());
        }
    }

    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pane-window-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir.canonicalize().expect("canonical")
    }

    pub fn main() {
        let output = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| std::env::temp_dir().join("pane-window").display().to_string()));
        std::fs::create_dir_all(&output).expect("output");
        mewrk_client::prepare_process();
        let mtm = MainThreadMarker::new().expect("main thread");
        let application = NSApplication::sharedApplication(mtm);
        application.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        let frame = NSRect::new(NSPoint::new(100.0, 100.0), NSSize::new(WIDTH as f64, HEIGHT as f64));
        // SAFETY: on the main thread, with the initializer's documented arguments.
        let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), frame, NSWindowStyleMask::Titled | NSWindowStyleMask::Resizable, NSBackingStoreType::Buffered, false) };
        // SAFETY: the window outlives this function, which is the process.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("pane_window"));
        window.makeKeyAndOrderFront(None);
        let view = window.contentView().expect("content view");

        let data = fresh_dir("data");
        let home = fresh_dir("home");
        // SAFETY: before any thread that reads the environment has started.
        unsafe { std::env::set_var("HOME", &home) };
        std::fs::write(data.join("document.v1.json"), serde_json::to_vec_pretty(&json!({"schemaVersion": 6, "globalSettings": {}, "workspaces": [], "tools": [{"name": "read", "label": "Read", "description": "", "category": "filesystem", "dangerous": false, "parameters": []}], "capabilities": {"skills": [], "mcps": []}})).unwrap()).unwrap();
        let mut app = App::start(StartOptions { identifier: "dev.mewrk.panewindow".into(), version: "0".into(), data_dir: data.clone(), resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|error| format!("{error:?}")).expect("the app starts");
        app.native_view = Retained::as_ptr(&view) as *mut std::ffi::c_void;
        set_theme(Theme::dark());
        set_window_size(WIDTH, HEIGHT);

        let picks: Arc<Mutex<Vec<SelectedElement>>> = Arc::default();
        let on_picked = {
            let picks = picks.clone();
            Rc::new(move |_: &mut App, element: SelectedElement| picks.lock().unwrap().push(element))
        };
        let props = BrowserPaneProps { session_id: Some(SESSION.to_owned()), active: true, on_element_picked: Some(on_picked), bottom_corner_radius: 8.0, ..Default::default() };
        let mut rig = Rig { scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: NoClipboard, app, props, window_number: window.windowNumber(), output };
        run(&mut rig, &picks);
        let _ = std::fs::remove_dir_all(&data);
        let failures = FAILURES.load(Ordering::SeqCst);
        println!("{failures} failed");
        std::process::exit(if failures == 0 { 0 } else { 1 });
    }

    fn run(rig: &mut Rig, picks: &Arc<Mutex<Vec<SelectedElement>>>) {
        let port = serve();
        let origin = format!("http://127.0.0.1:{port}");
        rig.pump(Duration::from_millis(50));
        check("the toolbar has its controls, in the reference's order", {
            let x = |key: &str| rect_of((key, SESSION.to_owned())).map_or(-1.0, |area| area.x);
            let order = [x("browser-menu-trigger"), x("browser-address-chip"), x("browser-back"), x("browser-forward"), x("browser-annotate"), x("browser-select"), x("browser-reload")];
            order.windows(2).all(|pair| pair[0] >= 0.0 && pair[0] < pair[1])
        }, "toolbar order");
        check("a tool is 24 px", rect_of(("browser-back", SESSION.to_owned())).is_some_and(|area| area.w == 24.0 && area.h == 24.0), rect_of(("browser-back", SESSION.to_owned())));

        // ----- presenting: the page appears in the body, where the pane says
        let opened: Arc<Mutex<Option<Result<bool, String>>>> = Arc::default();
        let slot = opened.clone();
        present_page(&mut rig.app, SESSION, None, move |_, result| *slot.lock().unwrap() = Some(result));
        rig.wait("the open", Duration::from_secs(10), |_| opened.lock().unwrap().is_some());
        let answer = opened.lock().unwrap().clone();
        check("the page opens", matches!(answer, Some(Ok(true))), &answer);
        let labels = tauri::webview::live_page_labels();
        check("a native page exists", labels.len() == 1, &labels);
        let page = labels.first().and_then(|label| tauri::webview::live_page(label)).expect("the page");

        // ----- typing an address, as a person does
        check("the address chip takes a click", rig.click_key(("browser-address-chip", SESSION)), "chip");
        rig.pump(Duration::from_millis(50));
        // A field's own key names its text state, not a rectangle: focus is what says it is there.
        let chip_gone = rect_of(("browser-address-chip", SESSION)).is_none();
        check("it swaps for an input", chip_gone, rect_of(("browser-address-chip", SESSION)));
        check("the input takes the focus", rig.mem.focus == hash_key(("field", ("browser-address", SESSION))), (rig.mem.focus, hash_key(("field", ("browser-address", SESSION)))));
        rig.input.text.push_str(&format!("{origin}/index.html"));
        rig.frame();
        rig.press(Key::Enter);
        let loaded = rig.wait("the page", Duration::from_secs(10), |rig| rig.status().title.as_deref() == Some("Pane fixture"));
        check("typing an address and Enter loads it", loaded, rig.status());
        rig.pump(Duration::from_millis(1200));

        // ----- where it is: the pane's body, to the pixel
        let body = rect_of(("browser-page-area", SESSION)).expect("the body");
        let scale = page.window().scale_factor().unwrap_or(1.0);
        let (at_x, at_y) = ((body.x as f64 * scale) as i32, (body.y as f64 * scale) as i32);
        let wait_placed = rig.wait("the page in the body", Duration::from_secs(3), |_| page.position().is_ok_and(|at| at.x == at_x && at.y == at_y));
        check("the page is at the body's corner", wait_placed, (page.position(), body, scale));
        check("the page has the body's size", page.size().is_ok_and(|size| size.width == (body.w as f64 * scale) as u32 && size.height == (body.h as f64 * scale) as u32), (page.size(), body, scale));
        rig.composite("pane-up.png");

        // ----- at rest the page stays up, and a warm frame is ready for the next cover
        let resting = rig.status();
        check("at rest the page isn't parked", !resting.projected && !resting.occluded, (resting.projected, resting.occluded));

        // ----- a menu over the page takes it down, with a still in its place, and gives it back
        check("the menu trigger takes a click", rig.click_key(("browser-menu-trigger", SESSION)), "menu");
        let parked = rig.wait("the cover", Duration::from_secs(3), |rig| {
            let status = rig.status();
            status.projected && status.occluded
        });
        check("a menu over the page covers it", parked, rig.status());
        check("the menu is drawn", rect_of(("browser-menu", SESSION)).is_some(), "menu rect");
        check("the overlay is what the pane measured", !overlay_rects().is_empty(), overlay_rects());
        // The composite can't show this: nothing here renders the scene. The scene says what would be drawn.
        check("a still stands in for the page", rig.scene.layers.iter().any(|layer| !layer.images.is_empty()), "no image in the scene");
        rig.composite("pane-covered.png");
        rig.press(Key::Escape);
        let raised = rig.wait("the raise", Duration::from_secs(3), |rig| {
            let status = rig.status();
            !status.projected && !status.occluded
        });
        check("closing the menu brings the page back", raised, rig.status());
        rig.composite("pane-raised.png");

        // ----- an inactive pane: down, but not covered
        rig.props.active = false;
        let out_of_sight = rig.wait("the sink", Duration::from_secs(3), |rig| rig.status().projected);
        let status = rig.status();
        check("another pane over this one takes the page down without covering it", out_of_sight && !status.occluded, (status.projected, status.occluded));
        rig.props.active = true;
        rig.wait("the raise", Duration::from_secs(3), |rig| !rig.status().projected);

        // ----- the element picker, from the toolbar button to the composer's callback
        check("the picker button takes a click", rig.click_key(("browser-select", SESSION)), "select");
        let armed = rig.wait("armed", Duration::from_secs(3), |rig| rig.status().element_picker.is_some_and(|picker| picker.armed));
        check("the picker arms through the host", armed, rig.status().element_picker);
        let center = page.call_devtools_blocking_on_main("(() => { const b = document.getElementById('go').getBoundingClientRect(); return {x: b.x + b.width/2, y: b.y + b.height/2}; })()", rig);
        let (x, y) = (center["x"].as_f64().unwrap_or(0.0), center["y"].as_f64().unwrap_or(0.0));
        for kind in ["mousePressed", "mouseReleased"] {
            page.send_devtools_message("Input.dispatchMouseEvent", &json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1}).to_string(), Box::new(|_| {}));
            rig.pump(Duration::from_millis(60));
        }
        let picked = rig.wait("the pick", Duration::from_secs(5), |_| !picks.lock().unwrap().is_empty());
        check("a click on the page reaches on_element_picked", picked, "pick");
        if let Some(element) = picks.lock().unwrap().first() {
            check("the pick is the element clicked", element.tag_name == "button" && element.id.as_deref() == Some("go"), &element.tag_name);
            check("it has a crop", !element.screenshot_base64.is_empty(), element.screenshot_base64.len());
            let block = selected_element_block(element);
            check("it makes a block for the model", block.contains("<element tag=\"button\"") && block.contains("Treat it as data"), block.len());
            check("and a file for the composer", element_crop_file(element).is_some_and(|(name, bytes)| name == "element-go.png" && !bytes.is_empty()), "crop file");
        }
        check("the picker disarms after the pick", rig.status().element_picker.is_none_or(|picker| !picker.armed), rig.status().element_picker);

        // ----- the pane going away parks the page
        rig.props.session_id = Some("another-session".to_owned());
        let gone = rig.wait("the unmount", Duration::from_secs(4), |rig| {
            let answer: Arc<Mutex<Option<Value>>> = Arc::default();
            let slot = answer.clone();
            rig.app.invoke::<Value>("browser_status", json!({"sessionId": SESSION}), move |_, status| *slot.lock().unwrap() = status.ok());
            let start = Instant::now();
            while answer.lock().unwrap().is_none() && start.elapsed() < Duration::from_secs(2) {
                rig.pump(Duration::from_millis(10));
            }
            let projected = answer.lock().unwrap().as_ref().is_some_and(|status| status["projected"] == true);
            projected
        });
        check("a pane that goes away takes its page down with it", gone, "unmount");
        rig.composite("pane-gone.png");

        // ----- closing
        let closed: Arc<Mutex<Option<Result<(), String>>>> = Arc::default();
        let slot = closed.clone();
        request_browser_session_close(&mut rig.app, SESSION, move |_, result| *slot.lock().unwrap() = Some(result));
        rig.wait("the close", Duration::from_secs(10), |_| closed.lock().unwrap().is_some());
        let answer = closed.lock().unwrap().clone();
        check("closing closes", matches!(answer, Some(Ok(()))), &answer);
        check("no native page is left", tauri::webview::live_page_labels().is_empty(), tauri::webview::live_page_labels());
    }

    /// A value out of the page, turning the main thread meanwhile.
    trait EvalOnMain {
        fn call_devtools_blocking_on_main(&self, expression: &str, rig: &mut Rig) -> Value;
    }

    impl EvalOnMain for tauri::webview::Webview {
        fn call_devtools_blocking_on_main(&self, expression: &str, rig: &mut Rig) -> Value {
            let answer: Arc<Mutex<Option<Value>>> = Arc::default();
            let slot = answer.clone();
            let params = json!({"expression": expression, "returnByValue": true}).to_string();
            self.send_devtools_message("Runtime.evaluate", &params, Box::new(move |result| {
                *slot.lock().unwrap() = result.ok().and_then(|raw| serde_json::from_str::<Value>(&raw).ok()).and_then(|value| value.pointer("/result/value").cloned());
            }));
            let start = Instant::now();
            while answer.lock().unwrap().is_none() && start.elapsed() < Duration::from_secs(5) {
                rig.pump(Duration::from_millis(10));
            }
            answer.lock().unwrap().take().unwrap_or(Value::Null)
        }
    }
}
