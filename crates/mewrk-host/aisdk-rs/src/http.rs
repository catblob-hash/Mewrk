//! The `fetch` the sidecar ran on: HTTP/1.1 over rustls + Graviola, one request per connection at a time.
//!
//! **Silence is not failure.** Encrypted reasoning can emit nothing during a healthy long thought, so there
//! are no header or body idle timers; only a connection that cannot be established is reported, as undici
//! does with its connect timeout. A cancelled request closes its socket, which is what tells the upstream
//! to stop generating.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::error::{AiError, ApiCallError};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// undici's connect timeout, the one timer the sidecar kept.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// undici's default keep-alive for an idle connection.
const KEEP_ALIVE: Duration = Duration::from_secs(4);

// ---------------------------------------------------------------- Cancellation

/// The step's `AbortController`.
#[derive(Clone)]
pub struct Cancel {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl Default for Cancel {
    fn default() -> Self {
        let (tx, rx) = watch::channel(false);
        Self { tx: Arc::new(tx), rx }
    }
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        let _ = rx.wait_for(|fired| *fired).await;
    }
}

// ---------------------------------------------------------------- Request and response

/// Header names lowercased, as `new Headers()` leaves them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Headers(pub Vec<(String, String)>);

impl Headers {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn from_pairs<K: AsRef<str>, V: AsRef<str>>(pairs: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut out = Self::new();
        for (k, v) in pairs {
            out.set(k.as_ref(), v.as_ref());
        }
        out
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.0.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }

    pub fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn set(&mut self, name: &str, value: &str) {
        let name = name.to_ascii_lowercase();
        if let Some(slot) = self.0.iter_mut().find(|(k, _)| *k == name) {
            slot.1 = value.to_owned();
        } else {
            self.0.push((name, value.to_owned()));
        }
    }

    pub fn delete(&mut self, name: &str) {
        let name = name.to_ascii_lowercase();
        self.0.retain(|(k, _)| *k != name);
    }

    pub fn to_vec(&self) -> Vec<(String, String)> {
        self.0.clone()
    }
}

#[derive(Clone)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Headers,
    /// The JSON text, or `None` for a bodiless request. Dialects patch it as text, as they did at the
    /// `fetch` layer.
    pub body: Option<String>,
    /// `new https.Agent({ ca })`: the only roots this request trusts (DER), in place of the bundled ones.
    pub ca_roots: Option<Arc<Vec<Vec<u8>>>>,
    pub cancel: Cancel,
}

impl HttpRequest {
    pub fn post(url: impl Into<String>, headers: Headers, body: String, cancel: Cancel) -> Self {
        Self { method: "POST", url: url.into(), headers, body: Some(body), ca_roots: None, cancel }
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Headers,
    pub body: Body,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// A response rebuilt from text, as `new Response(text, {...})` does in the dialect wrappers.
    pub fn with_text(status: u16, status_text: &str, headers: Headers, text: impl Into<String>) -> Self {
        Self { status, status_text: status_text.to_owned(), headers, body: Body::text(text.into()) }
    }
}

/// A response body: from the network, already in memory, or passing through a line rewriter.
pub struct Body {
    inner: BodyInner,
}

enum BodyInner {
    Empty,
    Buffered(Option<Bytes>),
    Net(NetBody),
    Mapped(Box<dyn ChunkMap>),
}

/// A transform over another body: the SSE line rewriter (`sse.rs`), gzip.
pub trait ChunkMap: Send {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, AiError>>;
}

impl Body {
    pub fn empty() -> Self {
        Self { inner: BodyInner::Empty }
    }

    pub fn text(text: String) -> Self {
        Self { inner: BodyInner::Buffered(Some(Bytes::from(text))) }
    }

    pub fn bytes(bytes: Bytes) -> Self {
        Self { inner: BodyInner::Buffered(Some(bytes)) }
    }

