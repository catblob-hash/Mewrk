//! Runs the WKWebView page engine in a real window, the way the host drives it, and says what it saw.
//!
//!     cargo run -p mewrk-pane-browser --example wk_engine -- [output directory]
//!
//! It needs a window and the main thread, so it isn't a test: a loopback server serves the page, a
//! worker thread drives the engine through the same calls `browser.rs` makes while the main thread
//! runs AppKit, and every check prints `ok` or `FAILED`. Screenshots land in the output directory.

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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    use base64::Engine as _;
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use serde_json::{json, Value};
    use tauri::webview::{DialogDecision, PageDialog, PageLoadEvent, Webview, WebviewBuilder};
    use tauri::{AppConfig, AppHandle, LogicalPosition, LogicalSize, Manager, WebviewUrl};

    const PAGE: &str = r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Engine fixture</title>
<link rel="stylesheet" href="/style.css">
<style>
body { font: 16px -apple-system, sans-serif; margin: 0; background: #fff; color: #111 }
#hero { background: linear-gradient(90deg, #d33, #36c); color: #fff; padding: 24px; font-size: 28px }
#tall { height: 3000px; background: linear-gradient(#efe, #fee) }
#bottom { background: #2a2; color: #fff; padding: 8px }
</style></head><body>
<div id="hero">WKWebView page engine</div>
<nav aria-label="Main"><a id="link" href="/second.html">Second page</a></nav>
<main>
<h1>Fixture</h1>
<button id="count" onclick="window.clicks = (window.clicks||0)+1; this.textContent='Clicked '+window.clicks; console.log('clicked', window.clicks)">Click me</button>
<label for="name">Your name</label><input id="name" placeholder="name">
<input id="file" type="file">
<img alt="A red square" width="16" height="16" src="/square.svg">
<a id="popup" href="/second.html" target="_blank">Open in new tab</a>
<div id="tall"></div>
<div id="bottom">the bottom of the page</div>
</main>
<script>
window.trustedClick = null;
document.getElementById('count').addEventListener('click', (e) => { window.trustedClick = e.isTrusted; });
fetch('/api?x=1').then(r => r.json()).then(j => { window.apiValue = j.value; console.log('api', j.value); });
console.warn('a warning');
</script>
</body></html>"##;

    /// One response by path, and a log of what was asked. Enough for a real fetch from a real origin.
    fn serve(log: Arc<Mutex<Vec<String>>>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let log = log.clone();
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 8192];
                    let Ok(read) = stream.read(&mut buffer) else { return };
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    log.lock().unwrap().push(path.clone());
                    if path.starts_with("/mjpeg") {
                        return serve_mjpeg(stream);
                    }
                    let (status, kind, body, extra): (&str, &str, Vec<u8>, &str) = match path.split('?').next().unwrap_or("/") {
                        "/" | "/index.html" => ("200 OK", "text/html", PAGE.as_bytes().to_vec(), ""),
                        "/second.html" => ("200 OK", "text/html", b"<!doctype html><title>Second</title><h1>Second page</h1><script>console.error('second page error')</script>".to_vec(), ""),
                        "/style.css" => ("200 OK", "text/css", b"h1 { color: rgb(10, 20, 30) }".to_vec(), ""),
                        "/square.svg" => ("200 OK", "image/svg+xml", br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="red"/></svg>"#.to_vec(), ""),
                        "/api" => ("200 OK", "application/json", br#"{"value":42}"#.to_vec(), "Set-Cookie: fromserver=yes; Path=/; HttpOnly\r\n"),
                        "/missing" => ("404 Not Found", "text/plain", b"nope".to_vec(), ""),
                        "/download.bin" => ("200 OK", "application/octet-stream", vec![0u8; 16], "Content-Disposition: attachment; filename=x.bin\r\n"),
                        "/viewer.html" => ("200 OK", "text/html", b"<!doctype html><title>Viewer</title><img id=screen src='/mjpeg'>".to_vec(), ""),
                        _ => ("404 Not Found", "text/plain", b"nope".to_vec(), ""),
                    };
                    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                });
            }
        });
        port
    }

    /// A SOCKS5 endpoint like the host's tunnel: every connection is answered by the fixture server,
    /// whatever it asked for, and what it asked for is recorded.
    fn serve_socks(targets: Arc<Mutex<Vec<String>>>, fixture_port: u16) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut client) = stream else { continue };
                let targets = targets.clone();
                std::thread::spawn(move || {
                    let mut greeting = [0u8; 2];
                    if client.read_exact(&mut greeting).is_err() {
                        return;
                    }
                    let mut methods = vec![0u8; greeting[1] as usize];
                    if client.read_exact(&mut methods).is_err() || client.write_all(&[5, 0]).is_err() {
                        return;
                    }
                    let mut request = [0u8; 4];
                    if client.read_exact(&mut request).is_err() {
                        return;
                    }
                    let host = match request[3] {
                        1 => {
                            let mut address = [0u8; 4];
                            let _ = client.read_exact(&mut address);
                            address.iter().map(u8::to_string).collect::<Vec<_>>().join(".")
                        }
                        3 => {
                            let mut length = [0u8; 1];
                            let _ = client.read_exact(&mut length);
                            let mut name = vec![0u8; length[0] as usize];
                            let _ = client.read_exact(&mut name);
                            String::from_utf8_lossy(&name).into_owned()
                        }
                        _ => {
                            let mut address = [0u8; 16];
                            let _ = client.read_exact(&mut address);
                            "ipv6".to_owned()
                        }
                    };
                    let mut port_bytes = [0u8; 2];
                    let _ = client.read_exact(&mut port_bytes);
                    targets.lock().unwrap().push(format!("{host}:{}", u16::from_be_bytes(port_bytes)));
                    let Ok(server) = std::net::TcpStream::connect(("127.0.0.1", fixture_port)) else {
                        return;
                    };
                    if client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).is_err() {
                        return;
                    }
                    let (mut client_read, mut server_write) = (client.try_clone().expect("clone"), server.try_clone().expect("clone"));
                    let up = std::thread::spawn(move || {
                        let _ = std::io::copy(&mut client_read, &mut server_write);
                    });
                    let (mut server_read, mut client_write) = (server, client);
                    let _ = std::io::copy(&mut server_read, &mut client_write);
                    let _ = up.join();
                });
            }
        });
        port
    }

    /// The viewer's stream as the host writes it (`remote_control::viewer`): each frame followed at once
    /// by the next part's headers. PNGs of two sizes stand in for JPEGs, so the page can tell them apart.
    fn serve_mjpeg(mut stream: std::net::TcpStream) {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n--frame\r\nContent-Type: image/png\r\n\r\n";
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        for frame in 0..40 {
            let png = tiny_png(if frame % 2 == 0 { 2 } else { 3 });
            if stream.write_all(&png).is_err() || stream.write_all(b"\r\n--frame\r\nContent-Type: image/png\r\n\r\n").is_err() {
                return;
            }
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A black square PNG `size` wide: an IDAT with stored deflate blocks, so no encoder is needed.
    fn tiny_png(size: u32) -> Vec<u8> {
        fn crc(data: &[u8]) -> u32 {
            let mut crc = 0xffff_ffffu32;
            for byte in data {
                crc ^= *byte as u32;
                for _ in 0..8 {
                    crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
                }
            }
            !crc
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend((data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend(data);
            out.extend(&body);
            out.extend(crc(&body).to_be_bytes());
        }
        let mut raw = Vec::new();
        for _ in 0..size {
            raw.push(0u8);
            raw.extend(std::iter::repeat_n(0u8, size as usize));
        }
        let mut idat = vec![0x78, 0x01, 0x01];
        idat.extend((raw.len() as u16).to_le_bytes());
        idat.extend((!(raw.len() as u16)).to_le_bytes());
        idat.extend(&raw);
        let (mut a, mut b) = (1u32, 0u32);
        for byte in &raw {
            a = (a + *byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        idat.extend(((b << 16) | a).to_be_bytes());
        let mut ihdr = Vec::new();
        ihdr.extend(size.to_be_bytes());
        ihdr.extend(size.to_be_bytes());
        ihdr.extend([8, 0, 0, 0, 0]);
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &idat);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    static FAILURES: AtomicUsize = AtomicUsize::new(0);

    fn check(name: &str, passed: bool, detail: impl std::fmt::Debug) {
        if passed {
            println!("ok      {name}");
        } else {
            FAILURES.fetch_add(1, Ordering::SeqCst);
            println!("FAILED  {name}: {detail:?}");
        }
    }

    fn cdp(page: &Webview, method: &str, params: Value) -> Result<Value, String> {
        let raw = page.call_devtools_blocking(method, &params.to_string(), Duration::from_secs(15))?;
        serde_json::from_str(&raw).map_err(|error| error.to_string())
    }

    fn eval(page: &Webview, expression: &str) -> Value {
        match cdp(page, "Runtime.evaluate", json!({"expression": expression, "returnByValue": true, "awaitPromise": true})) {
            Ok(response) => response.pointer("/result/value").cloned().unwrap_or(Value::Null),
            Err(error) => json!({ "adapterError": error }),
        }
    }

    fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        println!("        (timed out waiting for {what})");
        false
    }

    fn png_size(bytes: &[u8]) -> (u32, u32) {
        if bytes.len() < 24 {
            return (0, 0);
        }
        (u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]), u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]))
    }

    fn screenshot(page: &Webview, params: Value, path: &PathBuf) -> Result<Vec<u8>, String> {
        let response = cdp(page, "Page.captureScreenshot", params)?;
        let data = response.get("data").and_then(Value::as_str).ok_or("no data")?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|error| error.to_string())?;
        std::fs::write(path, &bytes).map_err(|error| error.to_string())?;
        Ok(bytes)
    }

    pub fn main() {
        let output = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| std::env::temp_dir().join("wk-engine").display().to_string()));
        std::fs::create_dir_all(&output).expect("output directory");
        let mtm = MainThreadMarker::new().expect("main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        let frame = NSRect::new(NSPoint::new(120.0, 120.0), NSSize::new(1000.0, 760.0));
        // SAFETY: on the main thread, with the initializer's documented arguments.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: the window outlives the process's only run loop, and is released with it.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("wk_engine"));
        window.makeKeyAndOrderFront(None);
        WINDOW_NUMBER.store(window.windowNumber() as usize, Ordering::SeqCst);
        let view = window.contentView().expect("content view");
        tauri::webview::attach_main_view(Retained::as_ptr(&view) as *mut std::ffi::c_void);
        let args: Vec<String> = std::env::args().skip(2).collect();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| run(&output, &args));
            if result.is_err() {
                FAILURES.fetch_add(1, Ordering::SeqCst);
                println!("FAILED  the driver panicked");
            }
            let failures = FAILURES.load(Ordering::SeqCst);
            println!("{failures} failed");
            std::process::exit(if failures == 0 { 0 } else { 1 });
        });
        app.run();
        let _ = (window, view);
    }

    /// The window as the window server composites it. Needs screen-recording permission, and if that
    /// isn't there this says so instead of failing: it's a picture for a person to look at.
    fn window_composite(path: &PathBuf) {
        let number = WINDOW_NUMBER.load(Ordering::SeqCst);
        let status = std::process::Command::new("/usr/sbin/screencapture").args(["-x", "-o", &format!("-l{number}")]).arg(path).status();
        println!("        window composite {}: {status:?}", path.display());
    }

    static WINDOW_NUMBER: AtomicUsize = AtomicUsize::new(0);

    /// The remote screen's real viewer page, from the host's `serve_a_fake_session_for_a_browser`.
    fn run_viewer(output: &PathBuf, url: &str) {
        let data = std::env::temp_dir().join(format!("wk-engine-viewer-{}", std::process::id()));
        std::fs::create_dir_all(&data).expect("data");
        let handle = AppHandle::new(AppConfig { identifier: "dev.mewrk.example".to_owned(), version: "0".to_owned(), data_dir: data.clone(), local_data_dir: data.clone(), resource_dir: None });
        let window = handle.get_window("main").expect("window");
        let page = window.add_child(WebviewBuilder::new("viewer", WebviewUrl::External(url::Url::parse(url).expect("url"))).data_directory(data.clone()).initialization_script("window.__errors = []; addEventListener('error', e => window.__errors.push(String(e.message) + ' @' + e.filename + ':' + e.lineno)); addEventListener('securitypolicyviolation', e => window.__errors.push('csp ' + e.violatedDirective + ' ' + e.blockedURI)); addEventListener('unhandledrejection', e => window.__errors.push('rejected ' + e.reason));"), LogicalPosition::new(0.0, 0.0), LogicalSize::new(1000.0, 760.0)).expect("page");
        std::thread::sleep(Duration::from_millis(800));
        println!("        hidden: {}, visibility: {}, state: {}", eval(&page, "document.hidden"), eval(&page, "document.visibilityState"), eval(&page, "fetch('state', {cache: 'no-store', credentials: 'omit'}).then(r => r.status)"));
        let painted = wait_until("the picture", Duration::from_secs(10), || eval(&page, "document.getElementById('screen').naturalWidth > 0") == true);
        check("the viewer's stream paints", painted, eval(&page, "document.getElementById('screen').naturalWidth"));
        check("the state poll reached the page", wait_until("the name", Duration::from_secs(5), || eval(&page, "document.getElementById('name').textContent.length > 0") == true), eval(&page, "document.getElementById('name').textContent"));
        println!("        errors: {}", eval(&page, "window.__errors"));
        println!("        scripts: {}", eval(&page, "Array.from(document.scripts).map(s => (s.type || 'js') + ':' + s.textContent.length)"));
        let first = eval(&page, "document.getElementById('phase').textContent");
        println!("        phase: {first}, title: {}", eval(&page, "document.title"));
        std::thread::sleep(Duration::from_millis(500));
        screenshot(&page, json!({"format": "png"}), &output.join("viewer.png")).ok();
        // Input goes out as a page's fetch, and the server answers it: a click on the stage has to post.
        eval(&page, "window.__posts = 0; const f = window.fetch; window.fetch = function(u, o) { if (o && o.method === 'POST') window.__posts++; return f.apply(this, arguments); }; true");
        let center = eval(&page, "(() => { const b = document.getElementById('screen').getBoundingClientRect(); return {x: b.x + b.width/2, y: b.y + b.height/2}; })()");
        for kind in ["mousePressed", "mouseReleased"] {
            cdp(&page, "Input.dispatchMouseEvent", json!({"type": kind, "x": center["x"], "y": center["y"], "button": "left", "clickCount": 1})).ok();
        }
        check("a click on the screen posts input", wait_until("the post", Duration::from_secs(3), || eval(&page, "window.__posts").as_i64().unwrap_or(0) > 0), eval(&page, "window.__posts"));
        page.close().ok();
        let _ = std::fs::remove_dir_all(&data);
    }

    fn run(output: &PathBuf, args: &[String]) {
        if let [flag, url] = args {
            if flag == "--viewer" {
                return run_viewer(output, url);
            }
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let port = serve(requests.clone());
        let origin = format!("http://127.0.0.1:{port}");
        let data = std::env::temp_dir().join(format!("wk-engine-{}", std::process::id()));
        std::fs::create_dir_all(&data).expect("data dir");
        let handle = AppHandle::new(AppConfig {
            identifier: "dev.mewrk.example".to_owned(),
            version: "0".to_owned(),
            data_dir: data.clone(),
            local_data_dir: data.clone(),
            resource_dir: None,
        });
        let window = handle.get_window("main").expect("the attached view makes a main window");
        check("the window reports a scale", window.scale_factor().is_ok_and(|scale| scale >= 1.0), window.scale_factor());
        check("the window reports its size", window.inner_size().is_ok_and(|size| size.width >= 1000), window.inner_size());

        let loads: Arc<Mutex<Vec<(PageLoadEvent, String)>>> = Arc::default();
        let titles: Arc<Mutex<Vec<String>>> = Arc::default();
        let blocked: Arc<Mutex<Vec<String>>> = Arc::default();
        let new_windows: Arc<Mutex<Vec<String>>> = Arc::default();
        let events: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let profile = data.join("profile-a");
        std::fs::create_dir_all(&profile).expect("profile");
        let builder = {
            let (loads, titles, blocked, new_windows) = (loads.clone(), titles.clone(), blocked.clone(), new_windows.clone());
            WebviewBuilder::new("page-a", WebviewUrl::External(url::Url::parse("about:blank").unwrap()))
                .data_directory(profile.clone())
                .initialization_script("window.__fromInitScript = location.href; console.__seen = [];['log','warn','error'].forEach(k => { const o = console[k]; console[k] = function(){ console.__seen.push([k].concat(Array.from(arguments).map(String)).join(' ')); return o.apply(this, arguments); }; });")
                .on_navigation(move |url| {
                    let allowed = !url.as_str().contains("forbidden");
                    if !allowed {
                        blocked.lock().unwrap().push(url.to_string());
                    }
                    allowed
                })
                .on_new_window(move |url, _| {
                    new_windows.lock().unwrap().push(url.to_string());
                    tauri::webview::NewWindowResponse::Deny
                })
                .on_download(|_, _| false)
                .on_page_load(move |_, payload| loads.lock().unwrap().push((payload.event(), payload.url().to_string())))
                .on_document_title_changed(move |_, title| titles.lock().unwrap().push(title))
        };
        let page = window.add_child(builder, LogicalPosition::new(0.0, 0.0), LogicalSize::new(1000.0, 760.0)).expect("the page is created");
        check("the page resolves by label", handle.get_webview("page-a").is_some(), "get_webview");
        check("the profile is the directory asked for", page.cache_path().ok().as_deref() == Some(profile.as_path()), page.cache_path());
        {
            let events = events.clone();
            page.set_event_listener(Some(Arc::new(move |method: &str, params: &str| {
                events.lock().unwrap().push((method.to_owned(), params.to_owned()));
            })));
        }
        for method in ["Network.enable", "Page.enable"] {
            check(method, cdp(&page, method, json!({})).is_ok(), method);
        }
        cdp(&page, "Page.setInterceptFileChooserDialog", json!({"enabled": true})).expect("intercept");

        // ----- about:blank and the bootstrap
        wait_until("the first document", Duration::from_secs(5), || loads.lock().unwrap().iter().any(|(event, url)| *event == PageLoadEvent::Finished && url == "about:blank"));
        let blank_hook = eval(&page, "typeof window.__mewrkWk");
        println!("        bootstrap on about:blank: {blank_hook}");
        check("the bootstrap runs on about:blank", blank_hook == "object", &blank_hook);
        check("the init script runs on about:blank", eval(&page, "window.__fromInitScript") == "about:blank", eval(&page, "window.__fromInitScript"));

        // ----- navigation, load events, title
        page.navigate(url::Url::parse(&format!("{origin}/index.html")).unwrap()).expect("navigate");
        let loaded = wait_until("the page", Duration::from_secs(10), || loads.lock().unwrap().iter().any(|(event, url)| *event == PageLoadEvent::Finished && url.ends_with("/index.html")));
        check("the page loads and says so", loaded, loads.lock().unwrap().clone());
        check("a load start is reported first", loads.lock().unwrap().iter().any(|(event, url)| *event == PageLoadEvent::Started && url.ends_with("/index.html")), "see the log above");
        check("the page's address is readable", page.url().is_ok_and(|url| url.as_str().ends_with("/index.html")), page.url());
        wait_until("the title", Duration::from_secs(3), || titles.lock().unwrap().iter().any(|title| title == "Engine fixture"));
        check("the title is reported", titles.lock().unwrap().iter().any(|title| title == "Engine fixture"), "see the log above");

        // ----- Runtime
        check("evaluate gives a number", eval(&page, "1 + 2") == 3, eval(&page, "1 + 2"));
        check("evaluate gives an object", eval(&page, "({a: [1, 'b']})") == json!({"a": [1, "b"]}), eval(&page, "({a: [1, 'b']})"));
        check("evaluate awaits a promise", eval(&page, "new Promise(r => setTimeout(() => r('later'), 20))") == "later", "promise");
        check("evaluate takes statements", eval(&page, "var a = 40; a + 2") == 42, eval(&page, "var a = 40; a + 2"));
        let thrown = cdp(&page, "Runtime.evaluate", json!({"expression": "throw new RangeError('boom')", "returnByValue": true})).unwrap_or(Value::Null);
        check("an exception comes back as exceptionDetails", thrown.pointer("/exceptionDetails/exception/description").and_then(Value::as_str).is_some_and(|text| text.contains("boom")), &thrown);
        let undefined = cdp(&page, "Runtime.evaluate", json!({"expression": "undefined", "returnByValue": true})).unwrap_or(Value::Null);
        check("undefined has no value", undefined.pointer("/result/value").is_none() && undefined.pointer("/result/type") == Some(&json!("undefined")), &undefined);
        let by_callback = {
            let (sender, receiver) = mpsc::channel();
            page.eval_with_callback("(function(){ return {ok:true, value:7}; })()", move |result| { let _ = sender.send(result); }).expect("eval");
            receiver.recv_timeout(Duration::from_secs(5)).unwrap_or_default()
        };
        check("eval_with_callback hands back JSON", by_callback == r#"{"ok":true,"value":7}"#, &by_callback);

        // ----- network events, cookies
        wait_until("the page's fetch", Duration::from_secs(5), || eval(&page, "window.apiValue") == 42);
        let seen = events.lock().unwrap().clone();
        let names: Vec<&str> = seen.iter().map(|(name, _)| name.as_str()).collect();
        for event in ["Network.requestWillBeSent", "Network.responseReceived", "Network.loadingFinished", "Page.frameNavigated", "Page.loadEventFired", "Page.domContentEventFired"] {
            check(event, names.contains(&event), &names);
        }
        let api = seen.iter().find(|(name, params)| name == "Network.requestWillBeSent" && params.contains("/api?x=1"));
        check("a fetch is logged with its method", api.is_some_and(|(_, params)| params.contains("\"GET\"")), api);
        let api_id = api.and_then(|(_, params)| serde_json::from_str::<Value>(params).ok()).and_then(|params| params["requestId"].as_str().map(str::to_owned)).unwrap_or_default();
        wait_until("the fetch's body", Duration::from_secs(3), || cdp(&page, "Network.getResponseBody", json!({"requestId": api_id})).is_ok());
        let body = cdp(&page, "Network.getResponseBody", json!({"requestId": api_id}));
        check("a fetch's body can be read back", body.as_ref().is_ok_and(|body| body["body"] == "{\"value\":42}"), &body);
        check("an unknown body is an error", cdp(&page, "Network.getResponseBody", json!({"requestId": "nope"})).is_err(), "getResponseBody");
        check("the document's request carries its status", seen.iter().any(|(name, params)| name == "Network.responseReceived" && params.contains("Document") && params.contains("200")), "document");
        let console = eval(&page, "console.__seen");
        check("the console is observed", console.as_array().is_some_and(|lines| lines.iter().any(|line| line == "warn a warning")), &console);

        let cookies = cdp(&page, "Storage.getCookies", json!({})).unwrap_or(Value::Null);
        let from_server = cookies["cookies"].as_array().and_then(|cookies| cookies.iter().find(|cookie| cookie["name"] == "fromserver")).cloned();
        check("cookies include an HttpOnly one from the server", from_server.as_ref().is_some_and(|cookie| cookie["httpOnly"] == true), &cookies);
        let expiry = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64() + 86_400.0;
        cdp(&page, "Storage.setCookies", json!({"cookies": [
            {"name": "restored", "value": "v1", "domain": "127.0.0.1", "path": "/", "secure": false, "httpOnly": true, "sameSite": "Lax", "expires": expiry, "priority": "Medium", "sourceScheme": "Unset", "sourcePort": -1}
        ]})).expect("set cookies");
        let cookies = cdp(&page, "Network.getCookies", json!({"urls": [format!("{origin}/")]})).unwrap_or(Value::Null);
        let restored = cookies["cookies"].as_array().and_then(|cookies| cookies.iter().find(|cookie| cookie["name"] == "restored")).cloned();
        check("a cookie round trips with HttpOnly, SameSite and expiry", restored.as_ref().is_some_and(|cookie| cookie["httpOnly"] == true && cookie["sameSite"] == "Lax" && cookie["expires"].as_f64().is_some_and(|e| (e - expiry).abs() < 2.0) && cookie["session"] == false), &restored);
        check("cookie matching excludes other hosts", cdp(&page, "Network.getCookies", json!({"urls": ["http://example.test/"]})).is_ok_and(|cookies| cookies["cookies"].as_array().is_some_and(Vec::is_empty)), "other host");

        // ----- screenshots
        let path = output.join("viewport.png");
        let viewport = screenshot(&page, json!({"format": "png"}), &path);
        check("a viewport capture is a PNG", viewport.as_ref().is_ok_and(|bytes| bytes.starts_with(b"\x89PNG")), viewport.as_ref().map(|bytes| bytes.len()));
        let scale = window.scale_factor().unwrap_or(1.0);
        let (width, _) = viewport.as_deref().map(png_size).unwrap_or((0, 0));
        check("it has the view's device pixels", width as f64 == 1000.0 * scale, (width, scale));
        let clip = screenshot(&page, json!({"format": "png", "captureBeyondViewport": true, "clip": {"x": 0, "y": 0, "width": 200, "height": 60, "scale": 1}}), &output.join("clip.png"));
        check("a clip has its own size", clip.as_deref().map(png_size) == Ok((200 * scale as u32, 60 * scale as u32)), clip.as_deref().map(png_size));
        let jpeg = screenshot(&page, json!({"format": "jpeg", "quality": 75, "clip": {"x": 0, "y": 0, "width": 1000, "height": 760, "scale": 0.5}}), &output.join("scaled.jpg"));
        check("a scaled capture is a JPEG", jpeg.as_ref().is_ok_and(|bytes| bytes.starts_with(&[0xff, 0xd8])), jpeg.as_ref().map(|bytes| bytes.len()));
        let full = screenshot(&page, json!({"format": "png", "captureBeyondViewport": true}), &output.join("full.png"));
        let (full_w, full_h) = full.as_deref().map(png_size).unwrap_or((0, 0));
        check("a full-page capture is taller than the view", full_h as f64 > 3000.0 * scale, (full_w, full_h));
        // By its pixels: a capture that only reaches past the view in size is blank down there.
        let pixel = |bytes: &[u8], x: u32, y: u32| image::load_from_memory(bytes).ok().map(|image| image.to_rgb8().get_pixel(x, y).0);
        let full_bytes = full.as_deref().unwrap_or_default();
        let bottom = pixel(full_bytes, 40, full_h.saturating_sub(10));
        check("the bottom of a full-page capture is the page's bottom", bottom.is_some_and(|[r, g, b]| g > 120 && r < 90 && b < 90), bottom);
        let middle = pixel(full_bytes, 40, full_h / 2);
        check("the middle of a full-page capture isn't blank", middle.is_some_and(|[r, g, b]| g > r && g > b && r > 200), middle);
        let top = pixel(full_bytes, 4, 4);
        check("the top of a full-page capture is the top", top.is_some_and(|[r, _, b]| r > 150 && b < 120), top);
        // Scrolled, a clip is still in document coordinates, on both paths.
        eval(&page, "window.scrollTo(0, 500); true");
        std::thread::sleep(Duration::from_millis(100));
        let scrolled = screenshot(&page, json!({"format": "png", "captureBeyondViewport": true, "clip": {"x": 0, "y": 0, "width": 300, "height": 40, "scale": 1}}), &output.join("scrolled-top.png"));
        let hero = scrolled.as_deref().ok().and_then(|bytes| pixel(bytes, 4, 4));
        check("a clip above a scrolled viewport is the document's top", hero.is_some_and(|[r, _, b]| r > 150 && b < 120), hero);
        let inside = screenshot(&page, json!({"format": "png", "captureBeyondViewport": true, "clip": {"x": 0, "y": 600, "width": 300, "height": 40, "scale": 1}}), &output.join("scrolled-inside.png"));
        let green = inside.as_deref().ok().and_then(|bytes| pixel(bytes, 4, 4));
        check("a clip inside a scrolled viewport is that part", green.is_some_and(|[r, g, b]| g > r && g > b), green);
        eval(&page, "window.scrollTo(0, 0); true");

        // ----- layout metrics and the view tracking the pane
        let metrics = cdp(&page, "Page.getLayoutMetrics", json!({})).unwrap_or(Value::Null);
        // The visual viewport leaves out a classic scrollbar, as Chromium's does; the layout viewport doesn't.
        let css_width = metrics.pointer("/cssVisualViewport/clientWidth").and_then(Value::as_f64).unwrap_or(0.0);
        check("layout metrics give the CSS viewport", metrics.pointer("/cssLayoutViewport/clientWidth").and_then(Value::as_f64) == Some(1000.0) && css_width > 900.0, &metrics);
        check("layout metrics give the device ratio", metrics.pointer("/visualViewport/clientWidth").and_then(Value::as_f64) == Some(css_width * scale), &metrics);
        page.set_position(LogicalPosition::new(100.0, 50.0)).expect("position");
        page.set_size(LogicalSize::new(700.0, 500.0)).expect("size");
        std::thread::sleep(Duration::from_millis(100));
        check("the view follows the pane", eval(&page, "innerWidth") == 700 && eval(&page, "innerHeight") == 500, (eval(&page, "innerWidth"), eval(&page, "innerHeight")));
        check("position and size read back", page.position().is_ok() && page.size().is_ok_and(|size| size.width == (700.0 * scale) as u32), page.size());
        screenshot(&page, json!({"format": "png"}), &output.join("moved.png")).ok();
        page.set_bottom_corner_radius(12.0).expect("radius");
        // A snapshot is painted by WebKit and says nothing about where the view is. The window's own
        // composite does: the page 100 from the left and 50 from the top, rounded at the bottom.
        std::thread::sleep(Duration::from_millis(300));
        window_composite(&output.join("window-moved.png"));
        page.set_stacking(true).expect("park");
        std::thread::sleep(Duration::from_millis(300));
        window_composite(&output.join("window-parked.png"));
        page.set_stacking(false).expect("raise");

        // ----- input through real events
        let press = |page: &Webview, x: f64, y: f64| {
            cdp(page, "Input.dispatchMouseEvent", json!({"type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1}))?;
            cdp(page, "Input.dispatchMouseEvent", json!({"type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1}))
        };
        let center = eval(&page, "(() => { const b = document.getElementById('count').getBoundingClientRect(); return {x: b.x + b.width/2, y: b.y + b.height/2}; })()");
        let (x, y) = (center["x"].as_f64().unwrap_or(0.0), center["y"].as_f64().unwrap_or(0.0));
        check("a click call completes", press(&page, x, y).is_ok(), "click");
        check("the click reached the page", eval(&page, "window.clicks") == 1, eval(&page, "window.clicks"));
        let trusted = eval(&page, "window.trustedClick");
        println!("        the click was isTrusted: {trusted}");
        check("the click is a real one", trusted == true, &trusted);
        let console = eval(&page, "console.__seen");
        check("the console saw the click", console.as_array().is_some_and(|lines| lines.iter().any(|line| line == "log clicked 1")), &console);
        // A click on an input moves focus, a default action only real events get.
        let input = eval(&page, "(() => { const b = document.getElementById('name').getBoundingClientRect(); return {x: b.x + b.width/2, y: b.y + b.height/2}; })()");
        press(&page, input["x"].as_f64().unwrap_or(0.0), input["y"].as_f64().unwrap_or(0.0)).ok();
        std::thread::sleep(Duration::from_millis(100));
        check("a click on an input focuses it", eval(&page, "document.activeElement && document.activeElement.id") == "name", eval(&page, "document.activeElement && document.activeElement.id"));

        check("a page in the window isn't hidden", eval(&page, "document.hidden") == false, eval(&page, "document.visibilityState"));
        // Parked, the page is out of the window and still takes the Agent's clicks.
        page.set_stacking(true).expect("park");
        std::thread::sleep(Duration::from_millis(100));
        check("a parked page isn't hidden", eval(&page, "document.hidden") == false, eval(&page, "document.visibilityState"));
        check("a parked page still captures", screenshot(&page, json!({"format": "png"}), &output.join("parked.png")).is_ok_and(|bytes| bytes.len() > 1000), "parked capture");
        check("a parked page still runs", eval(&page, "new Promise(r => setTimeout(() => r(true), 50))") == true, "parked timer");
        let before = eval(&page, "window.clicks").as_i64().unwrap_or(0);
        press(&page, x, y).ok();
        check("a parked page takes a click", eval(&page, "window.clicks").as_i64() == Some(before + 1), eval(&page, "window.clicks"));
        page.set_stacking(false).expect("raise");
        page.set_position(LogicalPosition::new(0.0, 0.0)).ok();
        page.set_size(LogicalSize::new(1000.0, 760.0)).ok();

        // Hidden, it should still answer scripts and captures rather than hang the host.
        page.hide().expect("hide");
        std::thread::sleep(Duration::from_millis(200));
        check("a hidden page answers scripts", eval(&page, "1") == 1, "hidden eval");
        check("a hidden page captures", screenshot(&page, json!({"format": "png"}), &output.join("hidden.png")).is_ok(), "hidden capture");
        page.show().expect("show");

        // ----- DOM, CSS, accessibility
        let document = cdp(&page, "DOM.getDocument", json!({"depth": 0})).unwrap_or(Value::Null);
        let root = document.pointer("/root/nodeId").and_then(Value::as_i64).unwrap_or(0);
        check("DOM.getDocument gives a node", root != 0 && document.pointer("/root/nodeName") == Some(&json!("#document")), &document);
        let found = cdp(&page, "DOM.querySelector", json!({"nodeId": root, "selector": "#name"})).unwrap_or(Value::Null);
        let node = found["nodeId"].as_i64().unwrap_or(0);
        check("DOM.querySelector finds a node", node != 0, &found);
        check("a miss is node 0", cdp(&page, "DOM.querySelector", json!({"nodeId": root, "selector": "#nope"})).is_ok_and(|found| found["nodeId"] == 0), "miss");
        let described = cdp(&page, "DOM.describeNode", json!({"nodeId": node})).unwrap_or(Value::Null);
        check("DOM.describeNode gives attributes", described.pointer("/node/nodeName") == Some(&json!("INPUT")) && described.pointer("/node/attributes").is_some_and(|attributes| attributes.to_string().contains("placeholder")), &described);
        let style = cdp(&page, "CSS.getComputedStyleForNode", json!({"nodeId": node})).unwrap_or(Value::Null);
        check("computed style lists properties", style["computedStyle"].as_array().is_some_and(|list| list.iter().any(|entry| entry["name"] == "display")), "style");
        let boxed = cdp(&page, "DOM.getBoxModel", json!({"nodeId": node})).unwrap_or(Value::Null);
        check("DOM.getBoxModel gives quads", boxed.pointer("/model/content").and_then(Value::as_array).is_some_and(|quad| quad.len() == 8), &boxed);
        let resolved = cdp(&page, "DOM.resolveNode", json!({"backendNodeId": node})).unwrap_or(Value::Null);
        let object_id = resolved.pointer("/object/objectId").and_then(Value::as_str).unwrap_or_default().to_owned();
        let called = cdp(&page, "Runtime.callFunctionOn", json!({"objectId": object_id, "functionDeclaration": "function() { return this.id; }", "returnByValue": true, "awaitPromise": true})).unwrap_or(Value::Null);
        check("Runtime.callFunctionOn runs against the node", called.pointer("/result/value") == Some(&json!("name")), &called);
        let by_ref = cdp(&page, "Runtime.evaluate", json!({"expression": "document.getElementById('name')", "returnByValue": false})).unwrap_or(Value::Null);
        check("evaluate gives a handle when not by value", by_ref.pointer("/result/objectId").is_some(), &by_ref);
        cdp(&page, "Runtime.releaseObject", json!({"objectId": "o1"})).expect("release");
        check("a stale node is an error", cdp(&page, "DOM.describeNode", json!({"backendNodeId": 1})).is_err(), "stale");

        let tree = cdp(&page, "Accessibility.getFullAXTree", json!({})).unwrap_or(Value::Null);
        let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
        let has = |role: &str, name: &str| nodes.iter().any(|node| node.pointer("/role/value") == Some(&json!(role)) && node.pointer("/name/value").and_then(Value::as_str).is_some_and(|text| text.contains(name)));
        check("the tree has a root with the title", nodes.first().is_some_and(|root| root.pointer("/role/value") == Some(&json!("RootWebArea")) && root.pointer("/name/value") == Some(&json!("Engine fixture"))), nodes.first());
        check("the tree has the button", has("button", "Click"), nodes.len());
        check("the tree has the heading", has("heading", "Fixture"), nodes.len());
        check("the tree has the link", has("link", "Second page"), nodes.len());
        check("the tree names the input by its label", has("textbox", "Your name"), nodes.len());
        check("the tree names the image by its alt", has("img", "A red square"), nodes.len());
        check("the tree has the navigation landmark", has("navigation", "Main"), nodes.len());
        check("every node is linked by id", nodes.iter().skip(1).all(|node| node["nodeId"].is_string() && node["backendDOMNodeId"].is_number()), "ids");

        // ----- the element picker
        cdp(&page, "Overlay.setInspectMode", json!({"mode": "searchForNode", "highlightConfig": {"showInfo": true}})).expect("arm");
        check("the picker's overlay is on the page", eval(&page, "!!document.querySelector('[data-mewrk-inspect]')") == true, "overlay");
        let before = eval(&page, "window.clicks");
        press(&page, x, y).ok();
        let picked = wait_until("the pick", Duration::from_secs(3), || events.lock().unwrap().iter().any(|(name, _)| name == "Overlay.inspectNodeRequested"));
        check("a click in inspect mode is a pick", picked, "pick");
        check("a pick doesn't reach the page", eval(&page, "window.clicks") == before, eval(&page, "window.clicks"));
        let pick = events.lock().unwrap().iter().find(|(name, _)| name == "Overlay.inspectNodeRequested").cloned();
        let backend = pick.and_then(|(_, params)| serde_json::from_str::<Value>(&params).ok()).and_then(|params| params["backendNodeId"].as_i64()).unwrap_or(0);
        let described = cdp(&page, "DOM.describeNode", json!({"backendNodeId": backend, "depth": 0})).unwrap_or(Value::Null);
        check("the pick names the element clicked", described.pointer("/node/attributes").is_some_and(|attributes| attributes.to_string().contains("count")), &described);
        cdp(&page, "Overlay.setInspectMode", json!({"mode": "none", "highlightConfig": {}})).expect("disarm");
        cdp(&page, "Overlay.hideHighlight", json!({})).expect("hide");
        check("the overlay leaves with the mode", eval(&page, "!!document.querySelector('[data-mewrk-inspect]')") == false, "overlay gone");

        // ----- files: with no chooser open, and with one
        let upload = output.join("upload.txt");
        std::fs::write(&upload, "file contents").expect("upload");
        let input = cdp(&page, "DOM.querySelector", json!({"nodeId": root, "selector": "#file"})).unwrap_or(Value::Null)["nodeId"].as_i64().unwrap_or(0);
        let set = cdp(&page, "DOM.setFileInputFiles", json!({"backendNodeId": input, "files": [upload.display().to_string()]}));
        check("files reach an input with no chooser", set.is_ok(), &set);
        check("the input has the file", eval(&page, "document.getElementById('file').files[0].name") == "upload.txt", eval(&page, "document.getElementById('file').files[0]?.name"));
        check("the file has its bytes", eval(&page, "document.getElementById('file').files[0].text()") == "file contents", "bytes");
        eval(&page, "document.getElementById('file').value = ''");
        let chooser = eval(&page, "(() => { const b = document.getElementById('file').getBoundingClientRect(); return {x: b.x + 8, y: b.y + b.height/2}; })()");
        press(&page, chooser["x"].as_f64().unwrap_or(0.0), chooser["y"].as_f64().unwrap_or(0.0)).ok();
        let opened = wait_until("the file chooser", Duration::from_secs(2), || events.lock().unwrap().iter().any(|(name, _)| name == "Page.fileChooserOpened"));
        check("a click on a file input opens a held chooser", opened, "chooser");
        if opened {
            let event = events.lock().unwrap().iter().find(|(name, _)| name == "Page.fileChooserOpened").cloned();
            check("the chooser names its input", event.as_ref().is_some_and(|(_, params)| params.contains(&input.to_string())), &event);
            let set = cdp(&page, "DOM.setFileInputFiles", json!({"backendNodeId": input, "files": [upload.display().to_string()]}));
            check("an open chooser takes the file", set.is_ok(), &set);
            let arrived = wait_until("the file", Duration::from_secs(2), || eval(&page, "document.getElementById('file').files.length") == 1);
            check("the chooser's file reaches the page", arrived, "chooser file");
        }

        // ----- dialogs
        let held = Arc::new(Mutex::new(Vec::new()));
        {
            let held = held.clone();
            page.set_dialog_handler(Some(Arc::new(move |dialog: PageDialog| {
                held.lock().unwrap().push((dialog.kind, dialog.message.clone(), dialog.default_text.clone()));
                DialogDecision::Hold(77)
            })));
        }
        let (sender, receiver) = mpsc::channel();
        page.eval_with_callback("(function(){ return prompt('Your favourite?', 'blue'); })()", move |result| { let _ = sender.send(result); }).expect("prompt");
        let shown = wait_until("the dialog", Duration::from_secs(3), || !held.lock().unwrap().is_empty());
        check("a prompt is held", shown, "dialog");
        check("the prompt blocks the page", receiver.recv_timeout(Duration::from_millis(200)).is_err(), "blocked");
        check("the prompt carries its text", held.lock().unwrap().first().is_some_and(|(kind, message, default)| *kind == "prompt" && message == "Your favourite?" && default == "blue"), "see the log above");
        page.answer_dialog(77, true, Some("green".to_owned())).expect("answer");
        check("the answer reaches the page", receiver.recv_timeout(Duration::from_secs(3)).as_deref() == Ok("\"green\""), "answer");
        check("a dialog can only be answered once", page.answer_dialog(77, true, None).is_err(), "twice");
        let (sender, receiver) = mpsc::channel();
        page.eval_with_callback("(function(){ return confirm('Sure?'); })()", move |result| { let _ = sender.send(result); }).expect("confirm");
        wait_until("the confirm", Duration::from_secs(3), || held.lock().unwrap().len() == 2);
        page.answer_dialog(77, false, None).expect("dismiss");
        check("a dismissed confirm is false", receiver.recv_timeout(Duration::from_secs(3)).as_deref() == Ok("false"), "confirm");
        page.set_dialog_handler(None);

        // ----- navigation policy, new windows, downloads, failures
        page.navigate(url::Url::parse(&format!("{origin}/forbidden.html")).unwrap()).expect("navigate");
        wait_until("the veto", Duration::from_secs(3), || !blocked.lock().unwrap().is_empty());
        check("the policy vetoes before the request", blocked.lock().unwrap().iter().any(|url| url.contains("forbidden")) && !requests.lock().unwrap().iter().any(|path: &String| path.contains("forbidden")), "see the log above");
        check("a vetoed page stays put", page.url().is_ok_and(|url| url.as_str().ends_with("/index.html")), page.url());
        eval(&page, "window.open('/second.html?popup=1'); true");
        let popped = wait_until("window.open", Duration::from_secs(3), || new_windows.lock().unwrap().iter().any(|url| url.contains("popup=1")));
        check("window.open reaches the handler and opens nothing", popped, new_windows.lock().unwrap().clone());
        let popup = eval(&page, "(() => { const b = document.getElementById('popup').getBoundingClientRect(); return {x: b.x + 4, y: b.y + b.height/2}; })()");
        new_windows.lock().unwrap().clear();
        press(&page, popup["x"].as_f64().unwrap_or(0.0), popup["y"].as_f64().unwrap_or(0.0)).ok();
        let popped = wait_until("target=_blank", Duration::from_secs(3), || new_windows.lock().unwrap().iter().any(|url| url.contains("second.html")));
        check("target=_blank reaches the handler", popped, new_windows.lock().unwrap().clone());

        let finished_before = loads.lock().unwrap().iter().filter(|(event, _)| *event == PageLoadEvent::Finished).count();
        page.navigate(url::Url::parse(&format!("{origin}/download.bin")).unwrap()).expect("navigate");
        wait_until("the refused download", Duration::from_secs(3), || loads.lock().unwrap().iter().filter(|(event, _)| *event == PageLoadEvent::Finished).count() > finished_before);
        check("a download is refused and the page stays", page.url().is_ok_and(|url| url.as_str().ends_with("/index.html")), page.url());

        page.navigate(url::Url::parse("http://127.0.0.1:1/unreachable").unwrap()).expect("navigate");
        let failed = wait_until("the failed load", Duration::from_secs(10), || loads.lock().unwrap().iter().any(|(event, url)| *event == PageLoadEvent::Finished && url.contains("127.0.0.1:1/")));
        check("a failed load finishes on the address that failed", failed, loads.lock().unwrap().iter().rev().take(4).collect::<Vec<_>>());
        std::thread::sleep(Duration::from_millis(200));
        screenshot(&page, json!({"format": "png"}), &output.join("failure.png")).ok();
        let failure_text = eval(&page, "document.body ? document.body.innerText : ''");
        check("a failed load leaves a readable page", failure_text.as_str().is_some_and(|text| text.contains("can't be opened")), &failure_text);
        check("a failed load is a failed request", events.lock().unwrap().iter().any(|(name, params)| name == "Network.loadingFailed" && params.contains("Document")), "loadingFailed");

        // ----- history
        page.navigate(url::Url::parse(&format!("{origin}/index.html")).unwrap()).expect("navigate");
        wait_until("back on the page", Duration::from_secs(5), || eval(&page, "document.title") == "Engine fixture");
        page.navigate(url::Url::parse(&format!("{origin}/second.html")).unwrap()).expect("navigate");
        wait_until("the second page", Duration::from_secs(5), || eval(&page, "document.title") == "Second");
        let history = cdp(&page, "Page.getNavigationHistory", json!({})).unwrap_or(Value::Null);
        let current = history["currentIndex"].as_u64().unwrap_or(0) as usize;
        check("history knows where it stands", history["entries"][current]["url"].as_str().is_some_and(|url| url.ends_with("/second.html")) && history["entries"][current - 1]["url"].as_str().is_some_and(|url| url.ends_with("/index.html")), &history);
        eval(&page, "history.back(); true");
        wait_until("history.back", Duration::from_secs(5), || eval(&page, "document.title") == "Engine fixture");
        check("history.back works", eval(&page, "document.title") == "Engine fixture", eval(&page, "document.title"));

        // ----- emulation
        let dark = cdp(&page, "Emulation.setEmulatedMedia", json!({"features": [{"name": "prefers-color-scheme", "value": "dark"}]}));
        std::thread::sleep(Duration::from_millis(100));
        check("prefers-color-scheme follows", dark.is_ok() && eval(&page, "matchMedia('(prefers-color-scheme: dark)').matches") == true, eval(&page, "matchMedia('(prefers-color-scheme: dark)').matches"));
        cdp(&page, "Emulation.setEmulatedMedia", json!({"features": [{"name": "prefers-color-scheme", "value": "light"}]})).ok();
        std::thread::sleep(Duration::from_millis(100));
        check("and back", eval(&page, "matchMedia('(prefers-color-scheme: light)').matches") == true, "light");
        check("another media feature is an error", cdp(&page, "Emulation.setEmulatedMedia", json!({"features": [{"name": "prefers-reduced-motion", "value": "reduce"}]})).is_err(), "reduced motion");
        cdp(&page, "Emulation.setDeviceMetricsOverride", json!({"width": 390, "height": 844, "deviceScaleFactor": 2, "mobile": true})).expect("metrics");
        cdp(&page, "Emulation.setUserAgentOverride", json!({"userAgent": "Mozilla/5.0 (Linux; Android 14) Chrome/132.0.0.0 Mobile"})).expect("agent");
        cdp(&page, "Emulation.setTouchEmulationEnabled", json!({"enabled": true, "maxTouchPoints": 5})).expect("touch");
        std::thread::sleep(Duration::from_millis(150));
        check("the emulated viewport is the CSS viewport", eval(&page, "innerWidth") == 390 && eval(&page, "innerHeight") == 844, (eval(&page, "innerWidth"), eval(&page, "innerHeight")));
        check("the device ratio is overridden", eval(&page, "devicePixelRatio") == 2, eval(&page, "devicePixelRatio"));
        check("the touch points are overridden", eval(&page, "navigator.maxTouchPoints") == 5, eval(&page, "navigator.maxTouchPoints"));
        check("the user agent is overridden", eval(&page, "navigator.userAgent").as_str().is_some_and(|agent| agent.contains("Android")), eval(&page, "navigator.userAgent"));
        screenshot(&page, json!({"format": "png"}), &output.join("emulated.png")).ok();
        page.reload().expect("reload");
        std::thread::sleep(Duration::from_millis(500));
        check("emulation survives a reload", eval(&page, "navigator.maxTouchPoints") == 5 && eval(&page, "devicePixelRatio") == 2, (eval(&page, "navigator.maxTouchPoints"), eval(&page, "devicePixelRatio")));
        cdp(&page, "Emulation.clearDeviceMetricsOverride", json!({})).expect("clear");
        cdp(&page, "Emulation.setUserAgentOverride", json!({"userAgent": ""})).expect("clear agent");
        cdp(&page, "Emulation.setTouchEmulationEnabled", json!({"enabled": false})).expect("clear touch");
        std::thread::sleep(Duration::from_millis(150));
        check("clearing gives the pane's viewport back", eval(&page, "innerWidth") == 1000, eval(&page, "innerWidth"));
        check("clearing takes the overrides back", eval(&page, "navigator.maxTouchPoints") == 0, eval(&page, "navigator.maxTouchPoints"));
        page.set_zoom(1.5).expect("zoom");
        std::thread::sleep(Duration::from_millis(100));
        check("zoom scales the CSS viewport", eval(&page, "innerWidth") == 666, eval(&page, "innerWidth"));
        page.set_zoom(1.0).expect("zoom");

        // ----- unavailable methods fail at once
        let start = Instant::now();
        let unknown = cdp(&page, "Debugger.enable", json!({}));
        check("an unknown method is a readable error, at once", unknown.as_ref().is_err_and(|error| error.contains("not available")) && start.elapsed() < Duration::from_secs(1), &unknown);
        check("a mouse wheel says it isn't available", cdp(&page, "Input.dispatchMouseEvent", json!({"type": "mouseWheel", "x": 1, "y": 1})).is_err_and(|error| error.contains("not available")), "wheel");

        // ----- the remote screen's MJPEG stream
        page.navigate(url::Url::parse(&format!("{origin}/viewer.html")).unwrap()).expect("navigate");
        std::thread::sleep(Duration::from_millis(700));
        let mut sizes = std::collections::BTreeSet::new();
        wait_until("two frames", Duration::from_secs(3), || {
            sizes.insert(eval(&page, "document.getElementById('screen').naturalWidth").as_i64().unwrap_or(0));
            sizes.iter().filter(|size| **size > 0).count() >= 2
        });
        println!("        MJPEG frame sizes seen: {sizes:?}");
        check("a multipart stream in an img plays", sizes.iter().filter(|size| **size > 0).count() >= 2, &sizes);
        // ----- local files through the scheme handler
        let site = output.join("site");
        std::fs::create_dir_all(site.join("css")).expect("site");
        std::fs::write(site.join("index.html"), "<!doctype html><title>Local</title><link rel=stylesheet href=css/a.css><h1 id=h>Local file</h1><a id=out href='other.html'>other</a><script>fetch('data.json').then(r=>r.json()).then(j=>window.data=j.v)</script>").expect("html");
        std::fs::write(site.join("css/a.css"), "h1 { color: rgb(1, 2, 3) }").expect("css");
        std::fs::write(site.join("data.json"), "{\"v\":5}").expect("json");
        std::fs::write(site.join("other.html"), "<!doctype html><title>Other</title>").expect("other");
        page.map_virtual_host_folder("mewrk-file-preview.invalid", &site).expect("map");
        page.navigate(url::Url::parse("https://mewrk-file-preview.invalid/index.html").unwrap()).expect("navigate");
        wait_until("the local page", Duration::from_secs(5), || eval(&page, "document.title") == "Local");
        check("a mapped host serves the folder", eval(&page, "document.title") == "Local", eval(&page, "document.title"));
        check("the address is the mapped one", page.url().is_ok_and(|url| url.as_str() == "https://mewrk-file-preview.invalid/index.html"), page.url());
        check("the load reports the mapped address", loads.lock().unwrap().iter().any(|(event, url)| *event == PageLoadEvent::Finished && url == "https://mewrk-file-preview.invalid/index.html"), "load");
        check("a relative stylesheet loads", eval(&page, "getComputedStyle(document.getElementById('h')).color") == "rgb(1, 2, 3)", eval(&page, "getComputedStyle(document.getElementById('h')).color"));
        wait_until("a relative fetch", Duration::from_secs(3), || eval(&page, "window.data") == 5);
        check("a relative fetch loads", eval(&page, "window.data") == 5, eval(&page, "window.data"));
        check("nothing outside the folder is served", eval(&page, "fetch('../../secret').then(r => r.status)") == 404, eval(&page, "fetch('../../secret').then(r => r.status)"));
        page.clear_virtual_host_folder("mewrk-file-preview.invalid").expect("clear");

        // ----- another machine's network: every connection through a SOCKS endpoint, loopback included
        let socks_targets = Arc::new(Mutex::new(Vec::new()));
        let socks_port = serve_socks(socks_targets.clone(), port);
        let set = page.set_network_proxy(Some(format!("socks5://127.0.0.1:{socks_port}")));
        check("a proxy can be set on a live page", set.is_ok(), &set);
        page.navigate(url::Url::parse("http://only-on-the-machine.invalid/second.html").unwrap()).expect("navigate");
        let through = wait_until("a page through the proxy", Duration::from_secs(8), || eval(&page, "document.title") == "Second");
        check("a name only the machine knows loads through the proxy", through, socks_targets.lock().unwrap().clone());
        // The dev-server case: `localhost` has to mean the machine's, not this computer's.
        page.navigate(url::Url::parse(&format!("http://localhost:{port}/index.html")).unwrap()).expect("navigate");
        wait_until("localhost", Duration::from_secs(8), || eval(&page, "document.title") == "Engine fixture");
        let targets = socks_targets.lock().unwrap().clone();
        println!("        SOCKS saw: {targets:?}");
        check("the machine's name went through the proxy", targets.iter().any(|target: &String| target.starts_with("only-on-the-machine.invalid")), &targets);
        // WebKit refuses to proxy loopback, so it goes by the alias the host's tunnel resolves.
        check("localhost goes through the proxy too", targets.iter().any(|target: &String| target.starts_with("machine.localhost")), &targets);
        check("the host still sees localhost", page.url().is_ok_and(|url| url.as_str().starts_with("http://localhost:")), page.url());
        let finished_at_localhost = loads.lock().unwrap().iter().any(|(event, url)| *event == PageLoadEvent::Finished && url.starts_with("http://localhost:"));
        check("the load events say localhost", finished_at_localhost, "see the log above");
        let history = cdp(&page, "Page.getNavigationHistory", json!({})).unwrap_or(Value::Null);
        check("so does the history", history.to_string().contains("http://localhost:") && !history.to_string().contains("machine.localhost"), &history);
        socks_targets.lock().unwrap().clear();
        // No-cors, because `127.0.0.1` from a page at `localhost` is cross-origin wherever it runs.
        let answered = eval(&page, &format!("fetch('http://127.0.0.1:{port}/api?aliased=1', {{mode: 'no-cors'}}).then(r => r.type)"));
        let seen = socks_targets.lock().unwrap().clone();
        check("the page's own fetch of loopback goes through the proxy", answered == "opaque" && seen.iter().any(|target: &String| target.starts_with("ipv4.machine.localhost")), (&answered, &seen));
        page.set_network_proxy(None).expect("direct again");
        let before = socks_targets.lock().unwrap().len();
        page.navigate(url::Url::parse(&format!("{origin}/second.html")).unwrap()).expect("navigate");
        wait_until("a direct page", Duration::from_secs(5), || eval(&page, "document.title") == "Second");
        check("without a proxy the page goes direct", socks_targets.lock().unwrap().len() == before && eval(&page, "document.title") == "Second", eval(&page, "document.title"));

        // ----- crash, sleep, a second profile, close
        page.set_sleeping(true).expect("sleep");
        std::thread::sleep(Duration::from_millis(200));
        page.set_sleeping(false).expect("wake");
        std::thread::sleep(Duration::from_millis(300));
        check("a page wakes with its state", eval(&page, "document.title") == "Second", eval(&page, "document.title"));

        let profile_b = data.join("profile-b");
        std::fs::create_dir_all(&profile_b).expect("profile b");
        let other = window.add_child(
            WebviewBuilder::new("page-b", WebviewUrl::External(url::Url::parse(&format!("{origin}/second.html")).unwrap())).data_directory(profile_b),
            LogicalPosition::new(0.0, 0.0),
            LogicalSize::new(400.0, 300.0),
        ).expect("second page");
        std::thread::sleep(Duration::from_millis(500));
        let cookies = cdp(&other, "Storage.getCookies", json!({})).unwrap_or(Value::Null);
        check("two profiles don't share cookies", cookies["cookies"].as_array().is_some_and(Vec::is_empty), &cookies);
        other.close().expect("close");
        check("a closed page no longer resolves", handle.get_webview("page-b").is_none(), "closed");
        check("a closed page's calls fail", cdp(&other, "Page.enable", json!({})).is_err(), "closed cdp");

        // The web process dying: the host resets the page on this.
        let crashes = Arc::new(AtomicUsize::new(0));
        {
            let crashes = crashes.clone();
            page.set_crash_handler(Some(Arc::new(move |_| {
                crashes.fetch_add(1, Ordering::SeqCst);
            })));
        }
        match page.web_process_id() {
            Some(pid) => {
                // SAFETY: the process this page's own engine started for it, and nothing else.
                unsafe { kill(pid, 9) };
                let heard = wait_until("the crash", Duration::from_secs(5), || crashes.load(Ordering::SeqCst) == 1);
                check("a dead web process reaches the crash handler", heard, crashes.load(Ordering::SeqCst));
                page.reload().ok();
                check("the page comes back after a reload", wait_until("the reload", Duration::from_secs(8), || eval(&page, "1 + 1") == 2), "reload");
            }
            None => println!("        (this WebKit doesn't say which process renders the page: crash not exercised)"),
        }
        page.set_crash_handler(None);

        // A cold close: the same profile, a new view, and the store still has what it had.
        page.close().expect("close");
        let again = window.add_child(
            WebviewBuilder::new("page-a", WebviewUrl::External(url::Url::parse("about:blank").unwrap())).data_directory(profile.clone()),
            LogicalPosition::new(0.0, 0.0),
            LogicalSize::new(1000.0, 760.0),
        ).expect("reopened");
        std::thread::sleep(Duration::from_millis(300));
        let cookies = cdp(&again, "Storage.getCookies", json!({})).unwrap_or(Value::Null);
        check("a profile outlives its page while the directory stands", cookies["cookies"].as_array().is_some_and(|cookies| cookies.iter().any(|cookie| cookie["name"] == "restored")), &cookies);
        again.close().expect("close");
        let _ = std::fs::remove_dir_all(&data);
    }
}
