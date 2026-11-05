//! The whole path a pane uses, in a real window: the host's own commands and page tools, over the
//! WKWebView engine. Where `wk_engine` checks the adapter, this checks that `browser.rs` runs on it.
//!
//!     cargo run -p mewrk-pane-browser --example wk_host -- [output directory]

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
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use base64::Engine as _;
    use mewrk_client::{Host, HostConfig};
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use serde_json::{json, Value};

    const PAGE: &str = r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Host fixture</title>
<style>body{margin:0;font:16px -apple-system,sans-serif;background:#fff}#hero{background:#36c;color:#fff;padding:20px}</style></head>
<body><div id="hero">Host fixture</div>
<button id="go" class="primary big" data-testid="go" aria-label="Go now" onclick="window.clicks=(window.clicks||0)+1;this.textContent='Done '+window.clicks;console.log('went',window.clicks)">Go</button>
<input id="name" placeholder="your name">
<a href="/second.html">Second</a>
<div id="filler" style="height:200px"></div>
<script>document.cookie='fromscript=1; path=/; max-age=9999'; fetch('/api').then(r=>r.text()).then(t=>console.log('api',t)); console.error('an error line');</script>
</body></html>"##;

    fn serve() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 8192];
                    let Ok(read) = stream.read(&mut buffer) else { return };
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let cookie = request.lines().find(|line| line.to_ascii_lowercase().starts_with("cookie:")).unwrap_or("").to_owned();
                    let (status, kind, body): (&str, &str, String) = match path.as_str() {
                        "/" | "/index.html" => ("200 OK", "text/html", PAGE.to_owned()),
                        "/second.html" => ("200 OK", "text/html", "<!doctype html><title>Second</title><h1>Second</h1>".to_owned()),
                        "/echo" => ("200 OK", "text/html", format!("<!doctype html><title>Echo</title><pre id=c>{cookie}</pre>")),
                        "/api" => ("200 OK", "text/plain", "hello".to_owned()),
                        "/gone" => ("500 Internal Server Error", "text/plain", "broken".to_owned()),
                        _ => ("404 Not Found", "text/plain", "nope".to_owned()),
                    };
                    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nSet-Cookie: login=secret; Path=/; HttpOnly\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                });
            }
        });
        port
    }

    static FAILURES: AtomicUsize = AtomicUsize::new(0);
    static WINDOW_NUMBER: AtomicUsize = AtomicUsize::new(0);

    fn check(name: &str, passed: bool, detail: impl std::fmt::Debug) {
        if passed {
            println!("ok      {name}");
        } else {
            FAILURES.fetch_add(1, Ordering::SeqCst);
            println!("FAILED  {name}: {detail:?}");
        }
    }

    fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("        (timed out waiting for {what})");
        false
    }

    pub fn main() {
        let output = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| std::env::temp_dir().join("wk-host").display().to_string()));
        std::fs::create_dir_all(&output).expect("output directory");
        mewrk_client::prepare_process();
        let mtm = MainThreadMarker::new().expect("main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        let frame = NSRect::new(NSPoint::new(160.0, 160.0), NSSize::new(1000.0, 760.0));
        // SAFETY: on the main thread, with the initializer's documented arguments.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), frame, NSWindowStyleMask::Titled | NSWindowStyleMask::Resizable, NSBackingStoreType::Buffered, false)
        };
        // SAFETY: the window outlives the process's only run loop.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("wk_host"));
        window.makeKeyAndOrderFront(None);
        WINDOW_NUMBER.store(window.windowNumber() as usize, Ordering::SeqCst);
        let view = window.contentView().expect("content view");
        tauri::webview::attach_main_view(Retained::as_ptr(&view) as *mut std::ffi::c_void);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| run(&output));
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

    fn composite(path: &PathBuf) {
        let number = WINDOW_NUMBER.load(Ordering::SeqCst);
        let _ = std::process::Command::new("/usr/sbin/screencapture").args(["-x", "-o", &format!("-l{number}")]).arg(path).status();
    }

    fn run(output: &PathBuf) {
        let port = serve();
        let origin = format!("http://127.0.0.1:{port}");
        let data = std::env::temp_dir().join(format!("wk-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).expect("data");
        let host = Host::start(
            HostConfig {
                identifier: "dev.mewrk.wkhost".to_owned(),
                version: "0".to_owned(),
                data_dir: data.clone(),
                local_data_dir: data.clone(),
                resource_dir: None,
                run_on_main: None,
                exit: None,
                show_window: None,
            },
            Arc::new(|_, _| {}),
        )
        .map_err(|error| format!("{error:?}"))
        .expect("the host starts");
        // What the pane's `attach_window` does: without it the host hides every page after 12 s.
        {
            let host = host.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(4));
                let _ = host.invoke_blocking("browser_renderer_mount_heartbeat", json!({}));
            });
        }
        let session = "wk-host-session";
        let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 * 1024;
        let call = |command: &str, args: Value| host.invoke_blocking(command, args);
        let action = |name: &str, value: Value| call("browser_action", json!({"sessionId": session, "action": name, "value": value}));
        let status = || call("browser_status", json!({"sessionId": session})).unwrap_or(Value::Null);
        let tool = |name: &str, input: Value| host.run_preview_tool(session, name, input);
        let loaded = |url_end: &str| {
            let end = url_end.to_owned();
            wait_until(url_end, Duration::from_secs(10), || {
                let status = status();
                status["loading"] == false && status["url"].as_str().is_some_and(|url| url.ends_with(&end))
            })
        };

        // ----- opening: the profile attestation, the start page, the observers
        let declared = action("project", json!(false));
        check("the pane can declare before a page exists", declared.is_ok(), &declared);
        let opened = call("browser_open", json!({"sessionId": session, "lifecycleEpoch": epoch}));
        check("browser_open makes a page", opened.as_ref().is_ok_and(|status| status["hasPage"] == true), &opened);
        check("the start page is about:blank", opened.as_ref().is_ok_and(|status| status["url"] == "about:blank"), &opened);
        let bounds = json!({"x": 120.0, "y": 60.0, "width": 800.0, "height": 600.0, "visible": true, "bottomCornerRadius": 10.0});
        let placed = call("browser_set_panel_bounds", json!({"sessionId": session, "bounds": bounds, "lifecycleEpoch": epoch}));
        check("the panel bounds are taken", placed.as_ref().is_ok_and(|status| status["open"] == true && status["viewport"]["width"] == 800.0), &placed);
        let labels = tauri::webview::live_page_labels();
        check("one native page exists", labels.len() == 1, &labels);
        println!("        page label: {labels:?}");
        std::thread::sleep(Duration::from_millis(200));
        composite(&output.join("host-start.png"));

        // ----- navigating
        let navigated = call("browser_navigate", json!({"sessionId": session, "url": format!("{origin}/index.html")}));
        check("browser_navigate is accepted", navigated.is_ok(), &navigated);
        check("the page finishes loading", loaded("/index.html"), status());
        check("the status has the title", status()["title"] == "Host fixture", status());
        std::thread::sleep(Duration::from_millis(300));
        composite(&output.join("host-page.png"));

        let refused = call("browser_navigate", json!({"sessionId": session, "url": "file:///etc/passwd"}));
        check("the navigation policy refuses file:", refused.is_err(), &refused);

        // ----- the captures the pane paints instead of the page
        let captured = call("browser_capture_page", json!({"sessionId": session})).unwrap_or(Value::Null);
        let png = captured["data"].as_str().map(|data| base64::engine::general_purpose::STANDARD.decode(data).unwrap_or_default()).unwrap_or_default();
        check("browser_capture_page gives a PNG", png.starts_with(b"\x89PNG") && captured["width"].as_u64().unwrap_or(0) >= 800, captured.get("width"));
        std::fs::write(output.join("host-capture.png"), &png).ok();

        // ----- stacking: project and occlude
        let projected = action("project", json!(true));
        check("project parks the page", projected.as_ref().is_ok_and(|status| status["projected"] == true), &projected);
        std::thread::sleep(Duration::from_millis(250));
        composite(&output.join("host-parked.png"));
        let occluded = action("occlude", json!(true));
        check("occlude is reported", occluded.as_ref().is_ok_and(|status| status["occluded"] == true), &occluded);
        action("occlude", json!(false)).ok();
        let raised = action("project", json!(false));
        check("the page comes back up", raised.as_ref().is_ok_and(|status| status["projected"] != true && status["occluded"] != true), &raised);
        std::thread::sleep(Duration::from_millis(250));
        composite(&output.join("host-raised.png"));

        // ----- the page tools the model uses: the person navigated, so the page is theirs until given back
        let tool_blocked = tool("preview_snapshot", json!({}));
        check("a page the user holds refuses the model, in words", tool_blocked.as_ref().is_err_and(|error| error.contains("the user is using it")), &tool_blocked);
        check("the pane hears the request", status()["control"]["handoffRequested"] == true, status()["control"].clone());
        let given = action("handoff_agent", Value::Null);
        check("the user gives it back", given.is_ok(), &given);
        let snapshot = tool("preview_snapshot", json!({}));
        println!("        preview_snapshot:\n{}", snapshot.as_deref().unwrap_or("").lines().take(12).map(|line| format!("          {line}")).collect::<Vec<_>>().join("\n"));
        check("preview_snapshot lists the button", snapshot.as_deref().is_ok_and(|text| text.contains("button") && text.contains("Go now")), &snapshot);
        let uid = snapshot.as_deref().ok().and_then(|text| text.lines().find(|line| line.contains("button"))).and_then(|line| line.trim().strip_prefix('[')).and_then(|rest| rest.split(']').next()).and_then(|uid| uid.parse::<u64>().ok());
        let clicked = tool("preview_click", json!({"selector": "#go"}));
        check("preview_click by selector", clicked.is_ok(), &clicked);
        let eval = |expression: &str| tool("preview_eval", json!({"expression": expression}));
        check("preview_eval reads what the click did", eval("window.clicks").as_deref().is_ok_and(|text| text.contains('1')), eval("window.clicks"));
        if let Some(uid) = uid {
            let clicked = tool("preview_click", json!({"uid": uid}));
            check("preview_click by snapshot uid", clicked.is_ok(), &clicked);
            check("the uid's click landed", eval("window.clicks").as_deref().is_ok_and(|text| text.contains('2')), eval("window.clicks"));
        }
        let filled = tool("preview_fill", json!({"selector": "#name", "value": "Ada"}));
        check("preview_fill", filled.is_ok() && eval("document.getElementById('name').value").as_deref().is_ok_and(|text| text.contains("Ada")), &filled);
        let inspected = tool("preview_inspect", json!({"selector": "#go"}));
        check("preview_inspect gives styles and a box", inspected.as_deref().is_ok_and(|text| text.contains("display") && text.contains("boundingBox") && text.contains("button")), &inspected);
        check("preview_inspect says when nothing matches", tool("preview_inspect", json!({"selector": "#nope"})).as_deref().is_ok_and(|text| text.contains("not found")), "miss");
        let console = tool("preview_console_logs", json!({}));
        check("preview_console_logs", console.as_deref().is_ok_and(|text| text.contains("went 1") && text.contains("an error line")), &console);
        let errors = tool("preview_console_logs", json!({"level": "error"}));
        check("preview_console_logs filters", errors.as_deref().is_ok_and(|text| text.contains("an error line") && !text.contains("went 1")), &errors);
        let network = tool("preview_network", json!({}));
        println!("        preview_network:\n{}", network.as_deref().unwrap_or("").lines().take(6).map(|line| format!("          {line}")).collect::<Vec<_>>().join("\n"));
        check("preview_network lists the page and its fetch", network.as_deref().is_ok_and(|text| text.contains("/index.html") && text.contains("/api") && text.contains("200")), &network);
        let shot = tool("preview_screenshot", json!({}));
        check("preview_screenshot", shot.as_deref().is_ok_and(|text| text.starts_with("image ")), shot.as_ref().map(|text| text.chars().take(40).collect::<String>()));
        if let Ok(text) = &shot {
            if let Some(data) = text.split(' ').nth(2) {
                std::fs::write(output.join("host-preview-shot.jpg"), base64::engine::general_purpose::STANDARD.decode(data).unwrap_or_default()).ok();
            }
        }
        let resized = tool("preview_resize", json!({"preset": "mobile"}));
        check("preview_resize mobile", resized.is_ok(), &resized);
        std::thread::sleep(Duration::from_millis(200));
        let inner = eval("innerWidth + 'x' + navigator.maxTouchPoints");
        println!("        mobile: {inner:?}");
        check("the mobile preset applies", inner.as_deref().is_ok_and(|text| text.contains("375x5")), &inner);
        std::thread::sleep(Duration::from_millis(100));
        composite(&output.join("host-mobile.png"));
        let dark = tool("preview_resize", json!({"colorScheme": "dark"}));
        check("preview_resize colorScheme", dark.is_ok() && eval("matchMedia('(prefers-color-scheme: dark)').matches").as_deref().is_ok_and(|text| text.contains("true")), &dark);
        tool("preview_resize", json!({"colorScheme": "light"})).ok();
        let cleared = tool("preview_resize", json!({"preset": "desktop"}));
        check("preview_resize desktop", cleared.is_ok(), &cleared);
        std::thread::sleep(Duration::from_millis(200));
        check("desktop gives the pane back", eval("innerWidth").as_deref().is_ok_and(|text| text.contains("800")), eval("innerWidth"));

        // ----- a dialog on the model's page, and on the user's
        let started = tool("preview_eval", json!({"expression": "setTimeout(() => { window.answer = prompt('Name?', 'Bo'); }, 0); 1"}));
        check("a script can open a prompt", started.is_ok(), &started);
        std::thread::sleep(Duration::from_millis(300));
        let blocked = tool("preview_eval", json!({"expression": "1"}));
        println!("        with a dialog open: {:?}", blocked.as_ref().map_err(|error| error.chars().take(100).collect::<String>()));
        check("other tools are refused while a dialog is open", blocked.is_err(), &blocked);
        let answered = tool("preview_dialog", json!({"accept": true, "prompt_text": "Ada"}));
        check("preview_dialog answers it", answered.is_ok(), &answered);
        check("the page has the answer", eval("window.answer").as_deref().is_ok_and(|text| text.contains("Ada")), eval("window.answer"));
        // Alerts route through the prompt with a marker in the page's script: the same dialog tool.
        tool("preview_eval", json!({"expression": "setTimeout(() => { alert('Hello'); window.alerted = true; }, 0); 1"})).ok();
        std::thread::sleep(Duration::from_millis(300));
        let answered = tool("preview_dialog", json!({"accept": true}));
        check("an alert is answered the same way", answered.as_deref().is_ok_and(|text| text.to_lowercase().contains("alert")), &answered);

        // ----- the element picker, end to end
        let armed = action("select_element", json!(true));
        check("the picker arms", armed.as_ref().is_ok_and(|status| status["elementPicker"]["armed"] == true), &armed);
        let page = tauri::webview::live_page(&labels[0]).expect("the page");
        let box_of = page.call_devtools_blocking("Runtime.evaluate", &json!({"expression": "(() => { const b = document.getElementById('go').getBoundingClientRect(); return {x: b.x + b.width/2, y: b.y + b.height/2}; })()", "returnByValue": true}).to_string(), Duration::from_secs(5)).unwrap_or_default();
        let center: Value = serde_json::from_str(&box_of).unwrap_or(Value::Null);
        let (x, y) = (center["result"]["value"]["x"].as_f64().unwrap_or(0.0), center["result"]["value"]["y"].as_f64().unwrap_or(0.0));
        let before = eval("window.clicks");
        for kind in ["mousePressed", "mouseReleased"] {
            page.call_devtools_blocking("Input.dispatchMouseEvent", &json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1}).to_string(), Duration::from_secs(5)).ok();
        }
        check("the picker reports a waiting pick", wait_until("the pick", Duration::from_secs(3), || status()["elementPicker"]["pendingPick"] == true), status());
        let picked = call("browser_take_selected_element", json!({"sessionId": session})).unwrap_or(Value::Null);
        check("the pick has the element's tag and id", picked["tagName"] == "button" && picked["id"] == "go", &picked["tagName"]);
        check("the pick has its classes", picked["classes"] == json!(["primary", "big"]), &picked["classes"]);
        check("the pick has allowed attributes", picked["attributes"]["aria-label"] == "Go now", &picked["attributes"]);
        check("the pick has computed styles", picked["computedStyles"].as_object().is_some_and(|styles| !styles.is_empty()), &picked["computedStyles"]);
        check("the pick has a box", picked["boundingBox"]["width"].as_f64().is_some_and(|width| width > 0.0), &picked["boundingBox"]);
        check("the pick has its text and HTML", picked["outerHtml"].as_str().is_some_and(|html| html.contains("data-testid")), &picked["outerHtml"]);
        let crop = picked["screenshotBase64"].as_str().unwrap_or_default();
        check("the pick has a crop", !crop.is_empty(), crop.len());
        std::fs::write(output.join("host-pick.png"), base64::engine::general_purpose::STANDARD.decode(crop).unwrap_or_default()).ok();
        check("a pick doesn't click the page", eval("window.clicks").ok() == before.ok(), eval("window.clicks"));
        check("the picker disarms itself", status()["elementPicker"]["armed"] != true, status()["elementPicker"].clone());

        // ----- the file preview
        let site = output.join("preview-site");
        std::fs::create_dir_all(&site).expect("site");
        std::fs::write(site.join("page.html"), "<!doctype html><title>Local file</title><link rel=stylesheet href=s.css><p id=p>hello</p>").expect("html");
        std::fs::write(site.join("s.css"), "p{color:rgb(9,8,7)}").expect("css");
        let previewed = call("browser_open_local_file", json!({"sessionId": session, "path": site.join("page.html").display().to_string()}));
        check("a local file opens", previewed.is_ok(), &previewed);
        check("it lands on its preview address", loaded("/page.html"), status());
        action("handoff_agent", Value::Null).ok();
        check("its stylesheet loads", eval("getComputedStyle(document.getElementById('p')).color").as_deref().is_ok_and(|text| text.contains("9, 8, 7")), eval("getComputedStyle(document.getElementById('p')).color"));

        // ----- history
        call("browser_navigate", json!({"sessionId": session, "url": format!("{origin}/second.html")})).ok();
        check("a second page loads", loaded("/second.html"), status());
        check("back is available", status()["canGoBack"] == true, status());
        let back = action("back", Value::Null);
        check("back is accepted", back.is_ok(), &back);
        check("back lands on the previous page", loaded("/page.html"), status());

        // ----- cookies across a cold suspend
        call("browser_navigate", json!({"sessionId": session, "url": format!("{origin}/index.html")})).ok();
        loaded("/index.html");
        let suspended = action("suspend", Value::Null);
        println!("        suspended: {:?}", suspended.as_ref().map(|status| status["suspended"].clone()));
        check("a page suspends", suspended.as_ref().is_ok_and(|status| status["suspended"] == true), &suspended);
        check("its native page is gone", tauri::webview::live_page_labels().is_empty(), tauri::webview::live_page_labels());
        let resumed = call("browser_open", json!({"sessionId": session, "lifecycleEpoch": epoch + 1}));
        check("it resumes", resumed.as_ref().is_ok_and(|status| status["suspended"] == false && status["hasPage"] == true), &resumed);
        check("at its address", loaded("/index.html"), status());
        call("browser_set_panel_bounds", json!({"sessionId": session, "bounds": bounds, "lifecycleEpoch": epoch + 1})).ok();
        call("browser_navigate", json!({"sessionId": session, "url": format!("{origin}/echo")})).ok();
        loaded("/echo");
        action("handoff_agent", Value::Null).ok();
        let cookies = tool("preview_eval", json!({"expression": "document.getElementById('c').textContent"}));
        println!("        cookies after resume: {cookies:?}");
        check("the sign-in survives the suspend, HttpOnly included", cookies.as_deref().is_ok_and(|text| text.contains("login=secret") && text.contains("fromscript=1")), &cookies);

        // ----- clearing a tab's data
        let cleared = action("clear_data", Value::Null);
        check("clear_data is accepted", cleared.is_ok(), &cleared);
        action("reload", Value::Null).ok();
        std::thread::sleep(Duration::from_millis(500));
        action("handoff_agent", Value::Null).ok();
        let cookies = tool("preview_eval", json!({"expression": "document.getElementById('c').textContent"}));
        check("clearing takes the sign-in", cookies.as_deref().is_ok_and(|text| !text.contains("fromscript=1")), &cookies);

        // ----- zoom, theme, language, hide, close
        check("zoom is reported", action("zoom", json!(1.5)).is_ok_and(|status| status["zoom"] == 1.5), status()["zoom"].clone());
        action("zoom", json!(1.0)).ok();
        check("theme is accepted", action("theme", json!("night")).is_ok(), "theme");
        check("language is accepted", action("locale", json!("en-US")).is_ok() || action("locale", json!("en")).is_ok(), "locale");
        let hidden = call("browser_action", json!({"sessionId": session, "action": "hide", "lifecycleEpoch": epoch + 2}));
        check("hide keeps the page", hidden.as_ref().is_ok_and(|status| status["open"] == false), &hidden);
        let closed = call("browser_close", json!({"sessionId": session, "lifecycleEpoch": epoch + 3}));
        check("close closes", closed.as_ref().is_ok_and(|disposition| disposition["status"] == "closed"), &closed);
        check("no native page is left", tauri::webview::live_page_labels().is_empty(), tauri::webview::live_page_labels());
        let _ = std::fs::remove_dir_all(&data);
    }
}
