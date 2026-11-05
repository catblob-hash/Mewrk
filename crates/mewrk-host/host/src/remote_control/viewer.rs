//! The loopback web server behind one session's viewer page.
//!
//! The preview pane shows a remote screen by opening a page, and this is that
//! page's server: the page itself, the live picture as an MJPEG stream, the
//! session's state, and the three things a person can send back (input, a
//! sign-in, whether the page is visible). It speaks only HTTP and knows nothing
//! of protocols; everything about the screen is behind [`ViewerBackend`].
//!
//! The trust model is the one of `docs/remote-control.md` §3.3. Each session
//! listens on a port of its own, so each viewer is its own web origin, and the
//! 128-bit token in the path is the whole authorization — the picture and the
//! keyboard of a real device are behind it, so it is compared in constant time
//! and never leaves the host. Two more rules keep a web page from reaching in:
//! the `Host` header must be this very listener (a DNS-rebinding page arrives
//! with its own name and is refused), and a page can only be framed by a
//! loopback origin. `Access-Control-Allow-Origin: *` is deliberate: in the
//! browser-dev preview the page runs in a sandboxed iframe whose origin is
//! `null`, so no origin could be named, and the token is what authorizes.
//!
//! The server is small enough to hand-roll on std threads (as
//! [`crate::preview_tunnel`] does): one thread per connection with a cap, every
//! read bounded in size and in time so a stalled or hostile client cannot hold a
//! thread or grow a buffer without limit, and connections kept alive so the
//! page's 30-a-second input posts do not each pay a handshake.
//!
//! # The stream
//!
//! `multipart/x-mixed-replace` is the only way to make an `<img>` play video
//! with no script. Blink swaps the picture when it sees the **next** part's
//! headers, not when the current part's body ends — so a part written as
//! `boundary, headers, body` shows one frame late, and a still screen would
//! never show its last change. Each JPEG is therefore followed at once by the
//! boundary and headers of the part that has not been produced yet; the next
//! frame's bytes complete it. A part cannot carry a `Content-Length` for that
//! reason (it is not known yet), which parts do not need: the boundary ends
//! them. A screen that does not change is resent every second, which also
//! finds a dead socket without waiting for the next change.

