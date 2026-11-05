//! A deterministic stand-in for llama-server.
//!
//! It generates the same text for the same seed and implements assistant prefill by continuing at the
//! prefilled offset. So after any number of reconnects, a correct client ends up with exactly `text_for`,
//! byte for byte, which a real model's sampling could never prove.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const WORDS: &[&str] = &[
    "stream", "token", " agent", " runtime", " vector", " 緩存", " kernel", " graph", " node", " 数据", "\n", " buffer", " slot", " retry", " \"quoted\"", " back\\slash", " emoji🦀", " end.",
];

/// The `i`-th token for a seed.
pub fn token(seed: u64, i: usize) -> &'static str {
    let mixed = (seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    WORDS[(mixed >> 40) as usize % WORDS.len()]
}

pub fn text_for(seed: u64, from: usize, to: usize) -> String {
    (from..to).map(|i| token(seed, i)).collect()
}

/// Scripted misbehaviour, keyed by how many attempts this seed has seen so far.
#[derive(Clone, Debug, Default)]
pub struct MockConfig {
    /// Time between tokens.
    pub token_interval: Duration,
    /// Time before the first event of a response.
    pub first_byte_delay: Duration,
    /// Number of initial tokens sent as `reasoning_content`, the way LFM3 starts its answers.
    pub reasoning_tokens: usize,
    /// For attempts 0..N of a seed, cut the connection after this many tokens of that attempt.
    pub cut_after_tokens: Option<usize>,
    pub cut_attempts: u32,
    /// For attempts 0..N of a seed, go silent after this many tokens instead of cutting.
    pub stall_after_tokens: Option<usize>,
    pub stall_attempts: u32,
    /// For attempts 0..N of a seed, answer with this status.
    pub fail_status: Option<u16>,
    pub fail_attempts: u32,
    /// For attempts 0..N of a seed, send an SSE `error:` event mid-stream.
    pub sse_error_attempts: u32,
    /// Call the `calc` tool whenever the user message contains "use calc" and no tool result exists yet.
    pub tools: bool,
    /// Cut this many times in the middle of a tool call.
    pub cut_in_tool_attempts: u32,
    pub close_connection: bool,
    /// After the finish reason, go silent instead of sending usage and `[DONE]`.
    pub stall_after_finish: bool,
    /// After the finish reason, send something that does not parse.
    pub garbage_after_finish: bool,
    /// After `[DONE]`, keep sending SSE comments instead of ending the body.
    pub pings_after_done: bool,
    /// Ignore `max_tokens` and keep generating up to this many tokens.
    pub overrun_to: Option<usize>,
}

#[derive(Default)]
pub struct MockStats {
    pub requests: AtomicU64,
    pub active: AtomicI64,
    pub cancelled: AtomicU64,
    pub prefill_mismatches: AtomicU64,
    pub resumed: AtomicU64,
    pub tool_results_seen: AtomicU64,
    /// Bytes of the most recent data URL received, to check media survives the trip intact.
    pub last_media: Mutex<Option<String>>,
    attempts: Mutex<std::collections::HashMap<u64, u32>>,
}

impl MockStats {
    fn attempt_of(&self, seed: u64) -> u32 {
        let mut attempts = self.attempts.lock();
        let slot = attempts.entry(seed).or_insert(0);
        let current = *slot;
        *slot += 1;
        current
    }
}

