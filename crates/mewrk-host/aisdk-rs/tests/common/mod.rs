//! A mock upstream on a real socket, the in-process sidecar, and the original Node sidecar for differential runs.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mewrk_aisdk::Sidecar;
use mewrk_aisdk::json::Json;

/// What the mock received for one request.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Json {
        Json::parse(&self.body).expect("the request body is JSON")
    }
}

/// One scripted answer.
#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Written in order, with `Reply::pause` between them.
    pub chunks: Vec<Vec<u8>>,
    pub pause: Duration,
    /// Closes the socket after the chunks without ending the chunked body.
    pub truncate: bool,
    /// Keeps the connection open and silent after the chunks until the client closes it.
    pub hang: bool,
    /// Sends no framing headers at all and ends with the connection.
    pub raw: bool,
}

impl Reply {
    pub fn new(status: u16) -> Self {
        Self { status, headers: Vec::new(), chunks: Vec::new(), pause: Duration::ZERO, truncate: false, hang: false, raw: false }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.chunks.push(body.into());
        self
    }

    /// `content-type: application/json`.
    pub fn json(status: u16, body: &str) -> Self {
        Self::new(status).header("content-type", "application/json").body(body)
    }

    /// SSE of the given JSON values, one event each, with no `[DONE]`.
    pub fn sse(events: &[&str]) -> Self {
        let mut reply = Self::new(200).header("content-type", "text/event-stream");
        for event in events {
            reply.chunks.push(format!("data: {event}\n\n").into_bytes());
        }
        reply
    }

    /// An SSE body given verbatim.
    pub fn sse_raw(body: &str) -> Self {
        Self::new(200).header("content-type", "text/event-stream").body(body)
    }

    pub fn pause(mut self, pause: Duration) -> Self {
        self.pause = pause;
        self
    }

    pub fn truncated(mut self) -> Self {
        self.truncate = true;
        self
    }

    pub fn hanging(mut self) -> Self {
        self.hang = true;
        self
    }
}

pub type Handler = Arc<dyn Fn(&Seen, usize) -> Reply + Send + Sync>;

pub struct Mock {
    pub base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Set when a hanging reply's client went away.
    pub client_gone: Arc<Mutex<bool>>,
}

impl Mock {
    /// Answers each request from `replies` in order; the last repeats.
    pub fn script(replies: Vec<Reply>) -> Self {
        let queue = Mutex::new(VecDeque::from(replies));
        let last: Mutex<Option<Reply>> = Mutex::new(None);
        Self::start(Arc::new(move |_, _| {
            let mut queue = queue.lock().unwrap();
            let next = queue.pop_front();
            let mut last = last.lock().unwrap();
            if let Some(next) = next {
                *last = Some(next.clone());
                return next;
            }
            last.clone().expect("a mock with no replies got a request")
        }))
    }

    pub fn start(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let client_gone = Arc::new(Mutex::new(false));
        let (seen_for_thread, gone) = (seen.clone(), client_gone.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let (handler, seen, gone) = (handler.clone(), seen_for_thread.clone(), gone.clone());
                std::thread::spawn(move || serve(stream, handler, seen, gone));
            }
        });
        Self { base: format!("http://127.0.0.1:{port}"), seen, client_gone }
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn last_request(&self) -> Seen {
        self.requests().pop().expect("the mock saw a request")
    }
}

fn serve(mut stream: TcpStream, handler: Handler, seen: Arc<Mutex<Vec<Seen>>>, gone: Arc<Mutex<bool>>) {
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.trim_end().splitn(3, ' ');
        let method = parts.next().unwrap_or("").to_owned();
        let target = parts.next().unwrap_or("").to_owned();
        let mut headers = Vec::new();
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                let value = value.trim().to_owned();
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.parse().unwrap_or(0);
                }
                headers.push((name.to_owned(), value));
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let request = Seen { method, target, headers, body: String::from_utf8_lossy(&body).into_owned() };
        let index = {
            let mut seen = seen.lock().unwrap();
            seen.push(request.clone());
            seen.len() - 1
        };
        let reply = handler(&request, index);
        if write_reply(&mut stream, &reply, &gone).is_err() || reply.truncate || reply.hang || reply.raw {
            return;
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        529 => "Overloaded",
        _ => "Status",
    }
}

fn write_reply(stream: &mut TcpStream, reply: &Reply, gone: &Arc<Mutex<bool>>) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if reply.raw {
        head.push_str("connection: close\r\n\r\n");
        stream.write_all(head.as_bytes())?;
        for chunk in &reply.chunks {
            stream.write_all(chunk)?;
        }
        return stream.flush();
    }
    head.push_str("transfer-encoding: chunked\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    for (i, chunk) in reply.chunks.iter().enumerate() {
        if i > 0 && !reply.pause.is_zero() {
            std::thread::sleep(reply.pause);
        }
        if chunk.is_empty() {
            continue;
        }
        stream.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())?;
        stream.write_all(chunk)?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
    }
    if reply.truncate {
        return stream.flush();
    }
    if reply.hang {
        // Reads until the client closes: the proof a cancelled request closed its socket.
        let mut probe = [0u8; 16];
        loop {
            match stream.read(&mut probe) {
                Ok(0) | Err(_) => {
                    *gone.lock().unwrap() = true;
                    return Ok(());
                }
                Ok(_) => {}
            }
        }
    }
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

/// Everything a step produced, frame by frame.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub frames: Vec<Json>,
}

impl Outcome {
    pub fn events(&self) -> Vec<&Json> {
        self.frames.iter().filter(|f| f.str_at("type") == Some("event")).filter_map(|f| f.get("event")).collect()
    }

    pub fn events_of(&self, kind: &str) -> Vec<&Json> {
        self.events().into_iter().filter(|e| e.str_at("k") == Some(kind)).collect()
    }