    pub fn mapped(map: Box<dyn ChunkMap>) -> Self {
        Self { inner: BodyInner::Mapped(map) }
    }

    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, AiError> {
        match &mut self.inner {
            BodyInner::Empty => Ok(None),
            BodyInner::Buffered(bytes) => Ok(bytes.take().filter(|b| !b.is_empty())),
            BodyInner::Net(net) => net.next_chunk().await,
            BodyInner::Mapped(map) => map.next_chunk().await,
        }
    }

    pub async fn read_all(&mut self) -> Result<Vec<u8>, AiError> {
        let mut out = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// `response.text()`.
    pub async fn read_text(&mut self) -> Result<String, AiError> {
        let bytes = self.read_all().await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// The `fetch` signature every dialect wraps.
pub trait Fetch: Send + Sync {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>>;
}

pub type DynFetch = Arc<dyn Fetch>;

// ---------------------------------------------------------------- The network

enum Io {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Io {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_flush(cx),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

type ReqBody = http_body_util::Either<Full<Bytes>, Empty<Bytes>>;

struct Conn {
    sender: http1::SendRequest<ReqBody>,
    task: JoinHandle<()>,
}

struct Idle {
    conn: Conn,
    since: Instant,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Origin {
    tls: bool,
    host: String,
    port: u16,
    /// A connection is only ever reused under the trust it was made with.
    ca_roots: Option<Arc<Vec<Vec<u8>>>>,
}

fn pool() -> &'static Mutex<HashMap<Origin, Vec<Idle>>> {
    static POOL: OnceLock<Mutex<HashMap<Origin, Vec<Idle>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A root trusted on top of the bundled ones, for tests of the TLS path against a local server.
static TEST_ROOT: OnceLock<Vec<u8>> = OnceLock::new();

/// Trusts one more root certificate (DER) in this process. Only tests call this: production trust is the
/// same Mozilla set Node bundles. Must run before the first TLS connection.
#[doc(hidden)]
pub fn trust_root_for_tests(der: Vec<u8>) {
    let _ = TEST_ROOT.set(der);
}

fn tls_trusting(roots: rustls::RootCertStore) -> Result<TlsConnector, String> {
    let mut config = rustls::ClientConfig::builder_with_provider(rustls_graviola::default_provider().into())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsConnector::from(Arc::new(config)))
}

fn tls() -> Result<TlsConnector, AiError> {
    static TLS: OnceLock<Result<TlsConnector, String>> = OnceLock::new();
    TLS.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(der) = TEST_ROOT.get() {
            roots.add(der.clone().into()).map_err(|e| e.to_string())?;
        }
        tls_trusting(roots)
    })
    .clone()
    .map_err(AiError::Other)
}

/// The connector for a request's own roots: rare enough (a token refresh) to build each time.
fn tls_for(ca_roots: &[Vec<u8>]) -> Result<TlsConnector, String> {
    let mut roots = rustls::RootCertStore::empty();
    for der in ca_roots {
        roots.add(der.clone().into()).map_err(|e| e.to_string())?;
    }
    tls_trusting(roots)
}

/// A URL split into what a connection and a request line need.
struct Target {
    origin: Origin,
    authority: String,
    path_and_query: String,
}

fn parse_url(url: &str) -> Result<Target, String> {
    let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(format!("Failed to parse URL from {url}"));
    };
    let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(split);
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']').ok_or_else(|| format!("Failed to parse URL from {url}"))?;
        (host.to_owned(), after.strip_prefix(':').and_then(|p| p.parse().ok()))
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host.to_owned(), Some(port.parse().map_err(|_| format!("Failed to parse URL from {url}"))?))
    } else {
        (authority.to_owned(), None)
    };
    if host.is_empty() {
        return Err(format!("Failed to parse URL from {url}"));
    }
    let tail = tail.split('#').next().unwrap_or("");
    let path_and_query = match tail.strip_prefix('?') {
        Some(_) => format!("/{tail}"),
        None if tail.is_empty() => "/".to_owned(),
        None => tail.to_owned(),
    };
    let default_port = if tls { 443 } else { 80 };
    let port = port.unwrap_or(default_port);
    // As `new URL(...).host` writes it: a default port does not appear.
    let authority = if port == default_port {
        authority.rsplit_once(':').filter(|(_, tail)| !tail.contains(']')).map_or(authority, |(host, _)| host)
    } else {
        authority
    };
    Ok(Target { origin: Origin { tls, host, port, ca_roots: None }, authority: authority.to_owned(), path_and_query })
}