use std::{
    collections::HashMap,
    fmt::Write as _,
    io::{self, BufRead, Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    thread,
    time::{Duration, Instant},
};

use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use url::{Host, Url};
use zeroize::{Zeroize, Zeroizing};

use super::view::{ViewerBackend, ViewerCredentials, ViewerEvent, ViewerInput};

/// Connections served at once. A page needs three or four (the stream, the
/// state poll, the input post); more than this is not a page.
const MAX_CONNECTIONS: usize = 32;
/// The request line and headers together.
const MAX_HEAD: usize = 16 * 1024;
const MAX_INPUT_BODY: usize = 256 * 1024;
const MAX_CREDENTIALS_BODY: usize = 8 * 1024;
const MAX_VISIBILITY_BODY: usize = 1024;
const MAX_HEADERS: usize = 100;
/// Events in one input batch. A page sends a handful; this only has to stop a
/// 256 KiB body from becoming a hundred thousand events for the session.
const MAX_EVENTS: usize = 1000;
/// Requests on one kept-alive connection before it is closed.
const MAX_REQUESTS_PER_CONNECTION: usize = 1000;
/// Pages whose last applied input batch is remembered. Oldest forgotten first.
const MAX_TRACKED_VIEWERS: usize = 256;
const MAX_VIEWER_ID: usize = 64;
/// How often a blocked read wakes to see whether the viewer was stopped. A
/// socket `shutdown` does not wake a pending `recv` on Windows.
const READ_POLL: Duration = Duration::from_millis(250);
const STREAM_MIN_INTERVAL: Duration = Duration::from_millis(100);
const STREAM_KEEPALIVE: Duration = Duration::from_secs(1);
const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// A backend that answers a frame wait at once, forever (a session that ended),
/// must not turn a stream into a busy loop.
const STREAM_MIN_TURN: Duration = Duration::from_millis(20);

/// Sent with every response.
const COMMON_HEADERS: &str = "Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nAccess-Control-Allow-Origin: *\r\n";
/// The page loads nothing and talks only to its own server. `form-action` is
/// shut so a sign-in form whose script failed to run cannot submit the
/// password as a query string.
const PAGE_CSP: &str = "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; form-action 'none'; base-uri 'none'; frame-ancestors http://127.0.0.1:* http://localhost:*";

/// How long the server waits for each part of a request.
#[derive(Clone, Copy)]
struct Timing {
    /// A kept-alive connection with no request under way.
    idle: Duration,
    /// From the first byte of a request to the end of its headers.
    head: Duration,
    body: Duration,
    write: Duration,
}

impl Timing {
    const DEFAULT: Self = Self {
        idle: Duration::from_secs(15),
        head: Duration::from_secs(10),
        body: Duration::from_secs(10),
        write: Duration::from_secs(10),
    };
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut random = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| io::Error::other("the system random generator failed"))?;
    let mut hex = String::with_capacity(bytes * 2);
    for byte in random {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// What the accept thread and every connection thread share.
struct Shared {
    port: u16,
    token: String,
    backend: Weak<dyn ViewerBackend>,
    timing: Timing,
    stopped: AtomicBool,
    next_connection: AtomicU64,
    registry: Mutex<Registry>,
    /// Held while a page's stream count changes and the backend is told, so
    /// "page gone" cannot overtake the "page visible" of a stream that opened
    /// right after it.
    stream_changes: Mutex<()>,
}

#[derive(Default)]
struct Registry {
    /// Every open connection, so [`Viewer::stop`] can close them.
    connections: HashMap<u64, TcpStream>,
    /// Open streams per page. A page that hid and showed again opens a second
    /// stream before the first one is noticed closed; the page is gone only when
    /// the last ends.
    streams: HashMap<String, usize>,
    applied: HashMap<String, Applied>,
    clock: u64,
}

struct Applied {
    seq: u64,
    used: u64,
}

impl Registry {
    /// Whether an input batch is newer than the last one applied from its page,
    /// and if so, remembers it.
    fn admit_batch(&mut self, viewer: &str, seq: u64) -> bool {
        self.clock += 1;
        let clock = self.clock;
        if let Some(applied) = self.applied.get_mut(viewer) {
            applied.used = clock;
            if seq <= applied.seq {
                return false;
            }
            applied.seq = seq;
            return true;
        }
        if self.applied.len() >= MAX_TRACKED_VIEWERS {
            let oldest = self
                .applied
                .iter()
                .min_by_key(|(_, applied)| applied.used)
                .map(|(viewer, _)| viewer.clone());
            if let Some(oldest) = oldest {
                self.applied.remove(&oldest);
            }
        }
        self.applied
            .insert(viewer.to_owned(), Applied { seq, used: clock });
        true
    }
}

/// One session's viewer page: a loopback listener at
/// `http://127.0.0.1:<port>/<token>/`.
pub struct Viewer {
    port: u16,
    token: String,
    shared: Arc<Shared>,
    accept: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Viewer {
    /// Starts listening on an OS-assigned loopback port. The backend is weak:
    /// the session owns the viewer, not the other way round.
    pub fn start(backend: Weak<dyn ViewerBackend>) -> io::Result<Self> {
        Self::start_with(backend, Timing::DEFAULT)
    }

    fn start_with(backend: Weak<dyn ViewerBackend>, timing: Timing) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let token = random_hex(16)?;
        let shared = Arc::new(Shared {
            port,
            token: token.clone(),
            backend,
            timing,
            stopped: AtomicBool::new(false),
            next_connection: AtomicU64::new(0),
            registry: Mutex::new(Registry::default()),
            stream_changes: Mutex::new(()),
        });
        let accepting = shared.clone();
        let accept = thread::Builder::new()
            .name(format!("remote-viewer-{port}"))
            .spawn(move || accept_loop(listener, accepting))?;
        Ok(Self {
            port,
            token,
            shared,
            accept: Mutex::new(Some(accept)),
        })
    }

    /// The page's address. Never shown to the model.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}/", self.port, self.token)
    }

    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether a page at `page_url` is this viewer: same origin and the token's
    /// path.
    pub fn shows(&self, page_url: &str) -> bool {
        let Ok(url) = Url::parse(page_url) else {
            return false;
        };
        let loopback = match url.host() {
            Some(Host::Ipv4(address)) => address == Ipv4Addr::LOCALHOST,
            Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
            _ => false,
        };
        if url.scheme() != "http" || !loopback || url.port_or_known_default() != Some(self.port) {
            return false;
        }
        match url.path().strip_prefix('/') {
            Some(path) => {
                let segment = path.split('/').next().unwrap_or_default();
                token_matches(segment, &self.token)
            }
            None => false,
        }
    }

    /// Stops listening and ends every open stream.
    pub fn stop(&self) {
        if self.shared.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        // A blocked `accept` only notices the flag when something connects.
        let woken = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
            Duration::from_secs(1),
        )
        .is_ok();
        let open: Vec<TcpStream> = lock(&self.shared.registry)
            .connections
            .values()
            .filter_map(|stream| stream.try_clone().ok())
            .collect();
        for stream in open {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(accept) = lock(&self.accept).take() {
            if woken {
                let _ = accept.join();
            }
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Shared {
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

/// Whether `candidate` is the token. The length of a token is not a secret; its
/// bytes are, so the comparison does not stop at the first difference.
fn token_matches(candidate: &str, token: &str) -> bool {
    let (candidate, token) = (candidate.as_bytes(), token.as_bytes());
    if candidate.len() != token.len() {
        return false;
    }
    let difference = candidate
        .iter()
        .zip(token)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    std::hint::black_box(difference) == 0
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    for incoming in listener.incoming() {
        if shared.stopped() {
            break;
        }
        match incoming {
            Ok(stream) => admit(&shared, stream),
            // Out of descriptors or the like: wait rather than spin.
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn admit(shared: &Arc<Shared>, stream: TcpStream) {
    let id = shared.next_connection.fetch_add(1, Ordering::Relaxed);
    {
        let mut registry = lock(&shared.registry);
        if shared.stopped() {
            return;
        }
        if registry.connections.len() >= MAX_CONNECTIONS {
            drop(registry);
            refuse(stream);
            return;
        }
        let Ok(clone) = stream.try_clone() else {
            return;
        };
        registry.connections.insert(id, clone);
    }
    let serving = shared.clone();
    let spawned = thread::Builder::new()
        .name("remote-viewer-connection".to_owned())
        .spawn(move || {
            let _open = OpenConnection {
                shared: serving.clone(),
                id,
            };
            serve_connection(&serving, stream);
        });
    if spawned.is_err() {
        lock(&shared.registry).connections.remove(&id);
    }
}

/// Answers a connection over the cap and closes it.
fn refuse(mut stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let response = Response::text(503, "Too many connections to this viewer.").closing();
    let _ = write_response(&mut stream, &response, false);
    let _ = stream.shutdown(Shutdown::Both);
}

/// Takes a connection off the registry when its thread ends, however it ends.
struct OpenConnection {
    shared: Arc<Shared>,
    id: u64,
}

impl Drop for OpenConnection {
    fn drop(&mut self) {
        lock(&self.shared.registry).connections.remove(&self.id);
    }
}

// ---------------------------------------------------------------- requests

/// What a connection's reader buffers at once, as a `BufReader` would.
const READ_BUFFER: usize = 8 * 1024;

/// A connection's read side, buffered like a `BufReader` — but its buffer can
/// be wiped, and is wiped when the connection ends. A sign-in's body passes
/// through it (it usually arrives in the same packet as the headers), and a
/// `BufReader` would keep the password in its buffer until the bytes happen to
/// be overwritten, and then in freed memory.
struct ConnectionReader {
    stream: TcpStream,
    buffer: Zeroizing<Vec<u8>>,
    /// The bytes not read yet are `buffer[start..end]`.
    start: usize,
    end: usize,
}

impl ConnectionReader {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: Zeroizing::new(vec![0; READ_BUFFER]),
            start: 0,
            end: 0,
        }
    }

    fn get_ref(&self) -> &TcpStream {
        &self.stream
    }

    /// The bytes received and not read yet.
    fn buffer(&self) -> &[u8] {
        &self.buffer[self.start..self.end]
    }

    /// Zeroes every byte of the buffer except those not read yet, which belong
    /// to the next request.
    fn wipe_read(&mut self) {
        let (start, end) = (self.start, self.end);
        self.buffer[..start].zeroize();
        self.buffer[end..].zeroize();
    }
}

impl Read for ConnectionReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // A large read with nothing buffered goes straight into the caller's
        // memory, as a `BufReader`'s does.
        if self.start == self.end && out.len() >= self.buffer.len() {
            return self.stream.read(out);
        }
        let available = self.fill_buf()?;
        let count = available.len().min(out.len());
        out[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for ConnectionReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.start >= self.end {
            self.start = 0;
            self.end = 0;
            self.end = self.stream.read(&mut self.buffer[..])?;
        }
        Ok(&self.buffer[self.start..self.end])
    }

    fn consume(&mut self, amount: usize) {
        self.start = (self.start + amount).min(self.end);
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    host: Option<String>,
    content_length: Option<u64>,
    chunked: bool,
    expect_continue: bool,
    keep_alive: bool,
}

impl Request {
    fn has_body(&self) -> bool {
        self.chunked || self.content_length.is_some_and(|length| length > 0)
    }
}

enum ReadError {
    /// The client went away between requests, or the viewer stopped. Nothing to
    /// answer.
    Closed,
    Timeout,
    TooLarge,
    Malformed(&'static str),
}

impl ReadError {
    fn response(&self) -> Response {
        match self {
            Self::Closed => Response::text(400, "The connection closed.").closing(),
            Self::Timeout => Response::text(408, "The request took too long to arrive.").closing(),
            Self::TooLarge => {
                Response::text(431, "The request headers may be at most 16 KiB.").closing()
            }
            Self::Malformed(why) => Response::text(400, *why).closing(),
        }
    }
}

fn timed_out(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Waits until `reader` has bytes buffered, in short slices so a stop is
/// noticed. Returns at once when some are already there.
fn await_data(
    reader: &mut ConnectionReader,
    deadline: Instant,
    stopped: &AtomicBool,
) -> Result<(), ReadError> {
    loop {
        if stopped.load(Ordering::SeqCst) {
            return Err(ReadError::Closed);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ReadError::Timeout);
        }
        let slice = remaining.min(READ_POLL).max(Duration::from_millis(1));
        if reader.get_ref().set_read_timeout(Some(slice)).is_err() {
            return Err(ReadError::Closed);
        }
        match reader.fill_buf() {
            Ok([]) => return Err(ReadError::Closed),
            Ok(_) => return Ok(()),
            Err(error) if timed_out(&error) => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(ReadError::Closed),
        }
    }
}

fn read_request(
    reader: &mut ConnectionReader,
    timing: &Timing,
    stopped: &AtomicBool,
) -> Result<Request, ReadError> {
    // The first byte may take as long as the client likes (up to the idle
    // limit); once it is there, the whole head has to follow promptly.
    await_data(reader, Instant::now() + timing.idle, stopped).map_err(|error| match error {
        ReadError::Timeout => ReadError::Closed,
        other => other,
    })?;
    let deadline = Instant::now() + timing.head;
    let mut head = Vec::with_capacity(1024);
    loop {
        await_data(reader, deadline, stopped)?;
        let mut used = 0;
        let mut complete = false;
        for &byte in reader.buffer() {
            used += 1;
            head.push(byte);
            if head.len() > MAX_HEAD {
                return Err(ReadError::TooLarge);
            }
            if head.ends_with(b"\r\n\r\n") {
                complete = true;
                break;
            }
        }
        reader.consume(used);
        if complete {
            return parse_head(&head);
        }
    }
}

fn parse_head(head: &[u8]) -> Result<Request, ReadError> {
    let malformed = ReadError::Malformed;
    let text = std::str::from_utf8(head).map_err(|_| malformed("The request is not text."))?;
    let text = text.trim_start_matches("\r\n");
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(malformed("The request line is malformed."));
    };
    let http11 = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        _ => return Err(malformed("Only HTTP/1.0 and HTTP/1.1 are spoken here.")),
    };
    if method.is_empty() || !target.starts_with('/') {
        return Err(malformed("The request target must be a path."));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut request = Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query: query.to_owned(),
        host: None,
        content_length: None,
        chunked: false,
        expect_continue: false,
        keep_alive: http11,
    };
    let mut count = 0;
    for line in lines {
        if line.is_empty() {
            break;
        }
        count += 1;
        if count > MAX_HEADERS {
            return Err(ReadError::TooLarge);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(malformed("A header line is malformed."));
        };
        if name.is_empty() || name.bytes().any(|byte| byte <= b' ' || byte == 0x7f) {
            return Err(malformed("A header name is malformed."));
        }
        let value = value.trim_matches(|c| c == ' ' || c == '\t');
        match name.to_ascii_lowercase().as_str() {
            "host" => {
                if request.host.replace(value.to_owned()).is_some() {
                    return Err(malformed("The request has more than one Host header."));
                }
            }
            "content-length" => {
                let digits = !value.is_empty()
                    && value.len() <= 10
                    && value.bytes().all(|byte| byte.is_ascii_digit());
                let length = value
                    .parse::<u64>()
                    .ok()
                    .filter(|_| digits)
                    .ok_or_else(|| malformed("Content-Length is not a number."))?;
                if request
                    .content_length
                    .replace(length)
                    .is_some_and(|before| before != length)
                {
                    return Err(malformed(
                        "The request has conflicting Content-Length headers.",
                    ));
                }
            }
            "transfer-encoding" => request.chunked = true,
            "expect" => request.expect_continue = value.eq_ignore_ascii_case("100-continue"),
            "connection" => {
                for token in value.split(',') {
                    let token = token.trim();
                    if token.eq_ignore_ascii_case("close") {
                        request.keep_alive = false;
                    } else if token.eq_ignore_ascii_case("keep-alive") && !http11 {
                        request.keep_alive = true;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(request)
}

/// The body of a POST, read in full and bounded. An error is the response to
/// send; it closes the connection, because the body was not read. The body is
/// wiped when it drops: one of them is a sign-in.
fn read_body(
    shared: &Shared,
    request: &Request,
    reader: &mut ConnectionReader,
    writer: &mut TcpStream,
    limit: usize,
) -> Result<Zeroizing<Vec<u8>>, Response> {
    if request.chunked {
        return Err(Response::text(400, "Chunked request bodies are not supported.").closing());
    }
    let Some(length) = request.content_length else {
        return Err(Response::text(411, "A Content-Length header is required.").closing());
    };
    let Some(length) = usize::try_from(length)
        .ok()
        .filter(|length| *length <= limit)
    else {
        return Err(
            Response::text(413, format!("The body may be at most {limit} bytes.")).closing(),
        );
    };
    if request.expect_continue && writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
        return Err(Response::text(400, "The connection closed.").closing());
    }
    // Its full length at once: a buffer that grew would leave copies behind.
    let mut body = Zeroizing::new(vec![0u8; length]);
    let mut filled = 0;
    let deadline = Instant::now() + shared.timing.body;
    while filled < length {
        await_data(reader, deadline, &shared.stopped).map_err(|error| error.response())?;
        match reader.read(&mut body[filled..]) {
            Ok(0) => {
                return Err(Response::text(400, "The body ended before Content-Length.").closing())
            }
            Ok(taken) => filled += taken,
            Err(error) if timed_out(&error) || error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(Response::text(400, "The connection closed.").closing()),
        }
    }
    Ok(body)
}

// --------------------------------------------------------------- responses

struct Response {
    status: u16,
    content_type: Option<&'static str>,
    extra: Vec<(&'static str, String)>,
    page: bool,
    body: Vec<u8>,
    /// Close the connection after this response (a request body is left unread,
    /// or the client is not behaving).
    close: bool,
}

impl Response {
    fn new(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            extra: Vec::new(),
            page: false,
            body: Vec::new(),
            close: false,
        }
    }

    fn text(status: u16, message: impl Into<String>) -> Self {
        Self {
            content_type: Some("text/plain; charset=utf-8"),
            body: message.into().into_bytes(),
            ..Self::new(status)
        }
    }

    fn json(body: Vec<u8>) -> Self {
        Self {
            content_type: Some("application/json"),
            body,
            ..Self::new(200)
        }
    }

    fn html(body: String) -> Self {
        Self {
            content_type: Some("text/html; charset=utf-8"),
            page: true,
            body: body.into_bytes(),
            ..Self::new(200)
        }
    }

    fn closing(mut self) -> Self {
        self.close = true;
        self
    }

    /// The session behind the viewer is gone: nothing here will ever answer.
    fn gone() -> Self {
        Self::text(410, "This viewer's session has ended.").closing()
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        204 => "No Content",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        410 => "Gone",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

fn write_response(
    writer: &mut impl Write,
    response: &Response,
    keep_alive: bool,
) -> io::Result<()> {
    let mut head = String::with_capacity(512);
    let _ = write!(
        head,
        "HTTP/1.1 {} {}\r\n{COMMON_HEADERS}",
        response.status,
        reason(response.status)
    );
    if let Some(content_type) = response.content_type {
        let _ = write!(head, "Content-Type: {content_type}\r\n");
    }
    if response.page {
        let _ = write!(head, "Content-Security-Policy: {PAGE_CSP}\r\n");
    }
    for (name, value) in &response.extra {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    if response.status != 204 {
        let _ = write!(head, "Content-Length: {}\r\n", response.body.len());
    }
    let _ = write!(
        head,
        "Connection: {}\r\n\r\n",
        if keep_alive { "keep-alive" } else { "close" }
    );
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&response.body);
    writer.write_all(&bytes)?;
    writer.flush()
}

// ---------------------------------------------------------------- routing

enum Reply {
    Respond(Response),
    /// The connection was used as the picture stream and is finished.
    Streamed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Page,
    State,
    Stream,
    Input,
    Credentials,
    Visibility,
}

impl Endpoint {
    fn of(name: &str) -> Option<Self> {
        Some(match name {
            "" => Self::Page,
            "state" => Self::State,
            "stream" => Self::Stream,
            "input" => Self::Input,
            "credentials" => Self::Credentials,
            "visibility" => Self::Visibility,
            _ => return None,
        })
    }

    fn method(self) -> &'static str {
        match self {
            Self::Page | Self::State | Self::Stream => "GET",
            Self::Input | Self::Credentials | Self::Visibility => "POST",
        }
    }
}

fn host_allowed(port: u16, host: Option<&str>) -> bool {
    let Some((name, given)) = host.and_then(|host| host.rsplit_once(':')) else {
        return false;
    };
    given == port.to_string() && (name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost"))
}

fn not_found() -> Response {
    Response::text(404, "Not found.")
}

/// Everything before a body is read: who is asking, for what, and whether this
/// is a method the endpoint has.
fn resolve(shared: &Shared, request: &Request) -> Result<Endpoint, Response> {
    if !host_allowed(shared.port, request.host.as_deref()) {
        return Err(Response::text(
            400,
            "The Host header must be this viewer's own address.",
        ));
    }
    let Some(after_slash) = request.path.strip_prefix('/') else {
        return Err(not_found());
    };
    let (segment, rest) = match after_slash.split_once('/') {
        Some((segment, rest)) => (segment, Some(rest)),
        None => (after_slash, None),
    };
    if !token_matches(segment, &shared.token) {
        return Err(not_found());
    }
    let Some(rest) = rest else {
        // The page's relative addresses need the trailing slash.
        let mut redirect = Response::new(308);
        redirect
            .extra
            .push(("Location", format!("/{}/", shared.token)));
        redirect.body = Vec::new();
        return Err(if request.method == "GET" {
            redirect
        } else {
            not_found()
        });
    };
    let Some(endpoint) = Endpoint::of(rest) else {
        return Err(not_found());
    };
    if request.method == "OPTIONS" {
        let mut preflight = Response::new(204);
        preflight.extra = vec![
            (
                "Access-Control-Allow-Methods",
                "GET, POST, OPTIONS".to_owned(),
            ),
            ("Access-Control-Allow-Headers", "Content-Type".to_owned()),
            ("Access-Control-Max-Age", "600".to_owned()),
            ("Allow", "GET, POST, OPTIONS".to_owned()),
        ];
        return Err(preflight);
    }
    if request.method != endpoint.method() {
        let mut refused = Response::text(405, "Method not allowed.");
        refused.extra.push(("Allow", endpoint.method().to_owned()));
        return Err(refused);
    }
    Ok(endpoint)
}

fn route(
    shared: &Arc<Shared>,
    request: &Request,
    reader: &mut ConnectionReader,
    writer: &mut TcpStream,
) -> Reply {
    let endpoint = match resolve(shared, request) {
        Ok(endpoint) => endpoint,
        Err(mut response) => {
            // A body was sent to an endpoint that never reads it.
            response.close |= request.has_body();
            return Reply::Respond(response);
        }
    };
    if endpoint == Endpoint::Stream {
        serve_stream(shared, request, writer);
        return Reply::Streamed;
    }
    let Some(backend) = shared.backend.upgrade() else {
        return Reply::Respond(Response::gone());
    };
    if endpoint.method() == "GET" && request.has_body() {
        return Reply::Respond(Response::text(400, "This request takes no body.").closing());
    }
    let response = match endpoint {
        Endpoint::Page => Response::html(render_page(
            &backend.state().name,
            crate::ui_text::english(),
        )),
        Endpoint::State => match serde_json::to_vec(&backend.state()) {
            Ok(body) => Response::json(body),
            Err(_) => Response::text(500, "The state could not be written."),
        },
        Endpoint::Input => handle_input(shared, &*backend, request, reader, writer),
        Endpoint::Credentials => handle_credentials(shared, &*backend, request, reader, writer),
        Endpoint::Visibility => handle_visibility(shared, &*backend, request, reader, writer),
        Endpoint::Stream => unreachable!("answered above"),
    };
    Reply::Respond(response)
}

fn serve_connection(shared: &Arc<Shared>, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(shared.timing.write));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = ConnectionReader::new(read_half);
    let mut writer = stream;
    for served in 0..MAX_REQUESTS_PER_CONNECTION {
        let request = match read_request(&mut reader, &shared.timing, &shared.stopped) {
            Ok(request) => request,
            Err(ReadError::Closed) => break,
            Err(error) => {
                let _ = write_response(&mut writer, &error.response(), false);
                linger(&mut reader, &writer);
                return;
            }
        };
        match route(shared, &request, &mut reader, &mut writer) {
            Reply::Streamed => break,
            Reply::Respond(response) => {
                let keep_alive = request.keep_alive
                    && !response.close
                    && !shared.stopped()
                    && served + 1 < MAX_REQUESTS_PER_CONNECTION;
                if write_response(&mut writer, &response, keep_alive).is_err() {
                    break;
                }
                if !keep_alive {
                    linger(&mut reader, &writer);
                    return;
                }
            }
        }
    }
    let _ = writer.shutdown(Shutdown::Both);
}

/// Closes politely after an answer that left request bytes unread: closing a
/// socket with unread input makes the system reset the connection, and the
/// client may lose the answer. Say goodbye, then drain a little.
fn linger(reader: &mut ConnectionReader, writer: &TcpStream) {
    let _ = writer.shutdown(Shutdown::Write);
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut sink = [0u8; 4096];
    let mut drained = 0usize;
    while drained < 512 * 1024 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero()
            || reader
                .get_ref()
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
                .is_err()
        {
            break;
        }
        match reader.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(count) => drained += count,
        }
    }
    // What was drained may be a sign-in the server refused to read.
    sink.zeroize();
    reader.wipe_read();
    let _ = writer.shutdown(Shutdown::Both);
}

// ------------------------------------------------------------ POST handlers

fn valid_viewer_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_VIEWER_ID
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn no_content() -> Response {
    Response::new(204)
}

fn handle_input(
    shared: &Shared,
    backend: &dyn ViewerBackend,
    request: &Request,
    reader: &mut ConnectionReader,
    writer: &mut TcpStream,
) -> Response {
    let body = match read_body(shared, request, reader, writer, MAX_INPUT_BODY) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Ok(input) = serde_json::from_slice::<ViewerInput>(&body) else {
        return Response::text(400, "The input batch is not understood.");
    };
    if !valid_viewer_id(&input.viewer) {
        return Response::text(400, "The viewer id is not valid.");
    }
    if input.events.len() > MAX_EVENTS {
        return Response::text(
            400,
            format!("A batch may hold at most {MAX_EVENTS} events."),
        );
    }
    let admitted = lock(&shared.registry).admit_batch(&input.viewer, input.seq);
    if admitted {
        let events = sanitize(input.events);
        if !events.is_empty() {
            backend.input(&input.viewer, events);
        }
    }
    // A stale batch is not an error the page could do anything about.
    no_content()
}

/// Keeps what the session receives inside the ranges its arithmetic assumes. A
/// page computes these in range itself; this is for one that does not.
fn sanitize(events: Vec<ViewerEvent>) -> Vec<ViewerEvent> {
    fn unit(value: f64) -> f64 {
        if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
    fn notches(value: f64) -> f64 {
        if value.is_finite() {
            value.clamp(-10.0, 10.0)
        } else {
            0.0
        }
    }
    const NAME: usize = 64;
    events
        .into_iter()
        .filter_map(|event| match event {
            ViewerEvent::Pointer {
                phase,
                x,
                y,
                buttons,
                at,
            } => Some(ViewerEvent::Pointer {
                phase,
                x: unit(x),
                y: unit(y),
                buttons,
                // A page's clock starts at zero when it loads; anything else is
                // not a time it could have sent.
                at: at.filter(|at| at.is_finite() && (0.0..1e12).contains(at)),
            }),
            ViewerEvent::Wheel { x, y, dx, dy } => Some(ViewerEvent::Wheel {
                x: unit(x),
                y: unit(y),
                dx: notches(dx),
                dy: notches(dy),
            }),
            ViewerEvent::Key {
                key,
                code,
                down,
                alt_graph,
            } => (key.len() <= NAME && code.len() <= NAME).then_some(ViewerEvent::Key {
                key,
                code,
                down,
                alt_graph,
            }),
            ViewerEvent::Text { text } => (!text.is_empty()).then_some(ViewerEvent::Text { text }),
            ViewerEvent::Button { name } => {
                (!name.is_empty() && name.len() <= NAME).then_some(ViewerEvent::Button { name })
            }
        })
        .collect()
}

fn handle_credentials(
    shared: &Shared,
    backend: &dyn ViewerBackend,
    request: &Request,
    reader: &mut ConnectionReader,
    writer: &mut TcpStream,
) -> Response {
    let body = read_body(shared, request, reader, writer, MAX_CREDENTIALS_BODY);
    // The password passed through the connection's buffer on its way.
    reader.wipe_read();
    let body = match body {
        Ok(body) => body,
        Err(response) => return response,
    };
    // The parse error is not passed on: serde quotes the value it choked on, and
    // that value is a password.
    let Ok(credentials) = serde_json::from_slice::<ViewerCredentials>(&body) else {
        return Response::text(400, "The sign-in is not understood.");
    };
    drop(body);
    backend.credentials(credentials);
    no_content()
}

#[derive(Deserialize)]
struct VisibilityBody {
    viewer: String,
    visible: bool,
}

fn handle_visibility(
    shared: &Shared,
    backend: &dyn ViewerBackend,
    request: &Request,
    reader: &mut ConnectionReader,
    writer: &mut TcpStream,
) -> Response {
    let body = match read_body(shared, request, reader, writer, MAX_VISIBILITY_BODY) {
        Ok(body) => body,
        Err(response) => return response,
    };
    match serde_json::from_slice::<VisibilityBody>(&body) {
        Ok(visibility) if valid_viewer_id(&visibility.viewer) => {
            backend.set_viewer_visible(&visibility.viewer, visibility.visible);
            no_content()
        }
        _ => Response::text(400, "The visibility report is not understood."),
    }
}

// --------------------------------------------------------------- the stream

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

/// One open stream of one page. The backend hears "visible" when it opens and
/// "gone" when the page's last one ends, in that order whatever the threads do;
/// then, outside that order, that the page's held keys and buttons can come up
/// ([`ViewerBackend::release_viewer`]), which may wait for the screen and must
/// not hold up another page's stream meanwhile.
struct StreamGuard<'a> {
    shared: &'a Shared,
    viewer: String,
}

impl<'a> StreamGuard<'a> {
    fn open(shared: &'a Shared, backend: &dyn ViewerBackend, viewer: &str) -> Self {
        let _changing = lock(&shared.stream_changes);
        *lock(&shared.registry)
            .streams
            .entry(viewer.to_owned())
            .or_default() += 1;
        backend.set_viewer_visible(viewer, true);
        Self {
            shared,
            viewer: viewer.to_owned(),
        }
    }
}

impl Drop for StreamGuard<'_> {
    fn drop(&mut self) {
        let backend = {
            let _changing = lock(&self.shared.stream_changes);
            let last = {
                let mut registry = lock(&self.shared.registry);
                match registry.streams.get_mut(&self.viewer) {
                    Some(count) if *count > 1 => {
                        *count -= 1;
                        false
                    }
                    Some(_) => {
                        registry.streams.remove(&self.viewer);
                        true
                    }
                    None => false,
                }
            };
            if !last {
                return;
            }
            let Some(backend) = self.shared.backend.upgrade() else {
                return;
            };
            backend.viewer_gone(&self.viewer);
            backend
        };
        backend.release_viewer(&self.viewer);
    }
}

/// Whether the client closed its end. The page sends nothing after its request,
/// so any readable state is a close or a reset.
fn peer_closed(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let mut byte = [0u8; 1];
    let closed = match stream.peek(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(error) => !timed_out(&error),
    };
    let _ = stream.set_nonblocking(false);
    closed
}

fn serve_stream(shared: &Shared, request: &Request, writer: &mut TcpStream) {
    let Some(viewer) = query_param(&request.query, "viewer").filter(|id| valid_viewer_id(id))
    else {
        let _ = write_response(
            writer,
            &Response::text(400, "The stream needs a valid viewer id.").closing(),
            false,
        );
        return;
    };
    let guard = match shared.backend.upgrade() {
        Some(backend) => StreamGuard::open(shared, &*backend, viewer),
        None => {
            let _ = write_response(writer, &Response::gone(), false);
            return;
        }
    };
    // The boundary cannot be guessed, so no picture can contain it.
    let boundary = random_hex(8).unwrap_or_else(|_| format!("{:016x}", shared.port));
    let _ = writer.set_write_timeout(Some(STREAM_WRITE_TIMEOUT));
    let mut head = format!(
        "HTTP/1.1 200 OK\r\n{COMMON_HEADERS}Content-Type: multipart/x-mixed-replace; boundary={boundary}\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    // The first part's opening; every JPEG after this brings the next one's.
    head.extend_from_slice(format!("--{boundary}\r\nContent-Type: image/jpeg\r\n\r\n").as_bytes());
    if writer.write_all(&head).is_err() {
        return;
    }
    let next_part = format!("\r\n--{boundary}\r\nContent-Type: image/jpeg\r\n\r\n").into_bytes();

    let mut sent: Option<u64> = None;
    let mut last_write: Option<Instant> = None;
    let mut packet: Vec<u8> = Vec::new();
    while !shared.stopped() {
        // At most ten frames a second: the picture is what the loop waits for,
        // so wait out the interval first and take the newest frame after it.
        if let Some(rest) = last_write.and_then(|at| STREAM_MIN_INTERVAL.checked_sub(at.elapsed()))
        {
            thread::sleep(rest);
        }
        let turn = Instant::now();
        let wait = last_write.map_or(STREAM_KEEPALIVE, |at| {
            STREAM_KEEPALIVE
                .saturating_sub(at.elapsed())
                .max(Duration::from_millis(1))
        });
        // The session is held only while it is asked, so a stream never keeps
        // one alive.
        let Some(backend) = shared.backend.upgrade() else {
            break;
        };
        let mut frame = match sent {
            None => backend
                .latest_frame()
                .or_else(|| backend.next_frame(0, wait)),
            Some(seq) => backend.next_frame(seq, wait),
        };
        if frame.is_none() {
            if peer_closed(writer) {
                break;
            }
            if let Some(rest) = STREAM_MIN_TURN.checked_sub(turn.elapsed()) {
                thread::sleep(rest);
            }
            if last_write.is_none_or(|at| at.elapsed() >= STREAM_KEEPALIVE) {
                frame = backend.latest_frame();
            }
        }
        drop(backend);
        let Some(frame) = frame else { continue };
        packet.clear();
        packet.extend_from_slice(&frame.jpeg);
        packet.extend_from_slice(&next_part);
        if writer.write_all(&packet).is_err() || writer.flush().is_err() {
            break;
        }
        last_write = Some(Instant::now());
        sent = Some(sent.map_or(frame.seq, |before| before.max(frame.seq)));
    }
    drop(guard);
    let _ = writer.shutdown(Shutdown::Both);
}

// ----------------------------------------------------------------- the page

/// One piece of text the page shows, in both languages the app has.
struct Text {
    key: &'static str,
    zh: &'static str,
    en: &'static str,
}

const fn text(key: &'static str, zh: &'static str, en: &'static str) -> Text {
    Text { key, zh, en }
}

const TEXTS: &[Text] = &[
    text("readonly", "只读", "Read-only"),
    text("model_active", "模型正在操作", "The model is controlling"),
    text("tools", "设备按键", "Device keys"),
    text("screen_label", "远程屏幕", "Remote screen"),
    text("waiting", "等待画面…", "Waiting for the picture…"),
    text("connecting", "正在连接…", "Connecting…"),
    text(
        "reconnecting",
        "连接中断，正在重连…",
        "Connection lost, reconnecting…",
    ),
    text("failed", "无法连接", "Could not connect"),
    text("no_details", "没有更多信息。", "No further details."),
    text(
        "closed",
        "Mewrk 已结束这个会话，或与 Mewrk 的连接断开了。",
        "Mewrk ended this session, or the connection to Mewrk was lost.",
    ),
    text("phase_connecting", "连接中", "Connecting"),
    text("phase_credentials", "需要登录", "Sign-in"),
    text("phase_ready", "已就绪", "Ready"),
    text("phase_reconnecting", "重连中", "Reconnecting"),
    text("phase_failed", "失败", "Failed"),
    text(
        "signin_title",
        "登录远程屏幕",
        "Sign in to the remote screen",
    ),
    text("signin_where", "密码会发送到", "Your password is sent to"),
    text("signin_username", "账户名", "Account name"),
    text("signin_password", "密码", "Password"),
    text(
        "signin_remember",
        "在这台电脑上记住",
        "Remember on this computer",
    ),
    text("signin_submit", "登录", "Sign in"),
    text("signin_busy", "正在登录…", "Signing in…"),
    text(
        "signin_refused",
        "密码被拒绝，请重试。",
        "The password was refused. Try again.",
    ),
    text("tool_back", "返回", "Back"),
    text("tool_home", "主屏幕", "Home"),
    text("tool_recents", "最近任务", "Recents"),
    text("tool_cad", "Ctrl+Alt+Del", "Ctrl+Alt+Del"),
];

const PAGE: &str = include_str!("viewer.html");
const PAGE_STYLE: &str = include_str!("viewer.css");
const PAGE_SCRIPT: &str = include_str!("viewer.js");

fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// The page for an entry called `name`, in the app's language. The words are
/// put in here rather than looked up by the script so the first paint is
/// already in the right language; the script gets the same words as JSON for
/// what it writes later.
fn render_page(name: &str, english: bool) -> String {
    let mut page = PAGE.to_owned();
    let mut words = serde_json::Map::new();
    for entry in TEXTS {
        let word = if english { entry.en } else { entry.zh };
        page = page.replace(&format!("{{{{{}}}}}", entry.key), &escape_html(word));
        words.insert(entry.key.to_owned(), serde_json::Value::from(word));
    }
    // `<` is escaped so no string can close the data block it sits in.
    let words = serde_json::Value::Object(words)
        .to_string()
        .replace('<', "\\u003c");
    page.replace("{{lang}}", if english { "en-US" } else { "zh-CN" })
        .replace("{{TITLE}}", &escape_html(name))
        .replace("{{I18N}}", &words)
        .replace("{{STYLE}}", PAGE_STYLE)
        .replace("{{SCRIPT}}", PAGE_SCRIPT)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::Condvar,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::remote_control::{
        driver::Platform,
        target::Protocol,
        view::{
            CredentialRequest, PointerPhase, ScreenSize, SessionPhase, ViewerFrame, ViewerState,
        },
    };

    /// A session as the server sees it, with everything it was told recorded.
    struct FakeBackend {
        state: Mutex<ViewerState>,
        latest: Mutex<Option<ViewerFrame>>,
        arrived: Condvar,
        inputs: Mutex<Vec<(String, Vec<ViewerEvent>)>>,
        credentials: Mutex<Vec<ViewerCredentials>>,
        visibility: Mutex<Vec<(String, bool)>>,
        gone: Mutex<VecDeque<String>>,
        released: Mutex<Vec<String>>,
        /// How long releasing a page's keys takes, as a stalled screen would make it.
        release_takes: Mutex<Duration>,
    }

    impl FakeBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(ViewerState {
                    name: "pixel".to_owned(),
                    protocol: Protocol::Adb,
                    platform: Platform::Android,
                    endpoint: "this computer · 5037".to_owned(),
                    label: Some("Pixel 8".to_owned()),
                    phase: SessionPhase::Ready,
                    message: None,
                    screen: Some(ScreenSize {
                        width: 1080,
                        height: 2400,
                    }),
                    read_only: false,
                    credentials: None,
                    model_active: false,
                }),
                latest: Mutex::new(None),
                arrived: Condvar::new(),
                inputs: Mutex::new(Vec::new()),
                credentials: Mutex::new(Vec::new()),
                visibility: Mutex::new(Vec::new()),
                gone: Mutex::new(VecDeque::new()),
                released: Mutex::new(Vec::new()),
                release_takes: Mutex::new(Duration::ZERO),
            })
        }

        /// Publishes a frame whose bytes carry its `seq`, so a test can tell
        /// which one it received.
        fn publish(&self, seq: u64) -> Vec<u8> {
            let mut jpeg = vec![0xFF, 0xD8];
            jpeg.extend_from_slice(format!("frame-{seq}-").as_bytes());
            jpeg.extend(std::iter::repeat_n(0xAB, 3000));
            jpeg.extend_from_slice(&[0xFF, 0xD9]);
            *lock(&self.latest) = Some(ViewerFrame {
                jpeg: Arc::from(jpeg.clone()),
                width: 100,
                height: 200,
                seq,
            });
            self.arrived.notify_all();
            jpeg
        }
    }

    impl ViewerBackend for FakeBackend {
        fn state(&self) -> ViewerState {
            lock(&self.state).clone()
        }

        fn next_frame(&self, after: u64, timeout: Duration) -> Option<ViewerFrame> {
            let deadline = Instant::now() + timeout;
            let mut latest = lock(&self.latest);
            loop {
                if let Some(frame) = latest.as_ref().filter(|frame| frame.seq > after) {
                    return Some(frame.clone());
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return None;
                }
                latest = self
                    .arrived
                    .wait_timeout(latest, remaining)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
            }
        }

        fn latest_frame(&self) -> Option<ViewerFrame> {
            lock(&self.latest).clone()
        }

        fn input(&self, viewer: &str, events: Vec<ViewerEvent>) {
            lock(&self.inputs).push((viewer.to_owned(), events));
        }

        fn credentials(&self, credentials: ViewerCredentials) {
            lock(&self.credentials).push(credentials);
        }

        fn set_viewer_visible(&self, viewer: &str, visible: bool) {
            lock(&self.visibility).push((viewer.to_owned(), visible));
        }

        fn viewer_gone(&self, viewer: &str) {
            lock(&self.gone).push_back(viewer.to_owned());
        }

        fn release_viewer(&self, viewer: &str) {
            assert!(
                lock(&self.gone).contains(&viewer.to_owned()),
                "released before it was gone"
            );
            let takes = *lock(&self.release_takes);
            thread::sleep(takes);
            lock(&self.released).push(viewer.to_owned());
        }
    }

    fn start() -> (Arc<FakeBackend>, Viewer) {
        start_timed(Timing::DEFAULT)
    }

    fn start_timed(timing: Timing) -> (Arc<FakeBackend>, Viewer) {
        let backend = FakeBackend::new();
        let dynamic: Arc<dyn ViewerBackend> = backend.clone();
        let viewer = Viewer::start_with(Arc::downgrade(&dynamic), timing).unwrap();
        (backend, viewer)
    }

    fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    struct Answer {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Answer {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }
    }

    fn connect(viewer: &Viewer) -> TcpStream {
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, viewer.port())).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
    }

    /// Reads one response with a `Content-Length` (or none, for a 204).
    fn read_answer(stream: &mut TcpStream) -> Answer {
        let mut buffered = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(position) = buffered.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0, "closed before the response head");
            buffered.extend_from_slice(&chunk[..count]);
        };
        let head = String::from_utf8(buffered[..head_end].to_vec()).unwrap();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers: Vec<(String, String)> = lines
            .map(|line| {
                let (name, value) = line.split_once(": ").unwrap();
                (name.to_owned(), value.to_owned())
            })
            .collect();
        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, value)| value.parse::<usize>().unwrap());
        let mut body = buffered[head_end + 4..].to_vec();
        while body.len() < length {
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0, "closed inside the response body");
            body.extend_from_slice(&chunk[..count]);
        }
        Answer {
            status,
            headers,
            body,
        }
    }

    /// One request on a connection of its own.
    fn fetch(viewer: &Viewer, raw: &str) -> Answer {
        let mut stream = connect(viewer);
        stream.write_all(raw.as_bytes()).unwrap();
        read_answer(&mut stream)
    }

    fn host(viewer: &Viewer) -> String {
        format!("127.0.0.1:{}", viewer.port())
    }

    fn get(viewer: &Viewer, path: &str) -> Answer {
        fetch(
            viewer,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                host(viewer)
            ),
        )
    }

    fn post(viewer: &Viewer, endpoint: &str, body: &str) -> Answer {
        fetch(
            viewer,
            &format!(
                "POST /{}/{endpoint} HTTP/1.1\r\nHost: {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                viewer.token,
                host(viewer),
                body.len()
            ),
        )
    }

    fn page_path(viewer: &Viewer, rest: &str) -> String {
        format!("/{}/{rest}", viewer.token)
    }

    // ---- the page and its guards

    #[test]
    fn the_page_is_served_with_its_security_headers() {
        let (_backend, viewer) = start();
        let answer = get(&viewer, &page_path(&viewer, ""));
        assert_eq!(answer.status, 200);
        assert!(answer
            .header("content-type")
            .unwrap()
            .starts_with("text/html"));
        assert_eq!(answer.header("cache-control"), Some("no-store"));
        assert_eq!(answer.header("referrer-policy"), Some("no-referrer"));
        assert_eq!(answer.header("access-control-allow-origin"), Some("*"));
        assert_eq!(answer.header("x-content-type-options"), Some("nosniff"));
        let policy = answer.header("content-security-policy").unwrap();
        assert!(policy.contains("default-src 'none'"));
        assert!(policy.contains("connect-src 'self'"));
        assert!(policy.contains("frame-ancestors http://127.0.0.1:* http://localhost:*"));
        assert!(answer.text().contains("<title>pixel</title>"));
    }

    #[test]
    fn data_endpoints_carry_the_common_headers_too() {
        let (_backend, viewer) = start();
        for answer in [
            get(&viewer, &page_path(&viewer, "state")),
            get(&viewer, "/nope"),
            post(&viewer, "visibility", "{}"),
        ] {
            assert_eq!(answer.header("cache-control"), Some("no-store"));
            assert_eq!(answer.header("referrer-policy"), Some("no-referrer"));
            assert_eq!(answer.header("access-control-allow-origin"), Some("*"));
            assert_eq!(answer.header("x-content-type-options"), Some("nosniff"));
        }
    }

    #[test]
    fn a_host_header_that_is_not_the_viewer_itself_is_refused() {
        let (_backend, viewer) = start();
        let port = viewer.port();
        let path = page_path(&viewer, "state");
        for host in [
            "evil.com".to_owned(),
            format!("evil.com:{port}"),
            format!("127.0.0.1:{}", port.wrapping_add(1)),
            "127.0.0.1".to_owned(),
            format!("127.0.0.1.evil.com:{port}"),
            format!("[::1]:{port}"),
            format!("127.0.0.1:0{port}"),
        ] {
            let answer = fetch(
                &viewer,
                &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
            );
            assert_eq!(answer.status, 400, "Host: {host}");
        }
        let no_host = fetch(&viewer, &format!("GET {path} HTTP/1.0\r\n\r\n"));
        assert_eq!(no_host.status, 400);
        for host in [format!("127.0.0.1:{port}"), format!("localhost:{port}")] {
            let answer = fetch(
                &viewer,
                &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
            );
            assert_eq!(answer.status, 200, "Host: {host}");
        }
    }

    #[test]
    fn a_wrong_token_or_unknown_path_is_not_found() {
        let (_backend, viewer) = start();
        let mut wrong = viewer.token.clone();
        wrong.replace_range(31.., if wrong.ends_with('0') { "1" } else { "0" });
        let paths = [
            format!("/{wrong}/"),
            format!("/{wrong}/state"),
            format!("/{}x/state", viewer.token),
            format!("/{}/", &viewer.token[..31]),
            "/".to_owned(),
            "/state".to_owned(),
            page_path(&viewer, "nope"),
            page_path(&viewer, "state/extra"),
        ];
        for path in paths {
            assert_eq!(get(&viewer, &path).status, 404, "{path}");
        }
        let preflight = fetch(
            &viewer,
            &format!(
                "OPTIONS /{wrong}/input HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                host(&viewer)
            ),
        );
        assert_eq!(preflight.status, 404);
    }

    #[test]
    fn the_token_path_without_its_slash_redirects_to_it() {
        let (_backend, viewer) = start();
        let answer = get(&viewer, &format!("/{}", viewer.token));
        assert_eq!(answer.status, 308);
        assert_eq!(
            answer.header("location"),
            Some(format!("/{}/", viewer.token).as_str())
        );
    }

    #[test]
    fn methods_an_endpoint_does_not_have_are_refused() {
        let (_backend, viewer) = start();
        let wrong_post = post(&viewer, "state", "{}");
        assert_eq!(wrong_post.status, 405);
        assert_eq!(wrong_post.header("allow"), Some("GET"));
        let wrong_get = get(&viewer, &page_path(&viewer, "input"));
        assert_eq!(wrong_get.status, 405);
        assert_eq!(wrong_get.header("allow"), Some("POST"));
    }

    #[test]
    fn state_is_the_backends_state_as_json() {
        let (backend, viewer) = start();
        lock(&backend.state).credentials = Some(CredentialRequest {
            username: Some("holycat".to_owned()),
            needs_username: true,
            retry: true,
        });
        lock(&backend.state).phase = SessionPhase::Credentials;
        let answer = get(&viewer, &page_path(&viewer, "state"));
        assert_eq!(answer.status, 200);
        assert_eq!(answer.header("content-type"), Some("application/json"));
        let state: serde_json::Value = serde_json::from_slice(&answer.body).unwrap();
        assert_eq!(state["name"], "pixel");
        assert_eq!(state["protocol"], "adb");
        assert_eq!(state["platform"], "android");
        assert_eq!(state["endpoint"], "this computer · 5037");
        assert_eq!(state["label"], "Pixel 8");
        assert_eq!(state["phase"], "credentials");
        assert_eq!(state["screen"]["width"], 1080);
        assert_eq!(state["screen"]["height"], 2400);
        assert_eq!(state["readOnly"], false);
        assert_eq!(state["modelActive"], false);
        assert_eq!(state["credentials"]["username"], "holycat");
        assert_eq!(state["credentials"]["needsUsername"], true);
        assert_eq!(state["credentials"]["retry"], true);
        assert!(state["message"].is_null());
    }

    #[test]
    fn a_preflight_is_answered_permissively_for_the_token_paths() {
        let (_backend, viewer) = start();
        for endpoint in ["", "state", "stream", "input", "credentials", "visibility"] {
            let answer = fetch(
                &viewer,
                &format!(
                    "OPTIONS /{}/{endpoint} HTTP/1.1\r\nHost: {}\r\nOrigin: null\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: content-type\r\nConnection: close\r\n\r\n",
                    viewer.token,
                    host(&viewer)
                ),
            );
            assert_eq!(answer.status, 204, "{endpoint}");
            assert_eq!(answer.header("access-control-allow-origin"), Some("*"));
            assert_eq!(
                answer.header("access-control-allow-methods"),
                Some("GET, POST, OPTIONS")
            );
            assert_eq!(
                answer.header("access-control-allow-headers"),
                Some("Content-Type")
            );
            assert!(answer.body.is_empty());
        }
    }

    #[test]
    fn requests_share_one_kept_alive_connection() {
        let (_backend, viewer) = start();
        let mut stream = connect(&viewer);
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\n\r\n",
            page_path(&viewer, "state"),
            host(&viewer)
        );
        for _ in 0..3 {
            stream.write_all(request.as_bytes()).unwrap();
            let answer = read_answer(&mut stream);
            assert_eq!(answer.status, 200);
            assert_eq!(answer.header("connection"), Some("keep-alive"));
        }
        let body = r#"{"viewer":"v1","seq":1,"events":[]}"#;
        stream
            .write_all(
                format!(
                    "POST /{}/input HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n\r\n{body}",
                    viewer.token,
                    host(&viewer),
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(read_answer(&mut stream).status, 204);
    }

    #[test]
    fn a_request_head_over_16_kib_is_refused() {
        let (_backend, viewer) = start();
        let mut stream = connect(&viewer);
        let padding = "a".repeat(17 * 1024);
        stream
            .write_all(
                format!(
                    "GET {} HTTP/1.1\r\nHost: {}\r\nX-Padding: {padding}\r\n\r\n",
                    page_path(&viewer, "state"),
                    host(&viewer)
                )
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(read_answer(&mut stream).status, 431);
    }

    #[test]
    fn a_head_that_never_finishes_is_dropped() {
        let (_backend, viewer) = start_timed(Timing {
            head: Duration::from_millis(300),
            ..Timing::DEFAULT
        });
        let mut stream = connect(&viewer);
        stream.write_all(b"GET /abc HTTP/1.1\r\nHost: x").unwrap();
        let started = Instant::now();
        assert_eq!(read_answer(&mut stream).status, 408);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_idle_connection_is_closed() {
        let (_backend, viewer) = start_timed(Timing {
            idle: Duration::from_millis(300),
            ..Timing::DEFAULT
        });
        let mut stream = connect(&viewer);
        let mut byte = [0u8; 1];
        let started = Instant::now();
        assert_eq!(stream.read(&mut byte).unwrap_or(0), 0);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn connections_beyond_the_cap_are_refused_until_one_closes() {
        let (_backend, viewer) = start();
        let mut held: Vec<TcpStream> = (0..MAX_CONNECTIONS).map(|_| connect(&viewer)).collect();
        eventually("all connections registered", || {
            lock(&viewer.shared.registry).connections.len() == MAX_CONNECTIONS
        });
        let mut refused = connect(&viewer);
        assert_eq!(read_answer(&mut refused).status, 503);
        held.pop();
        eventually("a slot to free", || {
            lock(&viewer.shared.registry).connections.len() < MAX_CONNECTIONS
        });
        assert_eq!(get(&viewer, &page_path(&viewer, "state")).status, 200);
    }

    // ---- input, sign-in, visibility

    fn batch(viewer: &str, seq: u64, text: &str) -> String {
        format!(r#"{{"viewer":"{viewer}","seq":{seq},"events":[{{"t":"text","text":"{text}"}}]}}"#)
    }

    fn typed(backend: &FakeBackend) -> Vec<String> {
        lock(&backend.inputs)
            .iter()
            .flat_map(|(viewer, events)| {
                events.iter().map(move |event| match event {
                    ViewerEvent::Text { text } => format!("{viewer}:{text}"),
                    other => format!("{viewer}:{other:?}"),
                })
            })
            .collect()
    }

    #[test]
    fn input_batches_apply_in_order_and_stale_ones_are_dropped() {
        let (backend, viewer) = start();
        for (id, seq, text) in [
            ("a", 1, "one"),
            ("a", 3, "three"),
            ("a", 2, "stale"),
            ("a", 3, "again"),
            ("b", 2, "other-viewer"),
            ("a", 4, "four"),
        ] {
            assert_eq!(post(&viewer, "input", &batch(id, seq, text)).status, 204);
        }
        assert_eq!(
            typed(&backend),
            ["a:one", "a:three", "b:other-viewer", "a:four"]
        );
    }

    #[test]
    fn input_events_arrive_as_the_page_wrote_them() {
        let (backend, viewer) = start();
        let body = r#"{"viewer":"v-1","seq":1,"events":[
            {"t":"pointer","phase":"down","x":0.5,"y":0.25,"buttons":1,"at":812.5},
            {"t":"wheel","x":0.1,"y":0.2,"dx":0,"dy":1.5},
            {"t":"key","key":"a","code":"KeyA","down":true},
            {"t":"key","key":"@","code":"KeyQ","down":true,"altGraph":true},
            {"t":"button","name":"Back"}
        ]}"#;
        assert_eq!(post(&viewer, "input", body).status, 204);
        let inputs = lock(&backend.inputs);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].0, "v-1");
        assert_eq!(
            inputs[0].1[0],
            ViewerEvent::Pointer {
                phase: PointerPhase::Down,
                x: 0.5,
                y: 0.25,
                buttons: 1,
                at: Some(812.5)
            }
        );
        // A page that does not say whether AltGr is held is read as not holding it.
        assert_eq!(
            inputs[0].1[2],
            ViewerEvent::Key {
                key: "a".into(),
                code: "KeyA".into(),
                down: true,
                alt_graph: false
            }
        );
        assert_eq!(
            inputs[0].1[3],
            ViewerEvent::Key {
                key: "@".into(),
                code: "KeyQ".into(),
                down: true,
                alt_graph: true
            }
        );
        assert_eq!(inputs[0].1.len(), 5);
    }

    #[test]
    fn out_of_range_input_is_clamped_and_absurd_names_dropped() {
        let (backend, viewer) = start();
        let long = "k".repeat(200);
        let body = format!(
            r#"{{"viewer":"v","seq":1,"events":[
            {{"t":"pointer","phase":"move","x":-3,"y":9e9,"buttons":0,"at":-5}},
            {{"t":"wheel","x":0.5,"y":0.5,"dx":-500,"dy":500}},
            {{"t":"key","key":"{long}","code":"KeyA","down":true}},
            {{"t":"text","text":""}},
            {{"t":"button","name":""}}
        ]}}"#
        );
        assert_eq!(post(&viewer, "input", &body).status, 204);
        let inputs = lock(&backend.inputs);
        assert_eq!(inputs[0].1.len(), 2);
        assert_eq!(
            inputs[0].1[0],
            ViewerEvent::Pointer {
                phase: PointerPhase::Move,
                x: 0.0,
                y: 1.0,
                buttons: 0,
                at: None
            }
        );
        assert_eq!(
            inputs[0].1[1],
            ViewerEvent::Wheel {
                x: 0.5,
                y: 0.5,
                dx: -10.0,
                dy: 10.0
            }
        );
    }

    #[test]
    fn malformed_and_oversized_input_is_refused() {
        let (backend, viewer) = start();
        assert_eq!(post(&viewer, "input", "not json").status, 400);
        assert_eq!(post(&viewer, "input", "").status, 400);
        assert_eq!(
            post(&viewer, "input", &batch("bad id!", 1, "x")).status,
            400
        );
        assert_eq!(post(&viewer, "input", &batch("", 1, "x")).status, 400);
        let many = format!(
            r#"{{"viewer":"v","seq":1,"events":[{}]}}"#,
            vec![r#"{"t":"button","name":"Back"}"#; MAX_EVENTS + 1].join(",")
        );
        assert_eq!(post(&viewer, "input", &many).status, 400);
        let too_big = "x".repeat(MAX_INPUT_BODY + 1);
        assert_eq!(post(&viewer, "input", &too_big).status, 413);
        assert!(lock(&backend.inputs).is_empty());
        // A refused batch did not use up its seq.
        assert_eq!(post(&viewer, "input", &batch("v", 1, "ok")).status, 204);
        assert_eq!(typed(&backend), ["v:ok"]);
    }

    #[test]
    fn a_post_needs_a_length_and_cannot_be_chunked() {
        let (_backend, viewer) = start();
        let no_length = fetch(
            &viewer,
            &format!(
                "POST /{}/input HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                viewer.token,
                host(&viewer)
            ),
        );
        assert_eq!(no_length.status, 411);
        let chunked = fetch(
            &viewer,
            &format!(
                "POST /{}/input HTTP/1.1\r\nHost: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n",
                viewer.token,
                host(&viewer)
            ),
        );
        assert_eq!(chunked.status, 400);
    }

    #[test]
    fn a_body_that_stalls_is_dropped() {
        let (_backend, viewer) = start_timed(Timing {
            body: Duration::from_millis(300),
            ..Timing::DEFAULT
        });
        let mut stream = connect(&viewer);
        stream
            .write_all(
                format!(
                    "POST /{}/input HTTP/1.1\r\nHost: {}\r\nContent-Length: 100\r\n\r\n{{\"viewer\"",
                    viewer.token,
                    host(&viewer)
                )
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(read_answer(&mut stream).status, 408);
    }

    #[test]
    fn a_body_over_the_limit_is_refused_without_reading_it() {
        let (_backend, viewer) = start();
        let mut stream = connect(&viewer);
        stream
            .write_all(
                format!(
                    "POST /{}/credentials HTTP/1.1\r\nHost: {}\r\nContent-Length: 20000\r\n\r\n",
                    viewer.token,
                    host(&viewer)
                )
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(read_answer(&mut stream).status, 413);
    }

    /// `hunter2-secret`, as the page encodes it.
    const SECRET_BASE64: &str = "aHVudGVyMi1zZWNyZXQ=";

    #[test]
    fn credentials_are_delivered_and_never_echoed() {
        let (backend, viewer) = start();
        let answer = post(
            &viewer,
            "credentials",
            &format!(
                r#"{{"username":"holycat","passwordBase64":"{SECRET_BASE64}","remember":true}}"#
            ),
        );
        assert_eq!(answer.status, 204);
        {
            let delivered = lock(&backend.credentials);
            assert_eq!(delivered.len(), 1);
            assert_eq!(delivered[0].username.as_deref(), Some("holycat"));
            assert_eq!(delivered[0].password.as_str(), "hunter2-secret");
            assert!(delivered[0].remember);
            assert!(!format!("{:?}", delivered[0]).contains("hunter2-secret"));
        }
        // A body of the wrong shape: serde would quote the password back.
        let wrong_type = format!(r#"{{"passwordBase64":"{SECRET_BASE64}","remember":"hunter2-secret"}}"#);
        let no_remember = format!(r#"{{"passwordBase64":"{SECRET_BASE64}"}}"#);
        for body in [
            wrong_type.as_str(),
            no_remember.as_str(),
            r#"{"password":"hunter2-secret","remember":true}"#,
            r#"[ "hunter2-secret" ]"#,
        ] {
            let refused = post(&viewer, "credentials", body);
            assert_eq!(refused.status, 400);
            assert!(!refused.text().contains("hunter2"));
            assert!(!refused.text().contains(SECRET_BASE64));
            assert!(refused
                .headers
                .iter()
                .all(|(_, value)| !value.contains("hunter2") && !value.contains(SECRET_BASE64)));
        }
        assert_eq!(lock(&backend.credentials).len(), 1);
    }

    #[test]
    fn a_password_only_sign_in_has_no_username() {
        let (backend, viewer) = start();
        let answer = post(
            &viewer,
            "credentials",
            r#"{"passwordBase64":"cHc=","remember":false}"#,
        );
        assert_eq!(answer.status, 204);
        let delivered = lock(&backend.credentials);
        assert_eq!(delivered[0].username, None);
        assert_eq!(delivered[0].password.as_str(), "pw");
        assert!(!delivered[0].remember);
    }

    #[test]
    fn visibility_reports_reach_the_backend() {
        let (backend, viewer) = start();
        assert_eq!(
            post(&viewer, "visibility", r#"{"viewer":"v1","visible":false}"#).status,
            204
        );
        assert_eq!(
            post(&viewer, "visibility", r#"{"viewer":"v1","visible":true}"#).status,
            204
        );
        assert_eq!(
            post(&viewer, "visibility", r#"{"viewer":"v1"}"#).status,
            400
        );
        assert_eq!(
            post(&viewer, "visibility", r#"{"viewer":"a b","visible":true}"#).status,
            400
        );
        assert_eq!(
            *lock(&backend.visibility),
            [("v1".to_owned(), false), ("v1".to_owned(), true)]
        );
    }

    // ---- the stream

    struct StreamReader {
        stream: TcpStream,
        boundary: String,
        buffered: Vec<u8>,
    }

    impl StreamReader {
        /// Opens the stream and reads the response head.
        fn open(viewer: &Viewer, id: &str) -> Self {
            let mut stream = connect(viewer);
            stream
                .write_all(
                    format!(
                        "GET /{}/stream?viewer={id}&t=1 HTTP/1.1\r\nHost: {}\r\n\r\n",
                        viewer.token,
                        host(viewer)
                    )
                    .as_bytes(),
                )
                .unwrap();
            let mut reader = Self {
                stream,
                boundary: String::new(),
                buffered: Vec::new(),
            };
            let head = reader.read_until(b"\r\n\r\n");
            let head = String::from_utf8(head).unwrap();
            assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
            assert!(head.contains("Cache-Control: no-store"));
            let content_type = head
                .lines()
                .find_map(|line| line.strip_prefix("Content-Type: "))
                .unwrap();
            reader.boundary = content_type
                .strip_prefix("multipart/x-mixed-replace; boundary=")
                .unwrap()
                .to_owned();
            reader
        }

        fn fill(&mut self) -> bool {
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) | Err(_) => false,
                Ok(count) => {
                    self.buffered.extend_from_slice(&chunk[..count]);
                    true
                }
            }
        }

        /// Everything up to and including `marker`, removed from the buffer.
        fn read_until(&mut self, marker: &[u8]) -> Vec<u8> {
            loop {
                if let Some(position) = self
                    .buffered
                    .windows(marker.len())
                    .position(|window| window == marker)
                {
                    return self.buffered.drain(..position + marker.len()).collect();
                }
                assert!(self.fill(), "stream ended while waiting for {marker:?}");
            }
        }

        fn take(&mut self, count: usize) -> Vec<u8> {
            while self.buffered.len() < count {
                assert!(self.fill(), "stream ended");
            }
            self.buffered.drain(..count).collect()
        }

        fn part_opening(&self, first: bool) -> Vec<u8> {
            let lead = if first { "" } else { "\r\n" };
            format!(
                "{lead}--{}\r\nContent-Type: image/jpeg\r\n\r\n",
                self.boundary
            )
            .into_bytes()
        }

        /// Reads the first part: its opening, then `jpeg`; and asserts that the
        /// next part's opening follows with no new frame published.
        fn expect_first(&mut self, jpeg: &[u8]) {
            let opening = self.part_opening(true);
            assert_eq!(self.take(opening.len()), opening);
            self.expect_body(jpeg);
        }

        /// Reads `jpeg` and the opening of the part after it.
        fn expect_body(&mut self, jpeg: &[u8]) {
            assert!(
                self.take(jpeg.len()) == jpeg,
                "the picture bytes differ from the published frame"
            );
            let next = self.part_opening(false);
            assert_eq!(
                self.take(next.len()),
                next,
                "the next part's boundary and headers must follow the picture at once"
            );
        }

        fn ended(&mut self) -> bool {
            self.stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let mut chunk = [0u8; 1024];
                match self.stream.read(&mut chunk) {
                    Ok(0) => return true,
                    Ok(_) => {}
                    Err(error) if timed_out(&error) => {}
                    Err(_) => return true,
                }
            }
            false
        }
    }

    #[test]
    fn the_stream_writes_the_next_parts_headers_right_after_each_picture() {
        let (backend, viewer) = start();
        let first = backend.publish(1);
        let mut stream = StreamReader::open(&viewer, "page-1");
        // Nothing else is published: the opening of the next part is there
        // anyway, which is what makes the browser show this picture now.
        stream.expect_first(&first);
        let second = backend.publish(2);
        stream.expect_body(&second);
        let third = backend.publish(3);
        stream.expect_body(&third);
        assert_eq!(
            *lock(&backend.visibility),
            [("page-1".to_owned(), true)],
            "opening a stream makes the page visible"
        );
    }

    #[test]
    fn a_stream_opened_before_the_first_frame_waits_for_it() {
        let (backend, viewer) = start();
        let mut stream = StreamReader::open(&viewer, "early");
        thread::sleep(Duration::from_millis(300));
        let first = backend.publish(1);
        stream.expect_first(&first);
    }

    #[test]
    fn a_frame_numbered_zero_is_still_shown() {
        let (backend, viewer) = start();
        let first = backend.publish(0);
        let mut stream = StreamReader::open(&viewer, "zero");
        stream.expect_first(&first);
    }

    #[test]
    fn a_still_screen_is_resent_every_second() {
        let (backend, viewer) = start();
        let first = backend.publish(1);
        let mut stream = StreamReader::open(&viewer, "still");
        stream.expect_first(&first);
        let started = Instant::now();
        stream.expect_body(&first);
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(800) && waited < Duration::from_millis(2500),
            "resent after {waited:?}"
        );
    }

    #[test]
    fn the_stream_shows_at_most_ten_frames_a_second() {
        let (backend, viewer) = start();
        backend.publish(1);
        let mut stream = StreamReader::open(&viewer, "fast");
        let mut arrived = Vec::new();
        let producer = {
            let backend = backend.clone();
            thread::spawn(move || {
                for seq in 2..=60 {
                    thread::sleep(Duration::from_millis(10));
                    backend.publish(seq);
                }
            })
        };
        let started = Instant::now();
        let opening = stream.part_opening(true);
        stream.take(opening.len());
        loop {
            let marker = stream.part_opening(false);
            let part = stream.read_until(&marker);
            let at = started.elapsed();
            if at >= Duration::from_millis(900) {
                break;
            }
            let text = String::from_utf8_lossy(&part).into_owned();
            let seq: u64 = text
                .split("frame-")
                .nth(1)
                .and_then(|rest| rest.split('-').next())
                .and_then(|number| number.parse().ok())
                .unwrap();
            arrived.push((seq, at));
        }
        producer.join().unwrap();
        assert!(
            arrived.len() >= 5 && arrived.len() <= 11,
            "{} frames in 0.9 s: {arrived:?}",
            arrived.len()
        );
        for pair in arrived.windows(2) {
            assert!(pair[1].0 > pair[0].0, "frames out of order: {arrived:?}");
            let gap = pair[1].1 - pair[0].1;
            assert!(gap >= Duration::from_millis(60), "{gap:?} between {pair:?}");
        }
    }

    #[test]
    fn a_closed_stream_tells_the_backend_the_page_is_gone() {
        let (backend, viewer) = start();
        let first = backend.publish(1);
        let mut stream = StreamReader::open(&viewer, "leaving");
        stream.expect_first(&first);
        drop(stream);
        eventually("viewer_gone", || !lock(&backend.gone).is_empty());
        assert_eq!(*lock(&backend.gone), ["leaving"]);
    }

    /// Letting a page's keys up can wait on a stalled screen; another page's stream must not
    /// wait with it.
    #[test]
    fn a_slow_release_does_not_hold_up_another_pages_stream() {
        let (backend, viewer) = start();
        *lock(&backend.release_takes) = Duration::from_millis(1500);
        let first = backend.publish(1);
        let mut leaving = StreamReader::open(&viewer, "leaving");
        leaving.expect_first(&first);
        drop(leaving);
        eventually("viewer_gone", || !lock(&backend.gone).is_empty());
        let opened = Instant::now();
        let mut other = StreamReader::open(&viewer, "other");
        other.expect_first(&first);
        assert!(
            opened.elapsed() < Duration::from_millis(1000),
            "the new stream waited {:?} for the release",
            opened.elapsed()
        );
        assert!(lock(&backend.released).is_empty());
        eventually("the release", || !lock(&backend.released).is_empty());
        assert_eq!(*lock(&backend.released), ["leaving"]);
    }

    #[test]
    fn a_closed_stream_is_noticed_before_any_frame_exists() {
        let (backend, viewer) = start();
        let stream = StreamReader::open(&viewer, "never-saw-one");
        drop(stream);
        eventually("viewer_gone", || !lock(&backend.gone).is_empty());
    }

    #[test]
    fn a_page_is_gone_only_when_its_last_stream_ends() {
        let (backend, viewer) = start();
        let first = backend.publish(1);
        let mut old = StreamReader::open(&viewer, "same-page");
        old.expect_first(&first);
        let mut new = StreamReader::open(&viewer, "same-page");
        new.expect_first(&first);
        drop(old);
        thread::sleep(Duration::from_millis(1500));
        assert!(lock(&backend.gone).is_empty());
        let second = backend.publish(2);
        // Resends of the first picture may come before the new one.
        loop {
            let marker = new.part_opening(false);
            if new.read_until(&marker).starts_with(&second) {
                break;
            }
        }
        drop(new);
        eventually("viewer_gone", || !lock(&backend.gone).is_empty());
        assert_eq!(*lock(&backend.gone), ["same-page"]);
    }

    #[test]
    fn a_stream_without_a_valid_viewer_id_is_refused() {
        let (_backend, viewer) = start();
        assert_eq!(get(&viewer, &page_path(&viewer, "stream")).status, 400);
        assert_eq!(
            get(&viewer, &page_path(&viewer, "stream?viewer=a%20b")).status,
            400
        );
    }

    // ---- stopping and the backend going away

    #[test]
    fn stop_closes_open_streams_and_the_listener() {
        let (backend, viewer) = start();
        let first = backend.publish(1);
        let mut stream = StreamReader::open(&viewer, "page");
        stream.expect_first(&first);
        let port = viewer.port();
        let started = Instant::now();
        viewer.stop();
        assert!(started.elapsed() < Duration::from_secs(3), "stop was slow");
        assert!(stream.ended(), "the stream stayed open after stop");
        eventually("viewer_gone after stop", || !lock(&backend.gone).is_empty());
        assert!(
            TcpStream::connect_timeout(
                &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                Duration::from_secs(1)
            )
            .is_err(),
            "the port is still accepting"
        );
        // Idempotent.
        viewer.stop();
    }

    #[test]
    fn stop_wakes_a_connection_waiting_for_its_next_request() {
        let (_backend, viewer) = start();
        let mut stream = connect(&viewer);
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\n\r\n",
            page_path(&viewer, "state"),
            host(&viewer)
        );
        stream.write_all(request.as_bytes()).unwrap();
        assert_eq!(read_answer(&mut stream).status, 200);
        viewer.stop();
        let mut byte = [0u8; 1];
        assert_eq!(stream.read(&mut byte).unwrap_or(0), 0);
    }

    #[test]
    fn dropping_the_viewer_stops_it() {
        let (_backend, viewer) = start();
        let port = viewer.port();
        drop(viewer);
        assert!(TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            Duration::from_secs(1)
        )
        .is_err());
    }

    #[test]
    fn a_session_that_is_gone_answers_410() {
        let (backend, viewer) = start();
        drop(backend);
        for path in ["", "state"] {
            assert_eq!(get(&viewer, &page_path(&viewer, path)).status, 410);
        }
        assert_eq!(
            post(&viewer, "visibility", r#"{"viewer":"v","visible":true}"#).status,
            410
        );
        assert_eq!(
            get(&viewer, &page_path(&viewer, "stream?viewer=v")).status,
            410
        );
    }

    #[test]
    fn a_stream_ends_when_its_session_goes_away() {
        let (backend, viewer) = start();
        let first = backend.publish(1);
        let mut stream = StreamReader::open(&viewer, "orphan");
        stream.expect_first(&first);
        drop(backend);
        assert!(stream.ended());
    }

    // ---- addresses

    #[test]
    fn shows_matches_only_this_viewers_origin_and_token_path() {
        let (_backend, viewer) = start();
        let port = viewer.port();
        let token = viewer.token.clone();
        assert_eq!(viewer.url(), format!("http://127.0.0.1:{port}/{token}/"));
        for url in [
            format!("http://127.0.0.1:{port}/{token}/"),
            format!("http://127.0.0.1:{port}/{token}"),
            format!("http://127.0.0.1:{port}/{token}/?t=1#top"),
            format!("http://127.0.0.1:{port}/{token}/state"),
            format!("http://localhost:{port}/{token}/"),
            format!("http://LOCALHOST:{port}/{token}/"),
        ] {
            assert!(viewer.shows(&url), "{url}");
        }
        for url in [
            format!("http://127.0.0.1:{}/{token}/", port.wrapping_add(1)),
            format!("https://127.0.0.1:{port}/{token}/"),
            format!("http://127.0.0.1/{token}/"),
            format!("http://example.com:{port}/{token}/"),
            format!("http://[::1]:{port}/{token}/"),
            format!("http://127.0.0.1:{port}/"),
            format!("http://127.0.0.1:{port}/{token}x/"),
            format!("http://127.0.0.1:{port}/x/{token}/"),
            format!("http://127.0.0.1:{port}//{token}/"),
            format!("http://127.0.0.1:{port}/{}/", "0".repeat(32)),
            "about:blank".to_owned(),
            "not a url".to_owned(),
            String::new(),
        ] {
            assert!(!viewer.shows(&url), "{url}");
        }
    }

    #[test]
    fn two_viewers_have_their_own_port_and_token() {
        let (_first_backend, first) = start();
        let (_second_backend, second) = start();
        assert_ne!(first.port(), second.port());
        assert_ne!(first.token, second.token);
        assert_eq!(first.token.len(), 32);
        assert!(first.token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!first.shows(&second.url()));
        assert!(!second.shows(&first.url()));
    }

    // ---- the page

    #[test]
    fn the_page_is_self_contained() {
        for english in [false, true] {
            let page = render_page("a \"quoted\" <name>", english);
            assert!(!page.contains("{{"), "an unfilled placeholder");
            assert!(page.contains("a &quot;quoted&quot; &lt;name&gt;"));
            let without_loopback = page.replace("127.0.0.1", "");
            assert!(
                !without_loopback.contains("http"),
                "the page mentions an address: {}",
                without_loopback
                    .match_indices("http")
                    .map(|(at, _)| &without_loopback
                        [at.saturating_sub(20)..(at + 40).min(without_loopback.len())])
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            assert!(!page.contains("//cdn") && !page.contains("url("));
            // The script and the data block cannot be closed from inside.
            assert_eq!(page.matches("</script").count(), 2);
            assert_eq!(page.matches("<script").count(), 2);
        }
    }

    #[test]
    fn the_page_speaks_the_apps_language() {
        let chinese = render_page("pixel", false);
        let english = render_page("pixel", true);
        assert!(chinese.contains(r#"lang="zh-CN""#) && english.contains(r#"lang="en-US""#));
        assert!(chinese.contains("只读") && !chinese.contains("Read-only"));
        assert!(english.contains("Read-only") && !english.contains("只读"));
        assert!(chinese.contains("登录远程屏幕"));
        assert!(english.contains("Sign in to the remote screen"));
        // The same words are there for the script.
        let data = english
            .split(r#"<script type="application/json" id="i18n">"#)
            .nth(1)
            .and_then(|rest| rest.split("</script>").next())
            .unwrap();
        let words: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(
            words["signin_refused"],
            "The password was refused. Try again."
        );
        for entry in TEXTS {
            assert!(words.get(entry.key).is_some(), "{}", entry.key);
        }
    }

    #[test]
    fn every_text_has_both_languages_and_is_used() {
        let mut keys = std::collections::HashSet::new();
        for entry in TEXTS {
            assert!(
                !entry.zh.is_empty() && !entry.en.is_empty(),
                "{}",
                entry.key
            );
            assert!(keys.insert(entry.key), "duplicate key {}", entry.key);
            let used = PAGE.contains(&format!("{{{{{}}}}}", entry.key))
                || PAGE_SCRIPT.contains(&format!("T.{}", entry.key))
                || PAGE_SCRIPT.contains(&format!("'{}'", entry.key));
            assert!(used, "{} is never shown", entry.key);
        }
    }

    #[test]
    fn the_served_page_is_the_rendered_page() {
        let (_backend, viewer) = start();
        let served = get(&viewer, &page_path(&viewer, "")).text();
        assert!(served.starts_with("<!doctype html>"));
        assert!(served.contains("<title>pixel</title>"));
        assert!(!served.contains("{{"));
    }

    /// Applies `key=value` words (`platform`, `phase`, `readonly`, `model`,
    /// `username`, `retry`, `message`) to a state, for the manual page.
    fn set_state_from_words(state: &mut ViewerState, words: &str) {
        let mut needs_username = false;
        let mut retry = false;
        for word in words.split_whitespace() {
            let Some((key, value)) = word.split_once('=') else {
                continue;
            };
            let on = value == "1";
            match key {
                "platform" => {
                    state.platform = match value {
                        "ios" => Platform::Ios,
                        "desktop" => Platform::Desktop,
                        _ => Platform::Android,
                    }
                }
                "phase" => {
                    state.phase = match value {
                        "connecting" => SessionPhase::Connecting,
                        "credentials" => SessionPhase::Credentials,
                        "reconnecting" => SessionPhase::Reconnecting,
                        "failed" => SessionPhase::Failed,
                        _ => SessionPhase::Ready,
                    }
                }
                "readonly" => state.read_only = on,
                "model" => state.model_active = on,
                "username" => needs_username = on,
                "retry" => retry = on,
                "message" => state.message = Some(value.replace('_', " ")),
                _ => {}
            }
        }
        state.credentials = (state.phase == SessionPhase::Credentials).then(|| CredentialRequest {
            username: Some("holycat".to_owned()),
            needs_username,
            retry,
        });
    }

    /// Serves a live fake session for a browser to open. Not part of the suite:
    /// run it with `--ignored --nocapture` and open the printed address; the
    /// page's input is printed as it arrives.
    #[test]
    #[ignore = "manual: serves a page for a browser"]
    fn serve_a_fake_session_for_a_browser() {
        let (backend, viewer) = start();
        println!("VIEWER_URL {}", viewer.url());
        let painter = {
            let backend = backend.clone();
            thread::spawn(move || {
                let (width, height) = (360u16, 640u16);
                for step in 1u64.. {
                    let mut rgb = Vec::with_capacity(usize::from(width) * usize::from(height) * 3);
                    for y in 0..height {
                        for x in 0..width {
                            let corner = x < 40 && y < 40;
                            let bar = u64::from(x) / 40 == step / 4 % 9;
                            rgb.extend_from_slice(&if corner {
                                [255, 0, 0]
                            } else if bar {
                                [255, 255, 255]
                            } else {
                                [(x % 256) as u8, (y % 256) as u8, (step % 256) as u8]
                            });
                        }
                    }
                    let mut jpeg = Vec::new();
                    jpeg_encoder::Encoder::new(&mut jpeg, 70)
                        .encode(&rgb, width, height, jpeg_encoder::ColorType::Rgb)
                        .unwrap();
                    *lock(&backend.latest) = Some(ViewerFrame {
                        jpeg: Arc::from(jpeg),
                        width: u32::from(width),
                        height: u32::from(height),
                        seq: step,
                    });
                    backend.arrived.notify_all();
                    thread::sleep(Duration::from_millis(250));
                }
            })
        };
        let control = std::env::var("VIEWER_CONTROL").ok();
        let mut applied = String::new();
        let mut reported = 0;
        let deadline = Instant::now() + Duration::from_secs(900);
        while Instant::now() < deadline && !painter.is_finished() {
            if let Some(words) = control
                .as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
            {
                if words != applied {
                    set_state_from_words(&mut lock(&backend.state), &words);
                    applied = words;
                }
            }
            {
                let inputs = lock(&backend.inputs);
                for (viewer_id, events) in inputs.iter().skip(reported) {
                    println!("INPUT {viewer_id} {events:?}");
                }
                reported = inputs.len();
            }
            for credentials in lock(&backend.credentials).drain(..) {
                println!("CREDENTIALS {credentials:?}");
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}