    pub fn terminal(&self) -> &Json {
        self.frames.iter().rev().find(|f| matches!(f.str_at("type"), Some("done") | Some("error"))).expect("a terminal frame")
    }

    pub fn result(&self) -> &Json {
        let terminal = self.terminal();
        assert_eq!(terminal.str_at("type"), Some("done"), "expected done, got {terminal}");
        terminal.get("result").expect("a result")
    }

    pub fn error(&self) -> &Json {
        let terminal = self.terminal();
        assert_eq!(terminal.str_at("type"), Some("error"), "expected error, got {terminal}");
        terminal.get("error").expect("an error")
    }

    /// The text deltas joined.
    pub fn text(&self) -> String {
        self.events_of("text-delta").iter().filter_map(|e| e.str_at("delta")).collect()
    }
}

/// The in-process sidecar with its frames on a channel.
pub struct Rust {
    sidecar: Sidecar,
    lines: Receiver<String>,
}

impl Rust {
    pub fn start() -> Self {
        let (tx, lines) = channel();
        let tx = Mutex::new(tx);
        let sidecar = Sidecar::start(
            Box::new(move |line| {
                let _ = tx.lock().unwrap().send(line);
            }),
            Box::new(|message| eprintln!("sidecar stopped: {message}")),
        )
        .expect("start");
        let rust = Self { sidecar, lines };
        rust.sidecar.send_line(r#"{"v":18,"type":"hello"}"#);
        let ready = rust.lines.recv_timeout(Duration::from_secs(5)).expect("ready");
        assert_eq!(ready, r#"{"v":18,"seq":0,"type":"ready","protocol":18}"#);
        rust
    }

    pub fn send(&self, line: &str) {
        self.sidecar.send_line(line);
    }

    pub fn run(&self, id: &str, payload: &Json) -> Outcome {
        self.send(&step_frame(id, payload));
        self.wait(id, Duration::from_secs(20))
    }

    pub fn wait(&self, id: &str, limit: Duration) -> Outcome {
        collect(id, limit, |left| self.lines.recv_timeout(left))
    }
}

pub fn step_frame(id: &str, payload: &Json) -> String {
    format!(r#"{{"v":18,"type":"step","id":{},"payload":{}}}"#, Json::from(id).to_text(), payload.to_text())
}

fn collect(id: &str, limit: Duration, mut next: impl FnMut(Duration) -> Result<String, RecvTimeoutError>) -> Outcome {
    let deadline = Instant::now() + limit;
    let mut outcome = Outcome::default();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = next(left).unwrap_or_else(|e| panic!("no terminal frame for {id} ({e:?}): {:?}", outcome.frames));
        let frame = Json::parse(&line).expect("a frame is JSON");
        if frame.str_at("id") != Some(id) {
            continue;
        }
        let terminal = matches!(frame.str_at("type"), Some("done") | Some("error"));
        outcome.frames.push(frame);
        if terminal {
            return outcome;
        }
    }
}

// ------------------------------------------------------------------ The original sidecar

/// The built Node sidecar, for a differential run. Differential tests skip themselves without it:
/// `MEWRK_AISDK_NODE_SIDECAR=<aisdk-service>/dist/main.mjs`.
pub fn node_sidecar_path() -> Option<String> {
    std::env::var("MEWRK_AISDK_NODE_SIDECAR").ok().filter(|p| std::path::Path::new(p).is_file())
}

/// Runs one step through `node <path>`.
pub fn run_node(path: &str, payload: &Json) -> Outcome {
    let mut child = Command::new("node")
        .arg(path)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("node");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    writeln!(stdin, r#"{{"v":18,"type":"hello"}}"#).unwrap();
    writeln!(stdin, "{}", step_frame("s", payload)).unwrap();
    stdin.flush().unwrap();
    let outcome = collect("s", Duration::from_secs(30), |left| rx.recv_timeout(left));
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

/// A request as a differential comparison reads it. Node's runtime segment of the user agent is the one
/// deliberate difference.
pub fn comparable_request(seen: &Seen) -> String {
    let mut headers: Vec<(String, String)> = seen
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("host") && !name.eq_ignore_ascii_case("connection"))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.replace("runtime/node.js/22", "runtime/rust")))
        .collect();
    headers.sort();
    format!("{} {}\n{headers:#?}\n{}", seen.method, seen.target, seen.body)
}

/// Frames as a differential comparison reads them: `seq`, heartbeats and wall-clock durations go, generated
/// ids are normalized by `normalize`.
pub fn comparable_frames(outcome: &Outcome) -> Vec<String> {
    outcome
        .frames
        .iter()
        .filter(|f| f.path(&["event", "k"]).and_then(Json::as_str) != Some("heartbeat"))
        .map(|frame| {
            let mut frame = frame.clone();
            if let Some(object) = frame.as_obj_mut() {
                object.remove("seq");
            }
            scrub(&mut frame);
            frame.to_text()
        })
        .collect()
}

fn scrub(value: &mut Json) {
    match value {
        Json::Obj(object) => {
            for key in ["durationMs", "reasoningMs"] {
                if object.contains_key(key) {
                    object.insert(key, 0u64);
                }
            }
            // A source's id is generated unless it derives from a search URL.
            if let Some(Json::Arr(sources)) = object.get_mut("sources") {
                for source in sources {
                    let generated = source.str_at("id").is_some_and(|id| !id.starts_with("search:"));
                    if let (true, Some(source)) = (generated, source.as_obj_mut()) {
                        source.insert("id", "<generated>");
                    }
                }
            }
            for (_, entry) in object.iter_mut() {
                scrub(entry);
            }
        }
        Json::Arr(items) => items.iter_mut().for_each(scrub),
        _ => {}
    }
}