/// undici's `autoSelectFamilyAttemptTimeout`: how long one address gets before the next one starts too.
const ADDRESS_ATTEMPT: Duration = Duration::from_millis(250);

/// Connects to the first address that answers, the next one starting whenever the last has had its turn. A
/// host whose IPv6 route silently drops packets must not cost the whole connect timeout.
async fn connect_any(origin: &Origin) -> Result<TcpStream, std::io::Error> {
    let addresses: Vec<std::net::SocketAddr> = tokio::net::lookup_host((origin.host.as_str(), origin.port)).await?.collect();
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no address");
    let mut attempts = tokio::task::JoinSet::new();
    let mut remaining = addresses.into_iter();
    loop {
        let more = match remaining.next() {
            Some(address) => {
                attempts.spawn(TcpStream::connect(address));
                true
            }
            None => false,
        };
        if attempts.is_empty() {
            return Err(last);
        }
        let joined = if more {
            match tokio::time::timeout(ADDRESS_ATTEMPT, attempts.join_next()).await {
                Ok(joined) => joined,
                Err(_) => continue,
            }
        } else {
            attempts.join_next().await
        };
        match joined {
            Some(Ok(Ok(stream))) => return Ok(stream),
            Some(Ok(Err(error))) => last = error,
            Some(Err(error)) => last = std::io::Error::other(error),
            None => {}
        }
    }
}

async fn dial(origin: &Origin) -> Result<Io, std::io::Error> {
    let tcp = connect_any(origin).await?;
    // A token is a few hundred bytes; Nagle would hold it for an ACK.
    tcp.set_nodelay(true)?;
    if !origin.tls {
        return Ok(Io::Plain(tcp));
    }
    let connector = match &origin.ca_roots {
        Some(ca_roots) => tls_for(ca_roots).map_err(std::io::Error::other)?,
        None => tls().map_err(|e| std::io::Error::other(e.message()))?,
    };
    let name = rustls::pki_types::ServerName::try_from(origin.host.clone())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    Ok(Io::Tls(Box::new(connector.connect(name, tcp).await?)))
}

/// How undici words a transport failure, by what happened.
fn undici_code(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::*;
    match error.kind() {
        ConnectionRefused => "ECONNREFUSED",
        ConnectionReset | ConnectionAborted => "ECONNRESET",
        BrokenPipe => "EPIPE",
        TimedOut => "ETIMEDOUT",
        NotFound | HostUnreachable | NetworkUnreachable => "ENOTFOUND",
        _ => {
            let text = error.to_string().to_ascii_lowercase();
            if text.contains("lookup") || text.contains("resolve") || text.contains("nodename") {
                "ENOTFOUND"
            } else {
                "UND_ERR_SOCKET"
            }
        }
    }
}

fn connect_failure(url: &str, detail: String) -> AiError {
    // `handleFetchError`: a connection that could not be made is a retryable API call failure.
    AiError::api(ApiCallError {
        message: format!("Cannot connect to API: {detail}"),
        url: url.to_owned(),
        status: None,
        response_headers: Vec::new(),
        response_body: None,
        is_retryable: true,
        data: None,
    })
}

/// What `fetch` itself adds to every request.
fn with_fetch_defaults(headers: &Headers) -> Headers {
    let mut out = headers.clone();
    // `Headers` iterates sorted, and undici writes what it iterates.
    out.0.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in [
        ("connection", "keep-alive"),
        ("accept", "*/*"),
        ("accept-language", "*"),
        ("sec-fetch-mode", "cors"),
        ("accept-encoding", "gzip, deflate"),
    ] {
        if !out.has(name) {
            out.set(name, value);
        }
    }
    out
}

