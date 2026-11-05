//! End-to-end behaviour against a scripted server, through real sockets and, where it matters, real TLS.
//!
//! The mock's text is a pure function of the seed, so "survived the failure" here means byte-exact output,
//! not merely "did not crash".

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_rt::media::MediaInput;
use agent_rt::{run_agent, AgentConfig, Calc, Cancel, Event, FailKind, Message, RetryPolicy, Role, Runtime, RuntimeConfig, Sink, ToolRegistry, Transport, TransportConfig};
use chaos::gateway::{self, Faults, GatewayConfig, SocketTuning};
use chaos::mock::{text_for, MockConfig, MockServer};
use chaos::{cert, mock};
use parking_lot::Mutex;

#[derive(Default)]
struct Recorder {
    text: Mutex<String>,
    reasoning: Mutex<String>,
    retries: AtomicU32,
    rewinds: AtomicU32,
    tool_results: Mutex<Vec<String>>,
}

impl Sink for Recorder {
    fn event(&self, _: u32, event: Event<'_>) {
        match event {
            Event::Text(t) => self.text.lock().push_str(t),
            Event::Reasoning(t) => self.reasoning.lock().push_str(t),
            Event::Retry { .. } => {
                self.retries.fetch_add(1, Ordering::SeqCst);
            }
            Event::ToolRewind => {
                self.rewinds.fetch_add(1, Ordering::SeqCst);
            }
            Event::ToolResult { output, .. } => self.tool_results.lock().push(output.to_owned()),
            _ => {}
        }
    }
}

fn fast_retry() -> RetryPolicy {
    RetryPolicy { max_attempts: 12, base: Duration::from_millis(5), cap: Duration::from_millis(40) }
}

fn runtime_for(base_url: String, ca: Option<&str>, http2: bool) -> Arc<Runtime> {
    let transport = Transport::new(TransportConfig {
        base_url,
        ca_pem: ca.map(|c| c.as_bytes().to_vec()),
        http2,
        hedge_after: None,
        connect_timeout: Duration::from_secs(5),
        first_byte_timeout: Duration::from_secs(10),
        ..Default::default()
    })
    .unwrap();
    Runtime::new(transport, RuntimeConfig { stall_timeout: Duration::from_millis(400), retry: fast_retry(), ..Default::default() })
}

fn plain_runtime(server: &MockServer) -> Arc<Runtime> {
    runtime_for(format!("http://{}/v1", server.addr), None, false)
}

fn agent(seed: u64, max_tokens: u32) -> AgentConfig {
    AgentConfig { seed: Some(seed), max_tokens, ..AgentConfig::new("mock") }
}

fn user(text: &str) -> Vec<Message> {
    vec![Message::text(Role::User, text)]
}

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn clean_stream_delivers_exact_text() {
    let server = MockServer::spawn(MockConfig::default()).await.unwrap();
    let rec = Recorder::default();
    let summary = run_agent(&plain_runtime(&server), &agent(1, 64), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(1, 0, 64));
    assert_eq!(summary.text, text_for(1, 0, 64));
    assert_eq!(summary.attempts, 1);
    assert_eq!(summary.finish, "stop");
    assert_eq!(summary.usage_completion, 64);
}

