//! HTTPS transport built for bad networks.
//!
//! Uses hyper at the connection level rather than through a client wrapper, because each decision here
//! needs to be explicit: socket options, when a dead connection is declared dead, how a slow upload is
//! told apart from a stuck one, and hedging a handshake instead of sitting out a 1 s SYN retransmission.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use parking_lot::Mutex;
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpSocket, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use crate::error::{FailKind, NetError};
use crate::wire::Rope;

/// Largest frame handed to hyper, so upload progress stays visible on big media.
const UPLOAD_FRAME: usize = 256 << 10;

#[derive(Clone, Debug)]
pub struct TransportConfig {
    /// Such as `https://host:8443/v1`.
    pub base_url: String,
    /// Extra trust root in PEM, for a private CA.
    pub ca_pem: Option<Vec<u8>>,
    /// Offer h2 in ALPN and multiplex over `h2_conns` connections.
    pub http2: bool,
    pub h2_conns: usize,
    pub connect_timeout: Duration,
    /// Start a second connection if the first has not finished by then. `None` disables hedging.
    pub hedge_after: Option<Duration>,
    pub first_byte_timeout: Duration,
    pub upload_stall_timeout: Duration,
    pub idle_conns: usize,
    /// Sent with every request: API keys and protocol versions.
    pub headers: Vec<(http::HeaderName, http::HeaderValue)>,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            ca_pem: None,
            http2: false,
            h2_conns: 1,
            connect_timeout: Duration::from_secs(20),
            // The first SYN or handshake flight retransmits after 1 s, so waiting a little past that is
            // the point where a parallel attempt starts to win.
            hedge_after: Some(Duration::from_millis(1200)),
            first_byte_timeout: Duration::from_secs(240),
            upload_stall_timeout: Duration::from_secs(25),
            idle_conns: 256,
            headers: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct TransportStats {
    pub dials: AtomicU64,
    pub hedges: AtomicU64,
    pub hedge_wins: AtomicU64,
    pub tls_full: AtomicU64,
    pub tls_resumed: AtomicU64,
    pub reused: AtomicU64,
    pub stale_retries: AtomicU64,
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ConnTiming {
    pub reused: bool,
    pub tcp_ms: f32,
    pub tls_ms: f32,
    pub h2: bool,
}

enum Io {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Io {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }
    fn is_write_vectored(&self) -> bool {
        match self {
            Io::Plain(s) => s.is_write_vectored(),
            Io::Tls(s) => s.is_write_vectored(),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_flush(cx),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Request body over a rope, counting what has been handed to the connection.
pub struct RopeBody {
    parts: std::vec::IntoIter<Bytes>,
    current: Bytes,
    remaining: u64,
    progress: Arc<AtomicU64>,
}

impl RopeBody {
    fn new(rope: Rope, progress: Arc<AtomicU64>) -> Self {
        Self { remaining: rope.len, parts: rope.parts.into_iter(), current: Bytes::new(), progress }
    }
}

impl Body for RopeBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        loop {
            if !self.current.is_empty() {
                let n = self.current.len().min(UPLOAD_FRAME);
                let frame = self.current.split_to(n);
                self.remaining -= n as u64;
                self.progress.fetch_add(n as u64, Ordering::Relaxed);
                return Poll::Ready(Some(Ok(Frame::data(frame))));
            }
            match self.parts.next() {
                Some(next) => self.current = next,
                None => return Poll::Ready(None),
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

struct H1Conn {
    sender: http1::SendRequest<RopeBody>,
    task: JoinHandle<()>,
}

#[derive(Clone)]
struct H2Conn {
    sender: http2::SendRequest<RopeBody>,
    created: Instant,
    /// Ends the connection task when the last clone goes.
    _task: Arc<AbortOnDrop>,
}

#[derive(Default)]
struct H2Slot {
    conn: Option<H2Conn>,
    failed: Option<(Instant, NetError)>,
}

enum H2Failure {
    /// The server does not speak h2, so the caller should go on with HTTP/1.1.
    NotOffered,
    Net(NetError),
}

struct AbortOnDrop(JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct Transport {
    cfg: TransportConfig,
    tls: Option<TlsConnector>,
    server_name: Option<ServerName<'static>>,
    host: String,
    port: u16,
    authority: String,
    scheme: &'static str,
    base_path: String,
    addrs: Mutex<Option<Arc<[SocketAddr]>>>,
    idle: Mutex<Vec<H1Conn>>,
    h2: Vec<tokio::sync::Mutex<H2Slot>>,
    h2_next: AtomicUsize,
    /// ALPN said the server does not do h2, so this transport speaks HTTP/1.1 after all.
    h2_refused: AtomicBool,
    pub stats: TransportStats,
}

/// A response whose headers have arrived. Dropping it tears the connection down.
pub struct OpenStream {
    pub status: StatusCode,
    pub timing: ConnTiming,
    pub ttfb: Duration,
    body: Incoming,
    /// The h1 connection, kept so that it can be aborted now or pooled later.
    h1: Option<H1Conn>,
    keep_alive: bool,
    /// Holds the h2 connection alive for at least as long as this stream.
    _h2: Option<H2Conn>,
    transport: Arc<Transport>,
}

impl OpenStream {
    /// Next body chunk, or `Ok(None)` on a clean end. A silent connection fails after `stall`.
    pub async fn next_chunk(&mut self, stall: Duration) -> Result<Option<Bytes>, NetError> {
        loop {
            let frame = match tokio::time::timeout(stall, self.body.frame()).await {
                Err(_) => return Err(NetError::new(FailKind::Stall, format!("no data for {stall:?}"))),
                Ok(None) => return Ok(None),
                Ok(Some(Err(e))) => return Err(classify_hyper(&e)),
                Ok(Some(Ok(frame))) => frame,
            };
            if let Ok(data) = frame.into_data()
                && !data.is_empty() {
                    self.transport.stats.bytes_down.fetch_add(data.len() as u64, Ordering::Relaxed);
                    return Ok(Some(data));
                }
        }
    }

    /// Reads everything left. For error bodies and small JSON responses.
    pub async fn read_all(&mut self, limit: usize, stall: Duration) -> Result<Vec<u8>, NetError> {
        let mut out = Vec::new();
        while let Some(chunk) = self.next_chunk(stall).await? {
            if out.len() < limit {
                out.extend_from_slice(&chunk[..chunk.len().min(limit - out.len())]);
            }
        }
        Ok(out)
    }

    /// Returns a cleanly finished connection to the pool. On HTTP/1.1 a connection only becomes reusable
    /// once the terminating chunk has been read, and `[DONE]` arrives just before it.
    pub async fn finish(mut self) {
        let body = &mut self.body;
        // A single deadline for the lot, or a server sending comments forever would hold the slot forever.
        let drained = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match body.frame().await {
                    None => return true,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
        let Some(mut conn) = self.h1.take() else { return };
        let ready = drained
            && self.keep_alive
            && matches!(tokio::time::timeout(Duration::from_millis(250), conn.sender.ready()).await, Ok(Ok(())));
        if ready {
            let mut idle = self.transport.idle.lock();
            if idle.len() < self.transport.cfg.idle_conns {
                idle.push(conn);
                return;
            }
        }
        conn.task.abort();
    }
}

impl Drop for OpenStream {
    fn drop(&mut self) {
        // An abandoned stream must close its socket right away: that is what tells the gateway, and then
        // llama-server, to stop generating and give the slot back.
        if let Some(conn) = self.h1.take() {
            conn.task.abort();
        }
    }
}

/// Requests are small, so what they send is a thin stream as well: too few packets in flight for fast retransmit,
/// which leaves each loss to a retransmission timer that doubles every time. This keeps it linear for a while.
#[cfg(target_os = "linux")]
fn thin_stream_timeouts(tcp: &TcpStream) {
    use std::os::fd::AsRawFd;
    const TCP_THIN_LINEAR_TIMEOUTS: libc::c_int = 16;
    let on: libc::c_int = 1;
    // SAFETY: the fd is live for as long as `tcp` is, and the pointer and length describe `on`.
    unsafe {
        libc::setsockopt(tcp.as_raw_fd(), libc::IPPROTO_TCP, TCP_THIN_LINEAR_TIMEOUTS, (&on as *const libc::c_int).cast(), size_of::<libc::c_int>() as libc::socklen_t);
    }
}

#[cfg(not(target_os = "linux"))]
fn thin_stream_timeouts(_: &TcpStream) {}

fn classify_hyper(e: &hyper::Error) -> NetError {
    let kind = if e.is_incomplete_message() || e.is_closed() || e.is_canceled() {
        FailKind::Reset
    } else if e.is_timeout() {
        FailKind::Stall
    } else if e.is_parse() {
        FailKind::Protocol
    } else {
        FailKind::Reset
    };
    let mut detail = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        detail.push_str(": ");
        detail.push_str(&s.to_string());
        source = s.source();
    }
    NetError::new(kind, detail)
}

fn parse_base_url(url: &str) -> Result<(bool, String, u16, String), String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("{url}: only http and https URLs are supported"));
    };
    let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, String::new()));
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (h, tail) = bracketed.split_once(']').ok_or_else(|| format!("{url}: unterminated IPv6 address"))?;
        (h.to_owned(), tail.strip_prefix(':').map(|p| p.parse()).transpose().map_err(|_| format!("{url}: bad port"))?)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        (h.to_owned(), Some(p.parse().map_err(|_| format!("{url}: bad port"))?))
    } else {
        (authority.to_owned(), None)
    };
    if host.is_empty() {
        return Err(format!("{url}: missing host"));
    }
    Ok((tls, host, port.unwrap_or(if tls { 443 } else { 80 }), path.trim_end_matches('/').to_owned()))
}

fn tls_config(cfg: &TransportConfig) -> Result<ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = &cfg.ca_pem {
        let text = std::str::from_utf8(pem).map_err(|_| "CA file is not PEM text".to_owned())?;
        let mut added = 0;
        for der in pem_certs(text)? {
            roots.add(der.into()).map_err(|e| format!("CA certificate: {e}"))?;
            added += 1;
        }
        if added == 0 {
            return Err("CA file holds no certificate".into());
        }
    }
    let mut config = ClientConfig::builder_with_provider(rustls_graviola::default_provider().into())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = if cfg.http2 { vec![b"h2".to_vec(), b"http/1.1".to_vec()] } else { vec![b"http/1.1".to_vec()] };
    Ok(config)
}

fn pem_certs(text: &str) -> Result<Vec<Vec<u8>>, String> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let after = &rest[start + BEGIN.len()..];
        let stop = after.find(END).ok_or("unterminated PEM certificate")?;
        let b64: String = after[..stop].chars().filter(|c| !c.is_whitespace()).collect();
        out.push(base64_simd::STANDARD.decode_to_vec(b64.as_bytes()).map_err(|e| format!("PEM: {e}"))?);
        rest = &after[stop + END.len()..];
    }
    Ok(out)
}