struct ActiveGuard(Arc<MockStats>, bool);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        if !self.1 {
            self.0.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn delta_event(id: &str, delta: Value, finish: Option<&str>) -> Bytes {
    let chunk = json!({
        "choices": [{"finish_reason": finish, "index": 0, "delta": delta}],
        "created": 1, "id": id, "model": "mock", "object": "chat.completion.chunk"
    });
    let mut out = b"data: ".to_vec();
    serde_json::to_writer(&mut out, &chunk).expect("chunk");
    out.extend_from_slice(b"\n\n");
    Bytes::from(out)
}

/// Counts how many tokens of the deterministic stream a prefill covers, or `None` if it does not match.
fn tokens_covered(seed: u64, reasoning: &str, text: &str, reasoning_tokens: usize) -> Option<usize> {
    let mut i = 0;
    let mut rest = reasoning;
    while !rest.is_empty() {
        rest = rest.strip_prefix(token(seed, i))?;
        i += 1;
    }
    if !text.is_empty() && i < reasoning_tokens {
        return None;
    }
    let mut rest = text;
    while !rest.is_empty() {
        rest = rest.strip_prefix(token(seed, i))?;
        i += 1;
    }
    Some(i)
}

async fn handle(req: Request<Incoming>, cfg: Arc<MockConfig>, stats: Arc<MockStats>) -> Result<Response<http_body_util::combinators::BoxBody<Bytes, Infallible>>, Infallible> {
    let path = req.uri().path().to_owned();
    if path.ends_with("/slots") {
        let busy = stats.active.load(Ordering::SeqCst).max(0) as usize;
        let slots: Vec<Value> = (0..128).map(|i| json!({"id": i, "is_processing": i < busy})).collect();
        return Ok(Response::new(Full::new(Bytes::from(Value::Array(slots).to_string())).boxed()));
    }
    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return Ok(Response::new(Full::new(Bytes::new()).boxed())),
    };
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        let mut resp = Response::new(Full::new(Bytes::from_static(b"{\"error\":{\"code\":400,\"message\":\"bad json\"}}")).boxed());
        *resp.status_mut() = StatusCode::BAD_REQUEST;
        return Ok(resp);
    };
    stats.requests.fetch_add(1, Ordering::SeqCst);

    let seed = v["seed"].as_u64().unwrap_or(0);
    let max_tokens = v["max_tokens"].as_u64().unwrap_or(16) as usize;
    let attempt = stats.attempt_of(seed);
    if let Some(status) = cfg.fail_status.filter(|_| attempt < cfg.fail_attempts) {
        let body = json!({"error": {"code": status, "message": "scripted failure"}}).to_string();
        let mut resp = Response::new(Full::new(Bytes::from(body)).boxed());
        *resp.status_mut() = StatusCode::from_u16(status).expect("status");
        return Ok(resp);
    }

    let messages = v["messages"].as_array().cloned().unwrap_or_default();
    for m in &messages {
        if let Some(parts) = m["content"].as_array() {
            for p in parts {
                for key in ["image_url", "video_url"] {
                    if let Some(url) = p[key]["url"].as_str() {
                        *stats.last_media.lock() = Some(url.to_owned());
                    }
                }
            }
        }
    }
    let saw_tool_result = messages.iter().any(|m| m["role"] == "tool");
    if saw_tool_result {
        stats.tool_results_seen.fetch_add(1, Ordering::SeqCst);
    }
    let last_user_text = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .map(|m| match &m["content"] {
            Value::String(s) => s.clone(),
            Value::Array(a) => a.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
            _ => String::new(),
        })
        .unwrap_or_default();

    // A trailing assistant message is a prefill.
    let mut position = match messages.last() {
        Some(m) if m["role"] == "assistant" && m["tool_calls"].is_null() => {
            let text = m["content"].as_str().unwrap_or("");
            let reasoning = m["reasoning_content"].as_str().unwrap_or("");
            stats.resumed.fetch_add(1, Ordering::SeqCst);
            match tokens_covered(seed, reasoning, text, cfg.reasoning_tokens) {
                Some(n) => n,
                None => {
                    stats.prefill_mismatches.fetch_add(1, Ordering::SeqCst);
                    0
                }
            }
        }
        _ => 0,
    };

    let offered_tools = v["tools"].as_array().is_some_and(|t| !t.is_empty());
    let wants_tool = cfg.tools && offered_tools && !saw_tool_result && last_user_text.contains("use calc");
    let tool_result = messages.iter().rev().find(|m| m["role"] == "tool").and_then(|m| m["content"].as_str()).map(str::to_owned);

    let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(64);
    stats.active.fetch_add(1, Ordering::SeqCst);
    let guard_stats = stats.clone();
    let task_cfg = cfg.clone();
    tokio::spawn(async move {
        let mut guard = ActiveGuard(guard_stats, false);
        let cfg = task_cfg;
        let id = format!("mock-{seed}-{attempt}");
        macro_rules! send {
            ($bytes:expr) => {
                if tx.send(Ok(Frame::data($bytes))).await.is_err() {
                    return; // the client went away, and the guard records it
                }
            };
        }
        if !cfg.first_byte_delay.is_zero() {
            tokio::time::sleep(cfg.first_byte_delay).await;
        }
        send!(delta_event(&id, json!({"role": "assistant", "content": null}), None));

        if wants_tool {
            let pieces = ["{\"expr\":", "\"6*", "7\"}"];
            send!(delta_event(&id, json!({"tool_calls": [{"index": 0, "id": "call-1", "type": "function", "function": {"name": "calc", "arguments": pieces[0]}}]}), None));
            if attempt < cfg.cut_in_tool_attempts {
                return; // connection drops halfway through the call
            }
            for piece in &pieces[1..] {
                send!(delta_event(&id, json!({"tool_calls": [{"index": 0, "function": {"arguments": piece}}]}), None));
            }
            send!(delta_event(&id, json!({}), Some("tool_calls")));
            send!(Bytes::from_static(b"data: [DONE]\n\n"));
            guard.1 = true;
            return;
        }

        if let Some(result) = tool_result {
            send!(delta_event(&id, json!({"content": format!("result is {result}")}), None));
            send!(delta_event(&id, json!({}), Some("stop")));
            send!(Bytes::from_static(b"data: [DONE]\n\n"));
            guard.1 = true;
            return;
        }

        let end = cfg.overrun_to.unwrap_or(position + max_tokens);
        let mut sent_this_attempt = 0;
        while position < end {
            if !cfg.token_interval.is_zero() {
                tokio::time::sleep(cfg.token_interval).await;
            }
            let t = token(seed, position);
            let delta = if position < cfg.reasoning_tokens { json!({"reasoning_content": t}) } else { json!({"content": t}) };
            send!(delta_event(&id, delta, None));
            position += 1;
            sent_this_attempt += 1;
            if let Some(n) = cfg.cut_after_tokens.filter(|_| attempt < cfg.cut_attempts)
                && sent_this_attempt >= n {
                    return;
                }
            if let Some(n) = cfg.stall_after_tokens.filter(|_| attempt < cfg.stall_attempts)
                && sent_this_attempt >= n {
                    tx.closed().await; // silent until the client gives up on this connection
                    return;
                }
            if attempt < cfg.sse_error_attempts && sent_this_attempt >= 2 {
                send!(Bytes::from_static(b"error: {\"code\":500,\"message\":\"scripted\"}\n\n"));
                return;
            }
        }
        send!(delta_event(&id, json!({}), Some("stop")));
        if cfg.stall_after_finish {
            tx.closed().await;
            return;
        }
        if cfg.garbage_after_finish {
            send!(Bytes::from_static(b"data: {\"choices\":[],\"usage\":{\"completion_tokens\":-1}}\n\n"));
            send!(Bytes::from_static(b"data: [DONE]\n\n"));
            guard.1 = true;
            return;
        }
        send!(Bytes::from(format!(
            "data: {{\"choices\":[],\"usage\":{{\"completion_tokens\":{sent_this_attempt},\"prompt_tokens\":3}},\"timings\":{{\"predicted_n\":{sent_this_attempt},\"predicted_ms\":1.0}}}}\n\n"
        )));
        send!(Bytes::from_static(b"data: [DONE]\n\n"));
        guard.1 = true;
        if cfg.pings_after_done {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                send!(Bytes::from_static(b": ping\n\n"));
            }
        }
    });

    let mut builder = Response::builder().header(http::header::CONTENT_TYPE, "text/event-stream");
    if cfg.close_connection {
        builder = builder.header(http::header::CONNECTION, "close");
    }
    Ok(builder.body(BodyExt::boxed(StreamBody::new(RxStream(rx)))).expect("response"))
}

