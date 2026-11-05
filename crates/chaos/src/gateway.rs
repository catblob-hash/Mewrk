//! HTTPS gateway in front of a plaintext llama-server, with application-level fault injection.
//!
//! It stands where a CDN or load balancer stands on a real path: it holds the TLS session and client
//! keep-alive, and it opens one upstream connection per request (llama-server answers `Connection: close`).
//! When the client goes away, the upstream connection is dropped at once, which is what frees the slot.

use std::convert::Infallible;
use std::error::Error as StdError;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode, Uri};
use http_body::{Body, Frame, SizeHint};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use parking_lot::Mutex;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

type BoxError = Box<dyn StdError + Send + Sync>;
type OutBody = http_body_util::combinators::UnsyncBoxBody<Bytes, BoxError>;

#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    /// Chance per response chunk, in 1/1000, of resetting the stream the way a dying proxy would.
    pub reset_per_mille: u32,
    /// Chance per response chunk, in 1/1000, of going silent for `stall_ms`.
    pub stall_per_mille: u32,
    pub stall_ms: u64,
    /// Chance per request, in 1/1000, of answering 503 without touching the upstream.
    pub http503_per_mille: u32,
}

#[derive(Clone, Debug, Default)]
pub struct SocketTuning {
    /// TCP congestion control for accepted sockets, such as `bbr`.
    pub congestion: Option<String>,
    /// Keep retransmission timeouts linear on thin streams. A token stream is the textbook thin stream:
    /// too few packets in flight for fast retransmit, so a lost one normally waits out a doubling RTO.
    pub thin_linear_timeouts: bool,
}

#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub upstream: SocketAddr,
    pub faults: Faults,
    pub tuning: SocketTuning,
    pub seed: u64,
}

#[derive(Default)]
pub struct GatewayStats {
    pub accepted: AtomicU64,
    pub tls_failures: AtomicU64,
    pub requests: AtomicU64,
    pub upstream_active: AtomicU64,
    pub upstream_errors: AtomicU64,
    pub client_gone: AtomicU64,
    pub injected_resets: AtomicU64,
    pub injected_stalls: AtomicU64,
    pub injected_503: AtomicU64,
    pub bytes_down: AtomicU64,
}

impl GatewayStats {
    pub fn to_json(&self) -> serde_json::Value {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        serde_json::json!({
            "accepted": g(&self.accepted),
            "tls_failures": g(&self.tls_failures),
            "requests": g(&self.requests),
            "upstream_active": g(&self.upstream_active),
            "upstream_errors": g(&self.upstream_errors),
            "client_gone": g(&self.client_gone),
            "injected_resets": g(&self.injected_resets),
            "injected_stalls": g(&self.injected_stalls),
            "injected_503": g(&self.injected_503),
            "bytes_down": g(&self.bytes_down),
        })
    }
}

struct Shared {
    cfg: GatewayConfig,
    stats: Arc<GatewayStats>,
    rng: Mutex<fastrand::Rng>,
}

impl Shared {
    fn roll(&self, per_mille: u32) -> bool {
        per_mille > 0 && self.rng.lock().u32(0..1000) < per_mille
    }
}

/// Response body from the upstream, which injects faults and owns the upstream connection.
struct ChaosBody {
    inner: Incoming,
    shared: Arc<Shared>,
    upstream_task: JoinHandle<()>,
    pause: Option<Pin<Box<tokio::time::Sleep>>>,
    held: Option<Frame<Bytes>>,
    finished: bool,
}

#[derive(Debug)]
struct InjectedReset;
impl std::fmt::Display for InjectedReset {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("injected stream reset")
    }
}
impl StdError for InjectedReset {}

