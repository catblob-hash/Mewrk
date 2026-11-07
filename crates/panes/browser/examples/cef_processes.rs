//! Runs the Chromium page engine in a real window, the way the host drives it, and reports which processes it runs, what
//! they cost together, and how long the calls that cross them take.
//!
//!     cargo run --profile dev-opt -p mewrk-pane-browser --example cef_processes -- [output directory] [rounds]
//!
//! A development build is its own helper, so this executable is relaunched for every Chromium process; the first thing
//! `main` does is hand those to the engine. Each line it prints is one measurement. `processes.json`, `page.png` and a
//! `window-N.png` for DevTools land in the output directory. Helpers here run unsandboxed, as in any development build;
//! the signed bundle is where the sandbox shows.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this example needs macOS");
}

#[cfg(target_os = "macos")]
fn main() {
    if let Some(code) = mewrk_client::chromium_subprocess_main() {
        std::process::exit(code);
    }
    mewrk_client::chromium_preload_framework();
    driver::main();
}

#[cfg(target_os = "macos")]
mod driver {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use mewrk_client::{Host, HostConfig};
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use serde_json::{json, Value};

    /// Twenty subresources, so the first load is more than one request.
    fn index_page() -> String {
        let images = (0..20).map(|i| format!("<img width=16 height=16 alt='' src='/square.svg?{i}'>")).collect::<String>();
        format!(
            r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Processes</title>
<link rel="stylesheet" href="/style.css"></head><body>
<div id="hero">Chromium processes</div>{images}
<canvas id="c" width="320" height="120"></canvas>
<script>
const c = document.getElementById('c').getContext('2d');
(function frame(t) {{ c.fillStyle = `hsl(${{(t / 10) % 360}} 70% 50%)`; c.fillRect(0, 0, 320, 120); requestAnimationFrame(frame); }})(0);
window.fetchRounds = async (n) => {{
  const times = [];
  for (let i = 0; i < n; i++) {{ const t = performance.now(); await (await fetch('/api?' + i)).text(); times.push(performance.now() - t); }}
  times.sort((a, b) => a - b);
  return times[Math.floor(times.length / 2)];
}};
</script></body></html>"##
        )
    }

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
                    let route = path.split('?').next().unwrap_or("/");
                    let (kind, body): (&str, String) = match route {
                        "/" | "/index.html" => ("text/html", index_page()),
                        "/a.html" | "/b.html" => ("text/html", format!("<!doctype html><title>{route}</title><h1>{route}</h1>")),
                        "/style.css" => ("text/css", "body{margin:0;font:16px -apple-system,sans-serif}#hero{background:#36c;color:#fff;padding:20px}".to_owned()),
                        "/square.svg" => ("image/svg+xml", "<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16'><rect width='16' height='16' fill='#d33'/></svg>".to_owned()),
                        "/api" => ("text/plain", "hello".to_owned()),
                        _ => ("text/plain", "nope".to_owned()),
                    };
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                });
            }
        });
        port
    }

    static WINDOW_NUMBER: AtomicUsize = AtomicUsize::new(0);

    pub fn main() {
        let output = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| std::env::temp_dir().join("cef-processes").display().to_string()));
        let rounds = std::env::args().nth(2).and_then(|rounds| rounds.parse().ok()).unwrap_or(30usize);
        std::fs::create_dir_all(&output).expect("output directory");
        mewrk_client::prepare_process();
        let mtm = MainThreadMarker::new().expect("main thread");
        let app = NSApplication::sharedApplication(mtm);
        // An accessory window shows without taking the foreground from whoever is at the keyboard.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let frame = NSRect::new(NSPoint::new(160.0, 160.0), NSSize::new(1000.0, 760.0));
        // SAFETY: on the main thread, with the initializer's documented arguments.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), frame, NSWindowStyleMask::Titled, NSBackingStoreType::Buffered, false)
        };
        // SAFETY: the window outlives the process's only run loop.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("cef_processes"));
        window.orderFrontRegardless();
        WINDOW_NUMBER.store(window.windowNumber() as usize, Ordering::SeqCst);
        let view = window.contentView().expect("content view");
        tauri::webview::attach_main_view(Retained::as_ptr(&view) as *mut std::ffi::c_void);
        std::thread::spawn(move || {
            let code = match std::panic::catch_unwind(|| run(&output, rounds)) {
                Ok(Ok(())) => 0,
                Ok(Err(error)) => {
                    println!("FAILED  {error}");
                    1
                }
                Err(_) => {
                    println!("FAILED  the driver panicked");
                    1
                }
            };
            std::process::exit(code);
        });
        app.run();
        let _ = (window, view);
    }

    fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) -> Result<(), String> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if done() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(format!("timed out waiting for {what}"))
    }

    /// Median and 90th percentile, in milliseconds.
    fn spread(mut samples: Vec<f64>) -> (f64, f64) {
        samples.sort_by(f64::total_cmp);
        let at = |fraction: f64| samples[((samples.len() - 1) as f64 * fraction).round() as usize];
        (at(0.5), at(0.9))
    }

    fn ms(since: Instant) -> f64 {
        since.elapsed().as_secs_f64() * 1000.0
    }

    /// This process's helpers: every Chromium process is a direct child of the browser process.
    fn helpers() -> Vec<(u32, String)> {
        let me = std::process::id();
        let listing = std::process::Command::new("/bin/ps").args(["-axo", "pid=,ppid=,command="]).output().map(|output| String::from_utf8_lossy(&output.stdout).into_owned()).unwrap_or_default();
        listing
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse::<u32>().ok()?;
                let parent = fields.next()?.parse::<u32>().ok()?;
                let command = fields.collect::<Vec<_>>();
                let switch = |name: &str| command.iter().find_map(|field| field.strip_prefix(name)).map(str::to_owned);
                let kind = switch("--type=")?;
                let kind = match switch("--utility-sub-type=") {
                    Some(sub) => format!("{kind}:{sub}"),
                    None => kind,
                };
                (parent == me).then_some((pid, kind))
            })
            .collect()
    }

    /// `struct rusage_info_v2` from `<sys/resource.h>`.
    #[repr(C)]
    #[derive(Default)]
    struct RusageInfo {
        uuid: [u8; 16],
        user_time: u64,
        system_time: u64,
        pkg_idle_wkups: u64,
        interrupt_wkups: u64,
        rest: [u64; 15],
    }

    #[repr(C)]
    #[derive(Default)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfo) -> i32;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }

    /// CPU time in milliseconds and wake-ups so far, summed over these processes.
    fn usage(pids: &[u32]) -> (f64, u64) {
        let mut timebase = Timebase::default();
        // SAFETY: both fill a caller-owned struct of the layout the headers give.
        unsafe { mach_timebase_info(&mut timebase) };
        pids.iter().fold((0.0, 0), |(cpu, wakeups), pid| {
            let mut info = RusageInfo::default();
            if unsafe { proc_pid_rusage(*pid as i32, 2, &mut info) } != 0 {
                return (cpu, wakeups);
            }
            let ticks = (info.user_time + info.system_time) as f64 * timebase.numer as f64 / timebase.denom as f64;
            (cpu + ticks / 1e6, wakeups + info.pkg_idle_wkups + info.interrupt_wkups)
        })
    }

    /// What every Chromium process together spends over a few seconds of whatever the page is doing.
    fn spend(what: &str) {
        let pids = std::iter::once(std::process::id()).chain(helpers().into_iter().map(|(pid, _)| pid)).collect::<Vec<_>>();
        let window = Duration::from_secs(5);
        let (cpu, wakeups) = usage(&pids);
        std::thread::sleep(window);
        let (cpu_after, wakeups_after) = usage(&pids);
        let seconds = window.as_secs_f64();
        println!("{what:<26}{:8.1} ms CPU/s {:8.0} wake-ups/s", (cpu_after - cpu) / seconds, (wakeups_after - wakeups) as f64 / seconds);
    }

    /// Footprint of this process and its helpers together, shared memory counted once.
    fn footprint(pids: &[u32], into: &Path) -> Option<u64> {
        let mut command = std::process::Command::new("/usr/bin/footprint");
        command.args(["-f", "bytes", "-j"]).arg(into);
        for pid in pids {
            command.args(["-p", &pid.to_string()]);
        }
        command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().ok()?;
        let report: Value = serde_json::from_slice(&std::fs::read(into).ok()?).ok()?;
        report["total footprint"].as_u64()
    }

    fn run(output: &Path, rounds: usize) -> Result<(), String> {
        let port = serve();
        let origin = format!("http://127.0.0.1:{port}");
        let data = std::env::temp_dir().join(format!("cef-processes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).map_err(|error| error.to_string())?;
        let host = Host::start(
            HostConfig {
                identifier: "dev.mewrk.cefprocesses".to_owned(),
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
        .map_err(|error| format!("the host did not start: {error:?}"))?;
        // What the pane's `attach_window` does: without it the host hides every page after 12 s.
        {
            let host = host.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(4));
                let _ = host.invoke_blocking("browser_renderer_mount_heartbeat", json!({}));
            });
        }
        let session = "cef-processes-session";
        let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 * 1024;
        let call = |command: &str, args: Value| host.invoke_blocking(command, args);
        let status = || call("browser_status", json!({"sessionId": session})).unwrap_or(Value::Null);
        let tool = |name: &str, input: Value| host.run_preview_tool(session, name, input);
        let loaded = |url_end: &str| {
            let end = url_end.to_owned();
            wait_until(url_end, Duration::from_secs(20), || {
                let status = status();
                status["loading"] == false && status["url"].as_str().is_some_and(|url| url.ends_with(&end))
            })
        };

        // The engine starts with the first real address, so this is a cold start: framework, helpers, first document.
        let started = Instant::now();
        call("browser_open", json!({"sessionId": session, "url": format!("{origin}/index.html"), "lifecycleEpoch": epoch})).map_err(|error| format!("browser_open: {error}"))?;
        let bounds = json!({"x": 100.0, "y": 40.0, "width": 800.0, "height": 600.0, "visible": true});
        call("browser_set_panel_bounds", json!({"sessionId": session, "bounds": bounds, "lifecycleEpoch": epoch})).map_err(|error| format!("browser_set_panel_bounds: {error}"))?;
        loaded("/index.html")?;
        println!("first page loaded         {:8.1} ms", ms(started));
        call("browser_action", json!({"sessionId": session, "action": "handoff_agent", "value": null})).ok();
        std::thread::sleep(Duration::from_millis(1500));

        let mut samples = Vec::new();
        for _ in 0..rounds {
            let at = Instant::now();
            let captured = call("browser_capture_page", json!({"sessionId": session})).map_err(|error| format!("browser_capture_page: {error}"))?;
            samples.push(ms(at));
            if captured["width"].as_u64().unwrap_or(0) < 800 {
                return Err(format!("the capture is not the page: {:?}", captured.get("width")));
            }
        }
        let (median, p90) = spread(samples);
        println!("capture_page              {median:8.1} ms median {p90:8.1} ms p90");

        let mut samples = Vec::new();
        for _ in 0..rounds * 3 {
            let at = Instant::now();
            tool("preview_eval", json!({"expression": "1 + 1"}))?;
            samples.push(ms(at));
        }
        let (median, p90) = spread(samples);
        println!("preview_eval              {median:8.1} ms median {p90:8.1} ms p90");

        // Frames still arriving, and from the GPU rather than a software fallback.
        let frames = tool("preview_eval", json!({"expression": "new Promise(r => { let n = 0; const t = performance.now(); (function f() { if (performance.now() - t < 1000) { n++; requestAnimationFrame(f); } else r(n); })(); })"}))?;
        println!("frames in one second      {}", frames.trim());
        let renderer = tool("preview_eval", json!({"expression": "(() => { const gl = document.createElement('canvas').getContext('webgl'); const info = gl && gl.getExtension('WEBGL_debug_renderer_info'); return info ? gl.getParameter(info.UNMASKED_RENDERER_WEBGL) : 'no webgl'; })()"}))?;
        println!("webgl renderer            {}", renderer.trim());

        let fetched = tool("preview_eval", json!({"expression": format!("window.fetchRounds({})", rounds * 5)}))?;
        let fetched = fetched.trim().trim_matches('"').parse::<f64>().map_err(|_| format!("the page's fetch rounds: {fetched}"))?;
        println!("page fetch                {fetched:8.1} ms median");

        let pids = std::iter::once(std::process::id()).chain(helpers().into_iter().map(|(pid, _)| pid)).collect::<Vec<_>>();
        let steady = footprint(&pids, &output.join("footprint.json"));
        spend("animating page");

        let mut samples = Vec::new();
        for round in 0..rounds {
            let page = if round % 2 == 0 { "/a.html" } else { "/b.html" };
            let at = Instant::now();
            call("browser_navigate", json!({"sessionId": session, "url": format!("{origin}{page}?{round}")}))?;
            loaded(&format!("{page}?{round}"))?;
            samples.push(ms(at));
        }
        let (median, p90) = spread(samples);
        println!("navigate                  {median:8.1} ms median {p90:8.1} ms p90");
        std::thread::sleep(Duration::from_secs(1));
        spend("still page");

        // An audio context opens an output stream: where the audio service runs shows up here.
        call("browser_navigate", json!({"sessionId": session, "url": format!("{origin}/index.html")}))?;
        loaded("/index.html")?;
        call("browser_action", json!({"sessionId": session, "action": "handoff_agent", "value": null})).ok();
        let audio = tool("preview_eval", json!({"expression": "(async () => { window.audio = new AudioContext(); const o = window.audio.createOscillator(); o.connect(window.audio.destination); o.start(); await new Promise(r => setTimeout(r, 800)); return window.audio.state + ' ' + window.audio.sampleRate; })()"}));
        println!("audio context             {}", audio.as_deref().unwrap_or("failed").trim());
        // DevTools is a window of its own on the same GPU service, and its front end a renderer of its own.
        call("browser_action", json!({"sessionId": session, "action": "devtools", "value": null})).map_err(|error| format!("devtools: {error}"))?;
        std::thread::sleep(Duration::from_millis(2500));

        let processes = helpers();
        let pids = std::iter::once(std::process::id()).chain(processes.iter().map(|(pid, _)| *pid)).collect::<Vec<_>>();
        let with_audio = footprint(&pids, &output.join("footprint-audio-devtools.json"));
        let mut kinds = processes.iter().map(|(_, kind)| kind.clone()).collect::<Vec<_>>();
        kinds.sort();
        println!("helper processes          {} {kinds:?}", processes.len());
        let mib = |bytes: Option<u64>| bytes.map_or_else(|| "unknown".to_owned(), |bytes| format!("{:.1} MiB", bytes as f64 / 1048576.0));
        println!("footprint, all processes  {} steady, {} with audio and DevTools", mib(steady), mib(with_audio));
        std::fs::write(output.join("processes.json"), serde_json::to_vec_pretty(&json!({"helpers": kinds, "steady": steady, "withAudioAndDevtools": with_audio})).unwrap_or_default()).ok();

        let number = WINDOW_NUMBER.load(Ordering::SeqCst);
        let _ = std::process::Command::new("/usr/sbin/screencapture").args(["-x", "-o", &format!("-l{number}")]).arg(output.join("page.png")).status();
        // AppKit's window list is read on the main thread; only the numbers leave it.
        let (sender, receiver) = std::sync::mpsc::channel();
        tauri::webview::post_main(move || {
            let mtm = MainThreadMarker::new().expect("main thread");
            let others = NSApplication::sharedApplication(mtm).windows().iter().filter(|window| window.isVisible()).map(|window| window.windowNumber()).filter(|other| *other as usize != number).collect::<Vec<_>>();
            let _ = sender.send(others);
        });
        for (index, other) in receiver.recv_timeout(Duration::from_secs(5)).unwrap_or_default().into_iter().enumerate() {
            let _ = std::process::Command::new("/usr/sbin/screencapture").args(["-x", "-o", &format!("-l{other}")]).arg(output.join(format!("window-{index}.png"))).status();
        }

        let closed = call("browser_close", json!({"sessionId": session, "lifecycleEpoch": epoch + 1}));
        if !closed.as_ref().is_ok_and(|disposition| disposition["status"] == "closed") {
            return Err(format!("browser_close: {closed:?}"));
        }
        let _ = std::fs::remove_dir_all(&data);
        Ok(())
    }
}