struct RxStream(mpsc::Receiver<Result<Frame<Bytes>, Infallible>>);

impl futures_core::Stream for RxStream {
    type Item = Result<Frame<Bytes>, Infallible>;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

pub struct MockServer {
    pub addr: SocketAddr,
    pub stats: Arc<MockStats>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockServer {
    pub async fn spawn(cfg: MockConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        Self::spawn_on(listener, cfg)
    }

    /// Serves on the IPv6 loopback only.
    pub async fn spawn_v6(cfg: MockConfig) -> std::io::Result<Self> {
        Self::spawn_on(TcpListener::bind("[::1]:0").await?, cfg)
    }

    pub fn spawn_on(listener: TcpListener, cfg: MockConfig) -> std::io::Result<Self> {
        let addr = listener.local_addr()?;
        let stats = Arc::new(MockStats::default());
        let cfg = Arc::new(cfg);
        let stats2 = stats.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                let _ = stream.set_nodelay(true);
                let (cfg, stats) = (cfg.clone(), stats2.clone());
                tokio::spawn(async move {
                    let service = service_fn(move |req| handle(req, cfg.clone(), stats.clone()));
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
                });
            }
        });
        Ok(Self { addr, stats, task })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefill_coverage_is_exact() {
        let seed = 42;
        let text = text_for(seed, 0, 10);
        assert_eq!(tokens_covered(seed, "", &text, 0), Some(10));
        assert_eq!(tokens_covered(seed, &text_for(seed, 0, 3), &text_for(seed, 3, 7), 3), Some(7));
        assert_eq!(tokens_covered(seed, "", "definitely wrong", 0), None);
    }
}