impl Body for ChaosBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        if let Some(pause) = &mut this.pause {
            if pause.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.pause = None;
        }
        if let Some(frame) = this.held.take() {
            return Poll::Ready(Some(Ok(frame)));
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                this.shared.stats.upstream_errors.fetch_add(1, Ordering::Relaxed);
                Poll::Ready(Some(Err(Box::new(e))))
            }
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.shared.stats.bytes_down.fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                let faults = this.shared.cfg.faults;
                if this.shared.roll(faults.reset_per_mille) {
                    this.shared.stats.injected_resets.fetch_add(1, Ordering::Relaxed);
                    return Poll::Ready(Some(Err(Box::new(InjectedReset))));
                }
                if this.shared.roll(faults.stall_per_mille) {
                    this.shared.stats.injected_stalls.fetch_add(1, Ordering::Relaxed);
                    let mut pause = Box::pin(tokio::time::sleep(Duration::from_millis(faults.stall_ms)));
                    let _ = pause.as_mut().poll(cx);
                    this.pause = Some(pause);
                    this.held = Some(frame);
                    return Poll::Pending;
                }
                Poll::Ready(Some(Ok(frame)))
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for ChaosBody {
    fn drop(&mut self) {
        // Dropping the upstream connection is the only signal llama-server gets that nobody is reading.
        self.upstream_task.abort();
        self.shared.stats.upstream_active.fetch_sub(1, Ordering::Relaxed);
        if !self.finished {
            self.shared.stats.client_gone.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn plain(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<OutBody> {
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, content_type)
        .body(Full::new(body.into()).map_err(|e: Infallible| match e {}).boxed_unsync())
        .expect("well-formed response")
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "te", "trailer", "upgrade"] {
        headers.remove(name);
    }
}

async fn handle(req: Request<Incoming>, shared: Arc<Shared>) -> Result<Response<OutBody>, Infallible> {
    if req.uri().path() == "/__chaos/stats" {
        return Ok(plain(StatusCode::OK, "application/json", shared.stats.to_json().to_string()));
    }
    shared.stats.requests.fetch_add(1, Ordering::Relaxed);
    if shared.roll(shared.cfg.faults.http503_per_mille) {
        shared.stats.injected_503.fetch_add(1, Ordering::Relaxed);
        return Ok(plain(StatusCode::SERVICE_UNAVAILABLE, "application/json", r#"{"error":{"code":503,"message":"injected"}}"#));
    }
    match forward(req, &shared).await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            shared.stats.upstream_errors.fetch_add(1, Ordering::Relaxed);
            Ok(plain(StatusCode::BAD_GATEWAY, "text/plain", e.to_string()))
        }
    }
}

async fn forward(req: Request<Incoming>, shared: &Arc<Shared>) -> Result<Response<OutBody>, BoxError> {
    let (mut parts, body) = req.into_parts();
    let authority = parts.uri.authority().map(|a| a.to_string());
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
    parts.uri = Uri::try_from(path)?;
    strip_hop_by_hop(&mut parts.headers);
    if !parts.headers.contains_key(http::header::HOST) {
        let host = authority.unwrap_or_else(|| shared.cfg.upstream.to_string());
        parts.headers.insert(http::header::HOST, host.parse()?);
    }
    parts.version = http::Version::HTTP_11;

    let upstream = TcpStream::connect(shared.cfg.upstream).await?;
    upstream.set_nodelay(true)?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, Incoming>(TokioIo::new(upstream)).await?;
    let upstream_task = tokio::spawn(async move {
        let _ = conn.await;
    });
    shared.stats.upstream_active.fetch_add(1, Ordering::Relaxed);
    let guard = UpstreamGuard { shared: Some(shared.clone()), task: Some(upstream_task) };

    sender.ready().await?;
    let resp = sender.send_request(Request::from_parts(parts, body)).await?;
    let (mut head, inner) = resp.into_parts();
    strip_hop_by_hop(&mut head.headers);
    let (shared, upstream_task) = guard.defuse();
    let chaos = ChaosBody { inner, shared, upstream_task, pause: None, held: None, finished: false };
    Ok(Response::from_parts(head, chaos.boxed_unsync()))
}

/// Keeps the books straight if the request dies before a `ChaosBody` takes over.
struct UpstreamGuard {
    shared: Option<Arc<Shared>>,
    task: Option<JoinHandle<()>>,
}

impl UpstreamGuard {
    fn defuse(mut self) -> (Arc<Shared>, JoinHandle<()>) {
        (self.shared.take().expect("guard is live"), self.task.take().expect("guard is live"))
    }
}

impl Drop for UpstreamGuard {
    fn drop(&mut self) {
        if let (Some(shared), Some(task)) = (self.shared.take(), self.task.take()) {
            task.abort();
            shared.stats.upstream_active.fetch_sub(1, Ordering::Relaxed);
            shared.stats.client_gone.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn tune_socket(stream: &TcpStream, tuning: &SocketTuning) {
    let _ = stream.set_nodelay(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let fd = stream.as_raw_fd();
        if let Some(cc) = &tuning.congestion {
            // SAFETY: fd is a live socket owned by `stream`, and the pointer and length describe `cc`.
            let rc = unsafe { libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_CONGESTION, cc.as_ptr().cast(), cc.len() as libc::socklen_t) };
            if rc != 0 {
                eprintln!("TCP_CONGESTION {cc}: {}", std::io::Error::last_os_error());
            }
        }
        if tuning.thin_linear_timeouts {
            const TCP_THIN_LINEAR_TIMEOUTS: libc::c_int = 16;
            let on: libc::c_int = 1;
            // SAFETY: as above, with a pointer to a live c_int of the stated size.
            unsafe { libc::setsockopt(fd, libc::IPPROTO_TCP, TCP_THIN_LINEAR_TIMEOUTS, (&on as *const libc::c_int).cast(), size_of::<libc::c_int>() as libc::socklen_t) };
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = tuning;
}

/// Serves until the listener fails or the returned task is aborted. `tls: None` serves plaintext.
pub fn spawn(listener: TcpListener, tls: Option<Arc<rustls::ServerConfig>>, cfg: GatewayConfig) -> (Arc<GatewayStats>, JoinHandle<()>) {
    let stats = Arc::new(GatewayStats::default());
    let shared = Arc::new(Shared { rng: Mutex::new(fastrand::Rng::with_seed(cfg.seed)), cfg, stats: stats.clone() });
    let acceptor = tls.map(TlsAcceptor::from);
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            shared.stats.accepted.fetch_add(1, Ordering::Relaxed);
            tune_socket(&stream, &shared.cfg.tuning);
            let shared = shared.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Some(acceptor) = acceptor else {
                    serve_conn(TokioIo::new(stream), shared).await;
                    return;
                };
                match tokio::time::timeout(Duration::from_secs(60), acceptor.accept(stream)).await {
                    Ok(Ok(tls)) => serve_conn(TokioIo::new(tls), shared).await,
                    _ => {
                        shared.stats.tls_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    (stats, task)
}

async fn serve_conn<I>(io: I, shared: Arc<Shared>)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(1024)
        .adaptive_window(true)
        .keep_alive_interval(Duration::from_secs(10))
        .keep_alive_timeout(Duration::from_secs(30));
    builder.http1().timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(120));
    let service = service_fn(move |req| handle(req, shared.clone()));
    let _ = builder.serve_connection(io, service).await;
}

/// Pipes TCP from `listener` to `target`, dialling through `dial`. TLS stays end to end, and the sockets
/// on each side belong to different network namespaces, which is how a client outside the test network
/// gets its traffic through the impaired link.
pub fn spawn_relay<F>(listener: TcpListener, target: SocketAddr, dial: F) -> JoinHandle<()>
where
    F: Fn() -> std::io::Result<tokio::net::TcpSocket> + Send + Sync + 'static,
{
    let dial = Arc::new(dial);
    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let dial = dial.clone();
            tokio::spawn(async move {
                let _ = inbound.set_nodelay(true);
                let Ok(socket) = dial() else { return };
                let Ok(mut outbound) = socket.connect(target).await else { return };
                let _ = outbound.set_nodelay(true);
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    })
}