#[tokio::test]
async fn repeated_cuts_resume_byte_exact_without_duplication() {
    let cfg = MockConfig { cut_after_tokens: Some(7), cut_attempts: 5, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let summary = run_agent(&plain_runtime(&server), &agent(2, 64), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(2, 0, 64), "text must be stitched exactly");
    assert_eq!(summary.attempts, 6);
    assert_eq!(summary.resumes, 5);
    assert_eq!(summary.tokens, 64, "the token budget must not grow across resumes");
    assert_eq!(server.stats.prefill_mismatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cut_during_reasoning_continues_the_reasoning() {
    let cfg = MockConfig { reasoning_tokens: 20, cut_after_tokens: Some(9), cut_attempts: 2, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    run_agent(&plain_runtime(&server), &agent(3, 40), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.reasoning.lock(), text_for(3, 0, 20));
    assert_eq!(*rec.text.lock(), text_for(3, 20, 40));
    assert_eq!(server.stats.prefill_mismatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stalled_stream_is_detected_and_resumed() {
    let cfg = MockConfig { stall_after_tokens: Some(5), stall_attempts: 2, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let started = Instant::now();
    // Each attempt makes progress before stalling, so patience never grows here.
    let summary = run_agent(&plain_runtime(&server), &agent(4, 32), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(4, 0, 32));
    assert_eq!(summary.fail_counts, vec![(FailKind::Stall, 2)]);
    assert!(started.elapsed() >= Duration::from_millis(800), "two stall timeouts must have passed");
    // Abandoning a stream must release the server side: a hung request that keeps its slot is how a
    // retry storm eats a 128-slot server.
    wait_for("abandoned streams to close", || server.stats.active.load(Ordering::SeqCst) == 0).await;
    assert_eq!(server.stats.cancelled.load(Ordering::SeqCst), 2);
}

/// Found the hard way on the real server: 128 images behind a serial encoder looked like 128 stalled streams,
/// and retrying them produced 1321 attempts and a 135 s time to first token on a clean network.
#[tokio::test]
async fn a_slow_server_does_not_turn_into_a_retry_storm() {
    let cfg = MockConfig { first_byte_delay: Duration::from_millis(1000), ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    // The limit starts at 400 ms, which is well under the server's real latency.
    let summary = run_agent(&plain_runtime(&server), &agent(40, 16), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(40, 0, 16));
    assert_eq!(summary.attempts, 3, "400 ms, 800 ms, then 1600 ms is enough: patience has to converge");
}

/// A connection stuck in TCP backoff accepts the request and then says nothing. That must fail in seconds, not
/// after the minutes the kernel would take to give up (251 s on the test network).
#[tokio::test]
async fn a_silent_connection_is_abandoned_while_waiting_for_headers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicU32::new(0));
    let count = accepted.clone();
    let black_hole = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            held.push(stream); // accepted and then ignored for ever
        }
    });
    let transport = Transport::new(TransportConfig { base_url: format!("http://{addr}/v1"), hedge_after: None, first_byte_timeout: Duration::from_secs(60), ..Default::default() }).unwrap();
    let policy = RetryPolicy { max_attempts: 3, base: Duration::from_millis(1), cap: Duration::from_millis(2) };
    let rt = Runtime::new(transport, RuntimeConfig { stall_timeout: Duration::from_millis(200), retry: policy, ..Default::default() });
    let rec = Recorder::default();
    let started = Instant::now();
    let (error, summary) = run_agent(&rt, &agent(42, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.err().unwrap();
    assert_eq!(error.kind, FailKind::FirstByteTimeout);
    assert_eq!(summary.attempts, 3);
    assert_eq!(accepted.load(Ordering::SeqCst), 3, "every retry must use a new connection");
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?} with a 60 s first byte timeout", started.elapsed());
    black_hole.abort();
}

#[tokio::test]
async fn known_encoding_load_extends_the_stall_limit() {
    let rt = runtime_for("http://127.0.0.1:1/v1".into(), None, false);
    assert_eq!(rt.stall_limit(0), Duration::from_millis(400));
    assert_eq!(rt.stall_limit(1), Duration::from_millis(800));
    assert_eq!(rt.stall_limit(2), Duration::from_millis(1600));
    assert_eq!(rt.stall_limit(99), Duration::from_secs(10), "capped by the first byte timeout");

    // A request with media that is waiting on the encoder makes silence normal for everyone.
    let server = MockServer::spawn(MockConfig { first_byte_delay: Duration::from_millis(300), ..Default::default() }).await.unwrap();
    let rt = plain_runtime(&server);
    let mut png = Vec::new();
    image::RgbImage::new(1280, 720).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    let media = Arc::new(MediaInput::from_bytes(&png, "a.png".into(), 1280).unwrap());
    let task = {
        let rt = rt.clone();
        tokio::spawn(async move {
            let rec = Recorder::default();
            let mut conversation = vec![Message::user_with_media("what", vec![media])];
            run_agent(&rt, &agent(41, 4), 1, &mut conversation, &rec, &Cancel::new()).await.map(|_| ()).map_err(|e| e.0)
        })
    };
    wait_for("the media request to register", || rt.stall_limit(0) > Duration::from_millis(400)).await;
    assert_eq!(rt.stall_limit(0), Duration::from_millis(400) + Duration::from_millis(116) * 3 / 2);
    task.await.unwrap().unwrap();
    assert_eq!(rt.stall_limit(0), Duration::from_millis(400), "the load must be released at the first token");
}

/// Tail loss of the last packets is common on a bad link. The answer is already complete by then.
#[tokio::test]
async fn losing_only_the_end_of_stream_marker_is_not_a_failure() {
    let cfg = MockConfig { stall_after_finish: true, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let transport = Transport::new(TransportConfig { base_url: format!("http://{}/v1", server.addr), ..Default::default() }).unwrap();
    // A stall limit far beyond the grace period shows which of the two ended the wait.
    let rt = Runtime::new(transport, RuntimeConfig { stall_timeout: Duration::from_secs(60), retry: fast_retry(), ..Default::default() });
    let rec = Recorder::default();
    let started = Instant::now();
    let summary = run_agent(&rt, &agent(44, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(44, 0, 8));
    assert_eq!(summary.attempts, 1, "a complete answer must not be asked for again");
    assert_eq!(summary.finish, "stop");
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
}

#[tokio::test]
async fn retries_do_not_go_back_to_the_connection_pool() {
    let cfg = MockConfig { cut_after_tokens: Some(3), cut_attempts: 2, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rt = plain_runtime(&server);
    // Leave an idle connection in the pool, ready to be picked up.
    assert_eq!(rt.transport.preconnect(2).await, 2);
    let rec = Recorder::default();
    run_agent(&rt, &agent(43, 12), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(43, 0, 12));
    let reused = rt.transport.stats.reused.load(Ordering::SeqCst);
    assert_eq!(reused, 1, "only the first attempt may use the pool");
}

#[tokio::test]
async fn server_errors_retry_but_client_errors_do_not() {
    let cfg = MockConfig { fail_status: Some(503), fail_attempts: 3, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let summary = run_agent(&plain_runtime(&server), &agent(5, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(summary.attempts, 4);
    assert_eq!(*rec.text.lock(), text_for(5, 0, 8));

    let cfg = MockConfig { fail_status: Some(400), fail_attempts: 100, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let (error, summary) = run_agent(&plain_runtime(&server), &agent(6, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.err().unwrap();
    assert_eq!(error.kind, FailKind::Http(400));
    assert!(error.detail.contains("scripted"), "{}", error.detail);
    assert_eq!(summary.attempts, 1, "a 400 must not be retried");
}

#[tokio::test]
async fn sse_error_event_mid_stream_is_retried() {
    let cfg = MockConfig { sse_error_attempts: 2, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let summary = run_agent(&plain_runtime(&server), &agent(7, 16), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(7, 0, 16));
    assert_eq!(summary.fail_counts, vec![(FailKind::Server(500), 2)]);
}

#[tokio::test]
async fn gives_up_after_the_retry_budget() {
    let cfg = MockConfig { fail_status: Some(500), fail_attempts: 1000, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let (error, summary) = run_agent(&plain_runtime(&server), &agent(8, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.err().unwrap();
    assert_eq!(error.kind, FailKind::Http(500));
    assert_eq!(summary.attempts, fast_retry().max_attempts);
}

#[tokio::test]
async fn tool_runs_once_even_when_the_call_is_cut_midway() {
    let cfg = MockConfig { tools: true, cut_in_tool_attempts: 3, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let mut conversation = user("please use calc");
    let cfg = AgentConfig { tools: ToolRegistry::new().with(Calc), ..agent(9, 32) };
    let summary = run_agent(&plain_runtime(&server), &cfg, 1, &mut conversation, &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(rec.rewinds.load(Ordering::SeqCst), 3);
    assert_eq!(*rec.tool_results.lock(), vec!["42".to_owned()], "exactly one execution, with intact arguments");
    assert_eq!(summary.text, "result is 42");
    assert_eq!(summary.steps, 2);
    assert_eq!(conversation.len(), 4, "user, assistant call, tool result, assistant answer");
}

#[tokio::test]
async fn media_is_delivered_intact_on_every_attempt() {
    let cfg = MockConfig { cut_after_tokens: Some(3), cut_attempts: 2, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let mut png = Vec::new();
    image::RgbImage::from_fn(32, 32, |x, y| image::Rgb([x as u8 * 7, y as u8 * 7, 9])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    let media = Arc::new(MediaInput::from_bytes(&png, "a.png".into(), 1280).unwrap());
    let mut conversation = vec![Message::user_with_media("what is it", vec![media])];
    let rec = Recorder::default();
    run_agent(&plain_runtime(&server), &agent(10, 16), 1, &mut conversation, &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(10, 0, 16));
    let url = server.stats.last_media.lock().clone().unwrap();
    let b64 = url.strip_prefix("data:image/png;base64,").unwrap();
    assert_eq!(base64_simd::STANDARD.decode_to_vec(b64).unwrap(), png);
}

#[tokio::test]
async fn cancel_closes_the_connection_and_frees_the_server() {
    let cfg = MockConfig { token_interval: Duration::from_millis(5), ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rt = plain_runtime(&server);
    let cancel = Cancel::new();
    let rec = Arc::new(Recorder::default());
    let task = {
        let (rt, cancel, rec) = (rt.clone(), cancel.clone(), rec.clone());
        tokio::spawn(async move { run_agent(&rt, &agent(11, 100_000), 1, &mut user("hi"), rec.as_ref(), &cancel).await })
    };
    wait_for("the first tokens", || !rec.text.lock().is_empty()).await;
    cancel.cancel();
    let (error, _) = task.await.unwrap().err().unwrap();
    assert_eq!(error.kind, FailKind::Cancelled);
    wait_for("the server to notice", || server.stats.active.load(Ordering::SeqCst) == 0).await;
}

async fn gateway_with(server: &MockServer, faults: Faults) -> (String, cert::Identity, Arc<gateway::GatewayStats>) {
    let id = cert::generate(&["localhost".into()]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tls = cert::server_config(&id).unwrap();
    let cfg = GatewayConfig { upstream: server.addr, faults, tuning: SocketTuning::default(), seed: 3 };
    let (stats, _) = gateway::spawn(listener, Some(tls), cfg);
    (format!("https://localhost:{port}/v1"), id, stats)
}

#[tokio::test]
async fn https_over_http1_and_http2() {
    for http2 in [false, true] {
        let server = MockServer::spawn(MockConfig { close_connection: true, ..Default::default() }).await.unwrap();
        let (url, id, stats) = gateway_with(&server, Faults::default()).await;
        let rt = runtime_for(url, Some(&id.ca_pem), http2);
        for seed in [20, 21, 22] {
            let rec = Recorder::default();
            run_agent(&rt, &agent(seed, 32), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
            assert_eq!(*rec.text.lock(), text_for(seed, 0, 32));
        }
        // The upstream closes after every response, yet the client keeps one TLS session through the gateway.
        assert_eq!(stats.accepted.load(Ordering::SeqCst), 1, "http2={http2}: connection should be reused");
        assert_eq!(rt.transport.stats.reused.load(Ordering::SeqCst), 2);
        wait_for("upstream count", || stats.upstream_active.load(Ordering::SeqCst) == 0).await;
    }
}

#[tokio::test]
async fn untrusted_certificate_is_refused_and_not_retried_into_success() {
    let server = MockServer::spawn(MockConfig::default()).await.unwrap();
    let (url, _, _) = gateway_with(&server, Faults::default()).await;
    let other_ca = cert::generate(&["localhost".into()]).unwrap().ca_pem;
    let transport = Transport::new(TransportConfig { base_url: url, ca_pem: Some(other_ca.into_bytes()), hedge_after: None, ..Default::default() }).unwrap();
    let policy = RetryPolicy { max_attempts: 2, base: Duration::from_millis(1), cap: Duration::from_millis(2) };
    let rt = Runtime::new(transport, RuntimeConfig { retry: policy, ..Default::default() });
    let rec = Recorder::default();
    let (error, _) = run_agent(&rt, &agent(30, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.err().unwrap();
    assert_eq!(error.kind, FailKind::Tls);
    assert!(rec.text.lock().is_empty());
    assert_eq!(server.stats.requests.load(Ordering::SeqCst), 0, "nothing may reach the server");
}

/// The thing the whole design is for: full concurrency, TLS, and a gateway resetting streams at random.
async fn storm(http2: bool) {
    const STREAMS: u64 = 128;
    const TOKENS: u32 = 200;
    let server = MockServer::spawn(MockConfig { reasoning_tokens: 16, token_interval: Duration::from_micros(200), close_connection: true, ..Default::default() }).await.unwrap();
    let faults = Faults { reset_per_mille: 8, stall_per_mille: 2, stall_ms: 30, http503_per_mille: 40 };
    let (url, id, stats) = gateway_with(&server, faults).await;
    let transport = Transport::new(TransportConfig { base_url: url, ca_pem: Some(id.ca_pem.into_bytes()), http2, h2_conns: 4, hedge_after: None, ..Default::default() }).unwrap();
    let policy = RetryPolicy { max_attempts: 64, base: Duration::from_millis(5), cap: Duration::from_millis(100) };
    let rt = Runtime::new(transport, RuntimeConfig { stall_timeout: Duration::from_secs(5), retry: policy, ..Default::default() });

    let mut set = tokio::task::JoinSet::new();
    for seed in 0..STREAMS {
        let rt = rt.clone();
        set.spawn(async move {
            let rec = Recorder::default();
            let result = run_agent(&rt, &agent(seed + 1000, TOKENS), seed as u32, &mut user("hi"), &rec, &Cancel::new()).await;
            let summary = result.map_err(|e| e.0).unwrap();
            let (reasoning, text) = (rec.reasoning.lock().clone(), rec.text.lock().clone());
            assert_eq!(reasoning, text_for(seed + 1000, 0, 16), "reasoning of stream {seed}");
            assert_eq!(text, text_for(seed + 1000, 16, TOKENS as usize), "text of stream {seed}");
            summary.resumes
        });
    }
    let mut resumes = 0;
    while let Some(r) = set.join_next().await {
        resumes += r.unwrap();
    }
    assert!(resumes > 100, "the storm should have forced many resumes, got {resumes}");
    assert_eq!(server.stats.prefill_mismatches.load(Ordering::SeqCst), 0);
    assert!(stats.injected_resets.load(Ordering::SeqCst) > 0);
    wait_for("every upstream to close", || stats.upstream_active.load(Ordering::SeqCst) == 0).await;
    wait_for("server to go idle", || server.stats.active.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storm_128_streams_http1() {
    storm(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storm_128_streams_http2() {
    storm(true).await;
}

#[tokio::test]
async fn admission_never_exceeds_the_slot_count() {
    let server = MockServer::spawn(MockConfig { token_interval: Duration::from_micros(500), ..Default::default() }).await.unwrap();
    let transport = Transport::new(TransportConfig { base_url: format!("http://{}/v1", server.addr), ..Default::default() }).unwrap();
    let rt = Runtime::new(transport, RuntimeConfig { max_concurrency: 8, ..Default::default() });
    let peak = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let (peak, stats, done) = (peak.clone(), server.stats.clone(), done.clone());
        tokio::spawn(async move {
            while !done.load(Ordering::SeqCst) {
                peak.fetch_max(stats.active.load(Ordering::SeqCst), Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
        })
    };
    let mut set = tokio::task::JoinSet::new();
    for seed in 0..40u64 {
        let rt = rt.clone();
        set.spawn(async move {
            let rec = Recorder::default();
            run_agent(&rt, &agent(seed + 5000, 24), seed as u32, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
        });
    }
    while let Some(r) = set.join_next().await {
        r.unwrap();
    }
    done.store(true, Ordering::SeqCst);
    watcher.await.unwrap();
    let observed = peak.load(Ordering::SeqCst);
    assert!((2..=8).contains(&observed), "peak concurrency {observed}");
}

#[tokio::test]
async fn a_long_answer_over_a_link_that_keeps_dropping_still_finishes() {
    // More cuts than max_attempts, and every attempt delivered something: that is progress, not failure.
    let cfg = MockConfig { cut_after_tokens: Some(7), cut_attempts: 1000, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let summary = run_agent(&plain_runtime(&server), &agent(50, 200), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(50, 0, 200));
    assert!(summary.attempts > fast_retry().max_attempts, "{} attempts", summary.attempts);
}

#[tokio::test]
async fn text_from_a_failed_run_stays_in_the_conversation() {
    let cfg = MockConfig { cut_after_tokens: Some(4), cut_attempts: 1, fail_status: None, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rt = plain_runtime(&server);
    let cancel = Cancel::new();
    let mut conversation = user("hi");
    struct CancelOnText(Cancel);
    impl Sink for CancelOnText {
        fn event(&self, _: u32, event: Event<'_>) {
            if matches!(event, Event::Text(_)) {
                self.0.cancel();
            }
        }
    }
    let (error, _) = run_agent(&rt, &agent(51, 100), 1, &mut conversation, &CancelOnText(cancel.clone()), &cancel).await.err().unwrap();
    assert_eq!(error.kind, FailKind::Cancelled);
    assert_eq!(conversation.len(), 2, "what the user saw has to be part of what comes next");
    assert_eq!(conversation[1].role, Role::Assistant);
}

#[tokio::test]
async fn cancelling_during_a_tool_leaves_a_valid_conversation() {
    struct Slow;
    impl agent_rt::Tool for Slow {
        fn name(&self) -> &str {
            "calc"
        }
        fn description(&self) -> &str {
            "never finishes in time"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn call(&self, _: &str) -> agent_rt::tools::ToolFuture {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(String::new())
            })
        }
    }
    struct CancelOnToolArgs(Cancel);
    impl Sink for CancelOnToolArgs {
        fn event(&self, _: u32, event: Event<'_>) {
            if matches!(event, Event::TurnEnd { .. }) {
                self.0.cancel();
            }
        }
    }
    let server = MockServer::spawn(MockConfig { tools: true, ..Default::default() }).await.unwrap();
    let cancel = Cancel::new();
    let mut conversation = user("please use calc");
    let cfg = AgentConfig { tools: ToolRegistry::new().with(Slow), ..agent(52, 32) };
    let (error, _) = run_agent(&plain_runtime(&server), &cfg, 1, &mut conversation, &CancelOnToolArgs(cancel.clone()), &cancel).await.err().unwrap();
    assert_eq!(error.kind, FailKind::Cancelled);
    let calls: Vec<_> = conversation.iter().flat_map(|m| m.tool_calls.iter().map(|c| c.id.clone())).collect();
    assert_eq!(calls.len(), 1);
    let answered = conversation.iter().filter(|m| m.tool_call_id.as_ref() == Some(&calls[0])).count();
    assert_eq!(answered, 1, "every tool call needs its result");
}

/// The real model calls a tool whenever one is on offer. A run limited to one step used to end with nothing but that call.
#[tokio::test]
async fn the_last_step_offers_no_tools_so_the_run_ends_in_an_answer() {
    let server = MockServer::spawn(MockConfig { tools: true, ..Default::default() }).await.unwrap();
    let rec = Recorder::default();
    let cfg = AgentConfig { tools: ToolRegistry::new().with(Calc), max_steps: 1, ..agent(71, 8) };
    let summary = run_agent(&plain_runtime(&server), &cfg, 1, &mut user("please use calc"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(summary.tool_calls, 0);
    assert_eq!(*rec.text.lock(), text_for(71, 0, 8));

    // With two steps the tool runs in the first, and the second must answer.
    let rec = Recorder::default();
    let cfg = AgentConfig { tools: ToolRegistry::new().with(Calc), max_steps: 2, ..agent(72, 8) };
    let summary = run_agent(&plain_runtime(&server), &cfg, 1, &mut user("please use calc"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(summary.tool_calls, 1);
    assert_eq!(summary.text, "result is 42");
}

#[tokio::test]
async fn thrown_away_tool_fragments_do_not_use_up_the_budget() {
    // Three cuts inside the call with a budget of three tokens used to end the turn with nothing at all.
    let cfg = MockConfig { tools: true, cut_in_tool_attempts: 3, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let cfg = AgentConfig { tools: ToolRegistry::new().with(Calc), ..agent(53, 3) };
    run_agent(&plain_runtime(&server), &cfg, 1, &mut user("please use calc"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.tool_results.lock(), vec!["42".to_owned()]);
}

#[tokio::test]
async fn a_server_ignoring_max_tokens_cannot_run_away() {
    let cfg = MockConfig { overrun_to: Some(50_000), ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let summary = run_agent(&plain_runtime(&server), &agent(54, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert!(summary.tokens < 100, "accepted {} tokens against a budget of 8", summary.tokens);
    assert_eq!(summary.finish, "length");
    assert_eq!(summary.attempts, 1);
    wait_for("the connection to close", || server.stats.active.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test]
async fn garbage_after_the_finish_reason_does_not_reopen_the_turn() {
    let cfg = MockConfig { garbage_after_finish: true, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let rec = Recorder::default();
    let summary = run_agent(&plain_runtime(&server), &agent(55, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(summary.attempts, 1, "a finished answer must not be asked for again");
    assert_eq!(*rec.text.lock(), text_for(55, 0, 8));
}

#[tokio::test]
async fn comments_after_done_cannot_hold_the_slot() {
    let server = MockServer::spawn(MockConfig { pings_after_done: true, ..Default::default() }).await.unwrap();
    let rec = Recorder::default();
    let started = Instant::now();
    run_agent(&plain_runtime(&server), &agent(56, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert!(started.elapsed() < Duration::from_secs(4), "took {:?}", started.elapsed());
    wait_for("the connection to close", || server.stats.active.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test]
async fn every_address_of_a_name_gets_tried() {
    // Whichever way `localhost` resolves here, a server on only one family has to be found.
    let v4 = MockServer::spawn(MockConfig::default()).await.unwrap();
    let rec = Recorder::default();
    let rt = runtime_for(format!("http://localhost:{}/v1", v4.addr.port()), None, false);
    run_agent(&rt, &agent(57, 4), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(57, 0, 4));

    let Ok(v6) = MockServer::spawn_v6(MockConfig::default()).await else { return };
    let rec = Recorder::default();
    let rt = runtime_for(format!("http://localhost:{}/v1", v6.addr.port()), None, false);
    run_agent(&rt, &agent(58, 4), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(58, 0, 4));
}

#[tokio::test]
async fn http2_falls_back_when_the_server_only_offers_http1() {
    let server = MockServer::spawn(MockConfig::default()).await.unwrap();
    let id = cert::generate(&["localhost".into()]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut tls = (*cert::server_config(&id).unwrap()).clone();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let cfg = GatewayConfig { upstream: server.addr, faults: Faults::default(), tuning: SocketTuning::default(), seed: 1 };
    let _gateway = gateway::spawn(listener, Some(Arc::new(tls)), cfg);
    let rt = runtime_for(format!("https://localhost:{port}/v1"), Some(&id.ca_pem), true);
    let rec = Recorder::default();
    run_agent(&rt, &agent(59, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(59, 0, 8));
}

#[tokio::test]
async fn http2_failures_do_not_queue_behind_one_dial() {
    // Accepts TCP and then never speaks TLS, so that every dial hangs until its timeout.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hole = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    let id = cert::generate(&["localhost".into()]).unwrap();
    let transport = Transport::new(TransportConfig {
        base_url: format!("https://localhost:{port}/v1"),
        ca_pem: Some(id.ca_pem.clone().into_bytes()),
        http2: true,
        h2_conns: 1,
        hedge_after: None,
        connect_timeout: Duration::from_millis(400),
        ..Default::default()
    })
    .unwrap();
    let policy = RetryPolicy { max_attempts: 1, ..Default::default() };
    let rt = Runtime::new(transport, RuntimeConfig { retry: policy, ..Default::default() });
    let started = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for seed in 0..8u64 {
        let rt = rt.clone();
        set.spawn(async move {
            let rec = Recorder::default();
            run_agent(&rt, &agent(seed + 60, 4), 1, &mut user("hi"), &rec, &Cancel::new()).await.is_err()
        });
    }
    while let Some(failed) = set.join_next().await {
        assert!(failed.unwrap());
    }
    assert!(started.elapsed() < Duration::from_millis(1200), "8 streams took {:?}, as if dialling one at a time", started.elapsed());
    hole.abort();
}

#[tokio::test]
async fn an_http2_retry_does_not_go_back_to_the_connection_that_just_failed() {
    let cfg = MockConfig { stall_after_tokens: Some(3), stall_attempts: 1, close_connection: true, ..Default::default() };
    let server = MockServer::spawn(cfg).await.unwrap();
    let (url, id, stats) = gateway_with(&server, Faults::default()).await;
    let transport = Transport::new(TransportConfig { base_url: url, ca_pem: Some(id.ca_pem.into_bytes()), http2: true, h2_conns: 1, hedge_after: None, ..Default::default() }).unwrap();
    let rt = Runtime::new(transport, RuntimeConfig { stall_timeout: Duration::from_millis(300), retry: fast_retry(), ..Default::default() });
    let rec = Recorder::default();
    let summary = run_agent(&rt, &agent(70, 8), 1, &mut user("hi"), &rec, &Cancel::new()).await.map_err(|e| e.0).unwrap();
    assert_eq!(*rec.text.lock(), text_for(70, 0, 8));
    assert_eq!(summary.attempts, 2);
    assert_eq!(stats.accepted.load(Ordering::SeqCst), 2, "the retry belongs on a new connection");
}

#[test]
fn mock_token_stream_is_stable() {
    assert_eq!(mock::text_for(1, 0, 4), mock::text_for(1, 0, 2) + &mock::text_for(1, 2, 4));
}