async fn checkout(origin: &Origin) -> Option<Conn> {
    loop {
        let idle = {
            let mut pool = pool().lock().unwrap_or_else(|p| p.into_inner());
            pool.get_mut(origin)?.pop()
        }?;
        if idle.since.elapsed() >= KEEP_ALIVE || idle.conn.sender.is_closed() || idle.conn.task.is_finished() {
            idle.conn.task.abort();
            continue;
        }
        return Some(idle.conn);
    }
}

async fn open(origin: &Origin, url: &str, cancel: &Cancel) -> Result<Conn, AiError> {
    let connect = async {
        let io = dial(origin).await?;
        let (sender, conn) = http1::Builder::new()
            .preserve_header_case(true)
            .handshake::<_, ReqBody>(TokioIo::new(io))
            .await
            .map_err(std::io::Error::other)?;
        let task = tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok::<_, std::io::Error>(Conn { sender, task })
    };
    tokio::select! {
        _ = cancel.cancelled() => Err(AiError::Abort),
        done = tokio::time::timeout(CONNECT_TIMEOUT, connect) => match done {
            Err(_) => Err(connect_failure(url, "Connect Timeout Error".to_owned())),
            Ok(Err(error)) => {
                let code = undici_code(&error);
                Err(connect_failure(url, format!("{code}: {error}")))
            }
            Ok(Ok(conn)) => Ok(conn),
        },
    }
}

/// The body of a response, holding the connection it arrives on.
struct NetBody {
    body: Incoming,
    conn: Option<Conn>,
    origin: Origin,
    reusable: bool,
    cancel: Cancel,
}

impl NetBody {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, AiError> {
        loop {
            let frame = tokio::select! {
                _ = self.cancel.cancelled() => {
                    self.abort();
                    return Err(AiError::Abort);
                }
                frame = self.body.frame() => frame,
            };
            match frame {
                None => {
                    self.release();
                    return Ok(None);
                }
                Some(Err(_)) => {
                    self.abort();
                    // `TypeError: terminated` with `cause.code`, which is what makes it retryable. The host sees
                    // this message, so the transport's own wording stays out of it, as `error.message` has none.
                    return Err(AiError::Network { code: "UND_ERR_SOCKET", message: "terminated".to_owned() });
                }
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        if !data.is_empty() {
                            return Ok(Some(data));
                        }
                    }
                }
            }
        }
    }

    fn abort(&mut self) {
        if let Some(conn) = self.conn.take() {
            conn.task.abort();
        }
    }

    fn release(&mut self) {
        let Some(conn) = self.conn.take() else { return };
        if !self.reusable || conn.task.is_finished() {
            conn.task.abort();
            return;
        }
        let mut pool = pool().lock().unwrap_or_else(|p| p.into_inner());
        let idle = pool.entry(self.origin.clone()).or_default();
        idle.retain(|entry| entry.since.elapsed() < KEEP_ALIVE);
        idle.push(Idle { conn, since: Instant::now() });
    }
}

impl Drop for NetBody {
    fn drop(&mut self) {
        // An abandoned stream closes its socket at once: that is what tells the upstream to stop.
        self.abort();
    }
}

/// The real network.
pub struct NetFetch;