impl Transport {
    pub fn new(cfg: TransportConfig) -> Result<Arc<Self>, String> {
        let (is_tls, host, port, base_path) = parse_base_url(&cfg.base_url)?;
        let (tls, server_name) = if is_tls {
            let config = tls_config(&cfg)?;
            let name = ServerName::try_from(host.clone()).map_err(|e| format!("{host}: {e}"))?;
            (Some(TlsConnector::from(Arc::new(config))), Some(name))
        } else {
            (None, None)
        };
        let default_port = if is_tls { 443 } else { 80 };
        let host_in_url = if host.contains(':') { format!("[{host}]") } else { host.clone() };
        let authority = if port == default_port { host_in_url } else { format!("{host_in_url}:{port}") };
        let h2_slots = if cfg.http2 { cfg.h2_conns.max(1) } else { 0 };
        Ok(Arc::new(Self {
            scheme: if is_tls { "https" } else { "http" },
            tls,
            server_name,
            host,
            port,
            authority,
            base_path,
            addrs: Mutex::new(None),
            idle: Mutex::new(Vec::new()),
            h2: (0..h2_slots).map(|_| tokio::sync::Mutex::new(H2Slot::default())).collect(),
            h2_next: AtomicUsize::new(0),
            h2_refused: AtomicBool::new(false),
            stats: TransportStats::default(),
            cfg,
        }))
    }