impl NetFetch {
    pub fn shared() -> DynFetch {
        Arc::new(NetFetch)
    }

    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, AiError> {
        if request.cancel.is_cancelled() {
            return Err(AiError::Abort);
        }
        let mut target = parse_url(&request.url).map_err(AiError::Other)?;
        target.origin.ca_roots = request.ca_roots.clone();
        let headers = with_fetch_defaults(&request.headers);
        let body = request.body.clone().map(Bytes::from);

        // A pooled connection may have died while idle; only then is a resend safe, because the server
        // never read the request. A fresh connection reports its failure as it is.
        let mut pooled = checkout(&target.origin).await;
        loop {
            let reused = pooled.is_some();
            let mut conn = match pooled.take() {
                Some(conn) => conn,
                None => open(&target.origin, &request.url, &request.cancel).await?,
            };
            if reused && conn.sender.ready().await.is_err() {
                conn.task.abort();
                continue;
            }
            let mut builder = http::Request::builder().method(request.method).uri(target.path_and_query.as_str());
            builder = builder.header(http::header::HOST, target.authority.as_str());
            for (name, value) in &headers.0 {
                if name == "host" || name == "content-length" {
                    continue;
                }
                builder = builder.header(name.as_str(), value.as_str());
            }
            let wire_body: ReqBody = match &body {
                Some(bytes) => {
                    builder = builder.header(http::header::CONTENT_LENGTH, bytes.len());
                    http_body_util::Either::Left(Full::new(bytes.clone()))
                }
                None => http_body_util::Either::Right(Empty::new()),
            };
            let wire = builder.body(wire_body).map_err(|e| {
                // `new Headers()` refuses an invalid name or value before anything is sent.
                AiError::Other(format!("Headers.append: {e}"))
            })?;
            let sent = tokio::select! {
                _ = request.cancel.cancelled() => {
                    conn.task.abort();
                    return Err(AiError::Abort);
                }
                sent = conn.sender.send_request(wire) => sent,
            };
            let response = match sent {
                Ok(response) => response,
                Err(error) if reused && (error.is_closed() || error.is_canceled() || error.is_incomplete_message()) => {
                    conn.task.abort();
                    continue;
                }
                Err(error) => {
                    conn.task.abort();
                    return Err(connect_failure(&request.url, format!("UND_ERR_SOCKET: {error}")));
                }
            };
            let status = response.status();
            // `response.statusText` is the wire's phrase; hyper only keeps one that is not the canonical one.
            let status_text = match response.extensions().get::<hyper::ext::ReasonPhrase>() {
                Some(reason) => String::from_utf8_lossy(reason.as_bytes()).into_owned(),
                None => status.canonical_reason().unwrap_or("").to_owned(),
            };
            let mut response_headers = Headers::new();
            for (name, value) in response.headers() {
                let value = String::from_utf8_lossy(value.as_bytes());
                // Repeated headers fold as `Headers` folds them.
                match response_headers.get(name.as_str()).map(str::to_owned) {
                    Some(existing) => response_headers.set(name.as_str(), &format!("{existing}, {value}")),
                    None => response_headers.set(name.as_str(), &value),
                }
            }
            let reusable = response_headers.get("connection").is_none_or(|v| !v.eq_ignore_ascii_case("close"));
            let encoding = response_headers.get("content-encoding").map(|v| v.trim().to_ascii_lowercase());
            let net = NetBody {
                body: response.into_body(),
                conn: Some(conn),
                origin: target.origin.clone(),
                reusable,
                cancel: request.cancel.clone(),
            };
            let mut body = Body { inner: BodyInner::Net(net) };
            match encoding.as_deref() {
                Some("gzip") | Some("x-gzip") => {
                    response_headers.delete("content-encoding");
                    response_headers.delete("content-length");
                    body = Body::mapped(Box::new(Inflate::new(body, true)));
                }
                Some("deflate") => {
                    response_headers.delete("content-encoding");
                    response_headers.delete("content-length");
                    body = Body::mapped(Box::new(Inflate::new(body, false)));
                }
                _ => {}
            }
            return Ok(HttpResponse { status: status.as_u16(), status_text, headers: response_headers, body });
        }
    }
}

impl Fetch for NetFetch {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.send(request))
    }
}

// ---------------------------------------------------------------- Content decoding

/// Streaming gzip or zlib decode, keeping a server that compresses SSE readable as it arrives.
struct Inflate {
    inner: Body,
    state: flate2::Decompress,
    header: Vec<u8>,
    gzip: bool,
    done: bool,
}

impl Inflate {
    fn new(inner: Body, gzip: bool) -> Self {
        // Gzip's container is parsed here; flate2's low-level API takes the raw DEFLATE stream inside.
        Self { inner, state: flate2::Decompress::new(!gzip), header: Vec::new(), gzip, done: false }
    }

    /// Bytes of the gzip header in `bytes`, once complete. `None` while more input is needed.
    fn gzip_header_len(bytes: &[u8]) -> Option<Result<usize, AiError>> {
        if bytes.len() < 10 {
            return None;
        }
        if bytes[0] != 0x1f || bytes[1] != 0x8b {
            return Some(Err(AiError::Network { code: "UND_ERR_SOCKET", message: "terminated: incorrect gzip header".into() }));
        }
        let flags = bytes[3];
        let mut at = 10;
        if flags & 0x04 != 0 {
            let extra = u16::from_le_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]) as usize;
            at += 2 + extra;
        }
        for flag in [0x08, 0x10] {
            if flags & flag != 0 {
                at += bytes.get(at..)?.iter().position(|b| *b == 0)? + 1;
            }
        }
        if flags & 0x02 != 0 {
            at += 2;
        }
        (bytes.len() >= at).then_some(Ok(at))
    }

    async fn pull(&mut self) -> Result<Option<Bytes>, AiError> {
        loop {
            if self.done {
                return Ok(None);
            }
            let Some(chunk) = self.inner.next_chunk().await? else {
                self.done = true;
                return Ok(None);
            };
            let mut input: &[u8] = &chunk;
            let joined;
            if self.gzip {
                self.header.extend_from_slice(input);
                match Self::gzip_header_len(&self.header) {
                    None => continue,
                    Some(Err(error)) => return Err(error),
                    Some(Ok(at)) => {
                        joined = self.header.split_off(at);
                        self.header = Vec::new();
                        self.gzip = false; // container header is behind us
                        input = &joined;
                    }
                }
            }
            let mut out = Vec::with_capacity(input.len() * 4 + 1024);
            while !input.is_empty() {
                out.reserve(8192);
                let before = self.state.total_in();
                let status = self
                    .state
                    .decompress_vec(input, &mut out, flate2::FlushDecompress::None)
                    .map_err(|e| AiError::Network { code: "UND_ERR_SOCKET", message: format!("terminated: {e}") })?;
                input = &input[(self.state.total_in() - before) as usize..];
                if status == flate2::Status::StreamEnd {
                    self.done = true;
                    break;
                }
                if input.is_empty() {
                    break;
                }
            }
            if !out.is_empty() {
                return Ok(Some(Bytes::from(out)));
            }
        }
    }
}

impl ChunkMap for Inflate {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, AiError>> {
        Box::pin(self.pull())
    }
}

/// `new URL(url)` with hash and search handling for the dialects' keys: the URL with its fragment off.
pub fn url_without_hash(url: &str) -> String {
    url.split('#').next().unwrap_or(url).to_owned()
}

/// The URL with fragment and query off.
pub fn url_without_query(url: &str) -> String {
    let no_hash = url.split('#').next().unwrap_or(url);
    no_hash.split('?').next().unwrap_or(no_hash).to_owned()
}

/// The lowercase hostname of a URL, or `None` when it does not parse.
pub fn url_hostname(url: &str) -> Option<String> {
    parse_url(url).ok().map(|t| t.origin.host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_origin_and_request_target() {
        let t = parse_url("https://api.example.com:8443/v1/messages?x=1#frag").unwrap();
        assert_eq!((t.origin.tls, t.origin.host.as_str(), t.origin.port), (true, "api.example.com", 8443));
        assert_eq!(t.path_and_query, "/v1/messages?x=1");
        assert_eq!(t.authority, "api.example.com:8443");
        let t = parse_url("http://[::1]:9/x").unwrap();
        assert_eq!((t.origin.host.as_str(), t.origin.port), ("::1", 9));
        let t = parse_url("http://host").unwrap();
        assert_eq!((t.origin.port, t.path_and_query.as_str()), (80, "/"));
        assert_eq!(parse_url("https://host:443/x").unwrap().authority, "host");
        assert_eq!(parse_url("http://[::1]:80/x").unwrap().authority, "[::1]");
        assert_eq!(parse_url("http://[::1]/x").unwrap().authority, "[::1]");
        assert!(parse_url("ftp://host").is_err());
    }
}