    pub fn config(&self) -> &TransportConfig {
        &self.cfg
    }

    async fn resolve(&self) -> io::Result<Arc<[SocketAddr]>> {
        if let Some(addrs) = self.addrs.lock().clone() {
            return Ok(addrs);
        }
        let addrs: Arc<[SocketAddr]> = tokio::net::lookup_host((self.host.as_str(), self.port)).await?.collect::<Vec<_>>().into();
        if addrs.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no address"));
        }
        *self.addrs.lock() = Some(addrs.clone());
        Ok(addrs)
    }

    async fn connect_any(&self, first: usize) -> Result<TcpStream, NetError> {
        let addrs = self.resolve().await.map_err(|e| NetError::new(FailKind::Connect, format!("resolve {}: {e}", self.host)))?;
        let mut last = None;
        // `localhost` often means two addresses and a server listening on only one of them.
        for i in 0..addrs.len() {
            let addr = addrs[(first + i) % addrs.len()];
            let socket = if addr.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }.map_err(|e| NetError::new(FailKind::Connect, e.to_string()))?;
            match socket.connect(addr).await {
                Ok(tcp) => return Ok(tcp),
                Err(e) => last = Some(NetError::new(FailKind::Connect, format!("{addr}: {e}"))),
            }
        }
        Err(last.unwrap_or_else(|| NetError::new(FailKind::Connect, "no address")))
    }

    async fn dial(&self, first_address: usize) -> Result<(Io, ConnTiming), NetError> {
        self.stats.dials.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let tcp = self.connect_any(first_address).await?;
        // A token is a few hundred bytes. Nagle would hold it for an ACK that a lossy link delays for ages.
        tcp.set_nodelay(true).map_err(|e| NetError::new(FailKind::Connect, e.to_string()))?;
        thin_stream_timeouts(&tcp);
        let tcp_ms = started.elapsed().as_secs_f32() * 1000.0;
        let Some(tls) = &self.tls else {
            return Ok((Io::Plain(tcp), ConnTiming { tcp_ms, ..Default::default() }));
        };
        let tls_started = Instant::now();
        let name = self.server_name.clone().expect("https implies a server name");
        let stream = tls.connect(name, tcp).await.map_err(|e| NetError::new(FailKind::Tls, e.to_string()))?;
        let conn = stream.get_ref().1;
        let counter = match conn.handshake_kind() {
            Some(rustls::HandshakeKind::Resumed) => &self.stats.tls_resumed,
            _ => &self.stats.tls_full,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let h2 = conn.alpn_protocol() == Some(b"h2".as_slice());
        let tls_ms = tls_started.elapsed().as_secs_f32() * 1000.0;
        Ok((Io::Tls(Box::new(stream)), ConnTiming { reused: false, tcp_ms, tls_ms, h2 }))
    }

    /// Dials with a parallel backup attempt once the first looks lost.
    async fn dial_hedged(&self) -> Result<(Io, ConnTiming), NetError> {
        let work = async {
            let Some(hedge_after) = self.cfg.hedge_after else {
                return self.dial(0).await;
            };
            let first = self.dial(0);
            tokio::pin!(first);
            tokio::select! {
                r = &mut first => return r,
                _ = tokio::time::sleep(hedge_after) => {}
            }
            self.stats.hedges.fetch_add(1, Ordering::Relaxed);
            // The backup starts from the next address, in case the first one is the problem.
            let second = self.dial(1);
            tokio::pin!(second);
            tokio::select! {
                r = &mut first => match r {
                    Ok(ok) => Ok(ok),
                    Err(_) => second.await,
                },
                r = &mut second => match r {
                    Ok(ok) => {
                        self.stats.hedge_wins.fetch_add(1, Ordering::Relaxed);
                        Ok(ok)
                    }
                    Err(_) => first.await,
                },
            }
        };
        match tokio::time::timeout(self.cfg.connect_timeout, work).await {
            Ok(r) => r,
            Err(_) => Err(NetError::new(FailKind::ConnectTimeout, format!("no connection after {:?}", self.cfg.connect_timeout))),
        }
    }

    fn pop_idle(&self) -> Option<H1Conn> {
        let mut idle = self.idle.lock();
        while let Some(conn) = idle.pop() {
            if conn.sender.is_ready() && !conn.sender.is_closed() {
                return Some(conn);
            }
            conn.task.abort();
        }
        None
    }

    fn request(&self, method: http::Method, path: &str, body: RopeBody, h2: bool) -> Request<RopeBody> {
        let mut req = Request::builder()
            .method(method)
            .header(http::header::ACCEPT, "text/event-stream")
            .header(http::header::CONTENT_TYPE, "application/json");
        for (name, value) in &self.cfg.headers {
            req = req.header(name, value);
        }
        if let Some(len) = body.size_hint().exact().filter(|n| *n > 0) {
            req = req.header(http::header::CONTENT_LENGTH, len);
        }
        let full_path = format!("{}{}", self.base_path, path);
        req = if h2 {
            req.uri(format!("{}://{}{}", self.scheme, self.authority, full_path))
        } else {
            req.uri(full_path).header(http::header::HOST, &self.authority)
        };
        req.body(body).expect("well-formed request")
    }

    /// Sends the request and waits for response headers, watching upload progress the whole time. `header_limit`
    /// is how long to wait for them once the body is handed over. Handing over only means the kernel has it: a
    /// connection deep in exponential backoff accepts the write and then goes quiet for minutes, which took 251 s
    /// to come back on the test network, while a new connection takes a fraction of a second.
    ///
    /// `retry_after` is when the previous attempt failed, if there was one. Connections older than that have been
    /// through the same network that just failed, and waiting out their timeout on top of the failure helps nobody.
    pub async fn post_stream(self: &Arc<Self>, path: &str, rope: Rope, header_limit: Duration, retry_after: Option<Instant>) -> Result<OpenStream, NetError> {
        self.send(http::Method::POST, path, rope, header_limit, retry_after).await
    }

    pub async fn get(self: &Arc<Self>, path: &str) -> Result<OpenStream, NetError> {
        self.send(http::Method::GET, path, Rope::default(), self.cfg.first_byte_timeout, None).await
    }

    async fn send(self: &Arc<Self>, method: http::Method, path: &str, rope: Rope, header_limit: Duration, retry_after: Option<Instant>) -> Result<OpenStream, NetError> {
        self.stats.bytes_up.fetch_add(rope.len, Ordering::Relaxed);
        if self.cfg.http2 && !self.h2_refused.load(Ordering::Relaxed) {
            match self.send_h2(method.clone(), path, rope.clone(), header_limit, retry_after).await {
                Err(H2Failure::NotOffered) => {}
                Err(H2Failure::Net(e)) => return Err(e),
                Ok(stream) => return Ok(stream),
            }
        }
        let fresh = retry_after.is_some();
        let started = Instant::now();
        // A pooled connection may have died while idle. That failure says nothing about the request, so
        // it is retried on a fresh connection without using up an attempt.
        if let Some(mut conn) = if fresh { None } else { self.pop_idle() } {
            self.stats.reused.fetch_add(1, Ordering::Relaxed);
            let progress = Arc::new(AtomicU64::new(0));
            let req = self.request(method.clone(), path, RopeBody::new(rope.clone(), progress.clone()), false);
            match self.await_response(conn.sender.send_request(req), progress, rope.len, header_limit).await {
                Ok(resp) => {
                    return Ok(self.opened(resp, started, ConnTiming { reused: true, ..Default::default() }, Some(conn), None));
                }
                Err(_) => {
                    // One failure says the pool is suspect. Going through the rest of it, each with its own timeout,
                    // would turn an outage into minutes of waiting.
                    self.stats.stale_retries.fetch_add(1, Ordering::Relaxed);
                    conn.task.abort();
                }
            }
        }

        let (io, timing) = self.dial_hedged().await?;
        let (mut sender, conn) = http1::Builder::new()
            .handshake::<_, RopeBody>(TokioIo::new(io))
            .await
            .map_err(|e| classify_hyper(&e))?;
        let task = tokio::spawn(async move {
            let _ = conn.await;
        });
        sender.ready().await.map_err(|e| {
            task.abort();
            classify_hyper(&e)
        })?;
        let mut conn = H1Conn { sender, task };
        let progress = Arc::new(AtomicU64::new(0));
        let req = self.request(method, path, RopeBody::new(rope.clone(), progress.clone()), false);
        match self.await_response(conn.sender.send_request(req), progress, rope.len, header_limit).await {
            Ok(resp) => Ok(self.opened(resp, started, timing, Some(conn), None)),
            Err(e) => {
                conn.task.abort();
                Err(e)
            }
        }
    }

    async fn send_h2(self: &Arc<Self>, method: http::Method, path: &str, rope: Rope, header_limit: Duration, retry_after: Option<Instant>) -> Result<OpenStream, H2Failure> {
        let started = Instant::now();
        let index = self.h2_next.fetch_add(1, Ordering::Relaxed) % self.h2.len();
        let (conn, timing) = {
            let mut slot = self.h2[index].lock().await;
            let usable = slot.conn.as_ref().filter(|c| !c.sender.is_closed() && retry_after.is_none_or(|t| c.created >= t));
            match usable {
                Some(conn) => {
                    self.stats.reused.fetch_add(1, Ordering::Relaxed);
                    (conn.clone(), ConnTiming { reused: true, h2: true, ..Default::default() })
                }
                None => {
                    // Everyone sharing this slot waited on the lock for the same dial. If it failed in the meantime
                    // they all have their answer, and dialling again one at a time would queue a whole connect timeout
                    // per stream behind each other.
                    if let Some((at, error)) = &slot.failed
                        && *at >= started {
                            return Err(H2Failure::Net(error.clone()));
                        }
                    let (io, mut timing) = match self.dial_hedged().await {
                        Ok(pair) => pair,
                        Err(e) => {
                            slot.failed = Some((Instant::now(), e.clone()));
                            return Err(H2Failure::Net(e));
                        }
                    };
                    if self.tls.is_some() && !timing.h2 {
                        // The server chose HTTP/1.1 from what ALPN offered, so that is what has to be spoken.
                        self.h2_refused.store(true, Ordering::Relaxed);
                        return Err(H2Failure::NotOffered);
                    }
                    timing.h2 = true;
                    let (sender, conn) = http2::Builder::new(TokioExecutor::new())
                        .timer(TokioTimer::new())
                        .adaptive_window(true)
                        .keep_alive_interval(Duration::from_secs(5))
                        .keep_alive_timeout(Duration::from_secs(15))
                        .handshake::<_, RopeBody>(TokioIo::new(io))
                        .await
                        .map_err(|e| H2Failure::Net(classify_hyper(&e)))?;
                    let task = tokio::spawn(async move {
                        let _ = conn.await;
                    });
                    let fresh = H2Conn { sender, created: Instant::now(), _task: Arc::new(AbortOnDrop(task)) };
                    slot.conn = Some(fresh.clone());
                    slot.failed = None;
                    (fresh, timing)
                }
            }
        };
        let mut sender = conn.sender.clone();
        let progress = Arc::new(AtomicU64::new(0));
        let req = self.request(method, path, RopeBody::new(rope.clone(), progress.clone()), true);
        let resp = self.await_response(sender.send_request(req), progress, rope.len, header_limit).await.map_err(H2Failure::Net)?;
        Ok(self.opened(resp, started, timing, None, Some(conn)))
    }

    async fn await_response<F>(&self, fut: F, progress: Arc<AtomicU64>, total: u64, header_limit: Duration) -> Result<Response<Incoming>, NetError>
    where
        F: Future<Output = Result<Response<Incoming>, hyper::Error>>,
    {
        tokio::pin!(fut);
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut seen = 0;
        let mut changed = Instant::now();
        loop {
            tokio::select! {
                r = &mut fut => return r.map_err(|e| classify_hyper(&e)),
                _ = tick.tick() => {
                    let sent = progress.load(Ordering::Relaxed);
                    if sent != seen {
                        seen = sent;
                        changed = Instant::now();
                    }
                    let quiet = changed.elapsed();
                    if sent < total {
                        if quiet > self.cfg.upload_stall_timeout {
                            return Err(NetError::new(FailKind::Upload, format!("upload stuck at {sent}/{total} bytes for {quiet:?}")));
                        }
                    } else if quiet > header_limit {
                        return Err(NetError::new(FailKind::FirstByteTimeout, format!("no response {quiet:?} after the upload finished")));
                    }
                }
            }
        }
    }

    fn opened(
        self: &Arc<Self>,
        resp: Response<Incoming>,
        started: Instant,
        timing: ConnTiming,
        h1: Option<H1Conn>,
        h2: Option<H2Conn>,
    ) -> OpenStream {
        let (parts, body) = resp.into_parts();
        let closes = parts
            .headers
            .get(http::header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("close"));
        OpenStream {
            status: parts.status,
            timing,
            ttfb: started.elapsed(),
            body,
            h1,
            keep_alive: !closes,
            _h2: h2,
            transport: self.clone(),
        }
    }

    /// Establishes `n` connections in advance, so that a burst does not pay for handshakes.
    pub async fn preconnect(self: &Arc<Self>, n: usize) -> usize {
        if self.cfg.http2 {
            return 0; // connections are made on first use and multiplexed from there
        }
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..n {
            let t = self.clone();
            set.spawn(async move {
                let (io, _) = t.dial_hedged().await.ok()?;
                let (mut sender, conn) = http1::Builder::new().handshake::<_, RopeBody>(TokioIo::new(io)).await.ok()?;
                let task = tokio::spawn(async move {
                    let _ = conn.await;
                });
                sender.ready().await.ok()?;
                t.idle.lock().push(H1Conn { sender, task });
                Some(())
            });
        }
        let mut done = 0;
        while let Some(r) = set.join_next().await {
            if matches!(r, Ok(Some(()))) {
                done += 1;
            }
        }
        done
    }

    /// GET returning a JSON document, for `/slots` and similar.
    pub async fn get_json(self: &Arc<Self>, path: &str) -> Result<serde_json::Value, NetError> {
        let mut stream = self.get(path).await?;
        let body = stream.read_all(32 << 20, Duration::from_secs(20)).await?;
        if !stream.status.is_success() {
            return Err(NetError::new(FailKind::Http(stream.status.as_u16()), String::from_utf8_lossy(&body).into_owned()));
        }
        let value = serde_json::from_slice(&body).map_err(|e| NetError::new(FailKind::Protocol, e.to_string()))?;
        stream.finish().await;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        assert_eq!(parse_base_url("https://a.b:8443/v1/").unwrap(), (true, "a.b".into(), 8443, "/v1".into()));
        assert_eq!(parse_base_url("http://a.b/v1").unwrap(), (false, "a.b".into(), 80, "/v1".into()));
        assert_eq!(parse_base_url("https://a.b").unwrap(), (true, "a.b".into(), 443, "".into()));
        assert_eq!(parse_base_url("https://[::1]:9/v1").unwrap(), (true, "::1".into(), 9, "/v1".into()));
        assert!(parse_base_url("ftp://a").is_err());
        assert!(parse_base_url("https://:1/x").is_err());
    }

    #[test]
    fn rope_body_splits_big_parts_and_counts_progress() {
        let progress = Arc::new(AtomicU64::new(0));
        let rope = Rope { parts: vec![Bytes::from(vec![0u8; UPLOAD_FRAME * 2 + 3]), Bytes::from_static(b"xy")], len: (UPLOAD_FRAME * 2 + 5) as u64 };
        let mut body = RopeBody::new(rope, progress.clone());
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut sizes = Vec::new();
        while let Poll::Ready(Some(Ok(frame))) = Pin::new(&mut body).poll_frame(&mut cx) {
            sizes.push(frame.into_data().unwrap().len());
        }
        assert_eq!(sizes, vec![UPLOAD_FRAME, UPLOAD_FRAME, 3, 2]);
        assert_eq!(progress.load(Ordering::Relaxed), (UPLOAD_FRAME * 2 + 5) as u64);
        assert!(body.is_end_stream());
    }
}
