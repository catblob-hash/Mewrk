//! The agent loop over real sockets, against a server that speaks just enough of each protocol.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mewrk_core::tasks::Session;
use mewrk_core::*;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One response per request. The last one repeats.
#[derive(Clone)]
enum Reply {
    Sse(Vec<String>),
    /// Sends these events, then drops the connection.
    Cut(Vec<String>),
    Status(u16, String),
}

struct Server {
    addr: String,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    requests: Arc<AtomicUsize>,
    headers: Arc<Mutex<Vec<String>>>,
}

async fn serve(replies: Vec<Reply>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}/v1", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let headers = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(AtomicUsize::new(0));
    let (b, h, r) = (bodies.clone(), headers.clone(), requests.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (b, h, r, replies) = (b.clone(), h.clone(), r.clone(), replies.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    // Read one whole request, using Content-Length.
                    let (head_end, len) = loop {
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                            let len = head.lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                            break (p + 4, len);
                        }
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    while buf.len() < head_end + len {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    h.lock().push(String::from_utf8_lossy(&buf[..head_end]).into_owned());
                    if let Ok(v) = serde_json::from_slice(&buf[head_end..head_end + len]) {
                        b.lock().push(v);
                    }
                    buf.drain(..head_end + len);
                    let n = r.fetch_add(1, Ordering::SeqCst);
                    let reply = replies[n.min(replies.len() - 1)].clone();
                    match reply {
                        Reply::Status(code, body) => {
                            let _ = sock.write_all(format!("HTTP/1.1 {code} X\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await;
                        }
                        Reply::Sse(events) | Reply::Cut(events) if false => {
                            let _ = events;
                        }
                        Reply::Sse(events) => {
                            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await;
                            for e in events {
                                let frame = format!("data: {e}\n\n");
                                let _ = sock.write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes()).await;
                            }
                            let _ = sock.write_all(b"0\r\n\r\n").await;
                        }
                        Reply::Cut(events) => {
                            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await;
                            for e in events {
                                let frame = format!("data: {e}\n\n");
                                let _ = sock.write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes()).await;
                            }
                            let _ = sock.flush().await;
                            return;
                        }
                    }
                }
            });
        }
    });
    Server { addr, bodies, requests, headers }
}

fn chat_text(s: &str) -> String {
    serde_json::json!({"choices":[{"delta":{"content":s}}]}).to_string()
}

fn chat_done() -> Vec<String> {
    vec![
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_owned(),
        r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5}}"#.to_owned(),
        "[DONE]".to_owned(),
    ]
}

struct Rig {
    engine: Arc<Engine>,
    dir: std::path::PathBuf,
}

fn rig(handle: tokio::runtime::Handle) -> Rig {
    let dir = std::env::temp_dir().join(format!("mewrk-engine-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.clone()).unwrap();
    Rig { engine: Engine::new(handle, store, Notifier::none()), dir }
}

fn spec(addr: &str, kind: ProviderKind, prefill: bool, dir: &std::path::Path) -> RunSpec {
    RunSpec {
        conv_id: "c".into(),
        store_key: "c".into(),
        items: vec![Item::User { id: 1, text: "hello".into(), images: vec![], at: 0 }],
        settings: ConvSettings { permission: PermissionMode::FullAccess, ..Default::default() },
        provider: ProviderConfig { id: "p".into(), name: "p".into(), kind, base_url: addr.to_owned(), models: vec![], resume_prefill: prefill, stall_secs: 5 },
        model: ModelProfile { id: "m".into(), context_window: 1000, max_output: 100, ..Default::default() },
        api_key: Some("secret-key".into()),
        workspaces: vec![dir.to_path_buf()],
        global_instructions: String::new(),
        session: Session::new(),
        child: None,
    }
}

/// Follows a run to its end, the way the UI does. Returns the mirrored items and how it ended.
async fn follow(handle: &RunHandle, start: Vec<Item>) -> (Vec<Item>, RunEnd, Vec<RunEvent>) {
    let mut items = start;
    let mut all = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let mut out = Vec::new();
        handle.inbox.drain(&mut out);
        for ev in out {
            match &ev {
                RunEvent::Push(i) => items.push(i.clone()),
                RunEvent::AppendText { id, delta } => {
                    if let Some(Item::Text { text, .. } | Item::Thinking { text, .. }) = items.iter_mut().find(|i| i.id() == *id) {
                        text.push_str(delta);
                    }
                }
                RunEvent::AppendArgs { id, delta } => {
                    if let Some(Item::Tool(t)) = items.iter_mut().find(|i| i.id() == *id) {
                        t.args.push_str(delta);
                    }
                }
                RunEvent::Tool { id, status, output, .. } => {
                    if let Some(Item::Tool(t)) = items.iter_mut().find(|i| i.id() == *id) {
                        t.status = *status;
                        if let Some(o) = output {
                            t.output.clone_from(o);
                        }
                    }
                }
                RunEvent::Rollback { from } => items.retain(|i| i.id() < *from),
                RunEvent::Finished(end) => {
                    let end = end.clone();
                    all.push(ev);
                    return (items, end, all);
                }
                _ => {}
            }
            all.push(ev);
        }
        assert!(tokio::time::Instant::now() < deadline, "the run did not finish");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn texts(items: &[Item]) -> Vec<String> {
    items.iter().filter_map(|i| if let Item::Text { text, .. } = i { Some(text.clone()) } else { None }).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn text_streams_is_persisted_and_the_key_is_sent() {
    let mut events = vec![chat_text("Hel"), chat_text("lo")];
    events.extend(chat_done());
    let server = serve(vec![Reply::Sse(events)]).await;
    let r = rig(tokio::runtime::Handle::current());
    let s = spec(&server.addr, ProviderKind::OpenAiChat, false, &r.dir);
    let start = s.items.clone();
    let h = r.engine.start(s, "t");
    let (items, end, evs) = follow(&h, start).await;
    assert_eq!(end, RunEnd::Completed);
    assert_eq!(texts(&items), ["Hello"]);
    assert!(server.headers.lock()[0].to_lowercase().contains("authorization: bearer secret-key"));
    assert!(evs.iter().any(|e| matches!(e, RunEvent::Usage(u) if u.input == 100)));
    r.engine.store.flush();
    let saved = r.engine.store.load_items("c").unwrap();
    assert_eq!(texts(&saved), ["Hello"], "what was on screen is what was saved");
    assert!(saved.iter().any(|i| matches!(i, Item::Turn { .. })));
    std::fs::remove_dir_all(&r.dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_stream_restarts_cleanly_without_prefill_support() {
    let mut full = vec![chat_text("complete answer")];
    full.extend(chat_done());
    let server = serve(vec![Reply::Cut(vec![chat_text("partial")]), Reply::Sse(full)]).await;
    let r = rig(tokio::runtime::Handle::current());
    let s = spec(&server.addr, ProviderKind::OpenAiChat, false, &r.dir);
    let start = s.items.clone();
    let h = r.engine.start(s, "t");
    let (items, end, evs) = follow(&h, start).await;
    assert_eq!(end, RunEnd::Completed);
    assert_eq!(texts(&items), ["complete answer"], "the partial attempt must be gone, not doubled");
    assert!(evs.iter().any(|e| matches!(e, RunEvent::Rollback { .. })));
    assert!(evs.iter().any(|e| matches!(e, RunEvent::Status(RunStatus::Retrying { .. }))));
    std::fs::remove_dir_all(&r.dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_stream_resumes_from_the_prefill_when_the_server_can() {
    let mut rest = vec![chat_text(" world")];
    rest.extend(chat_done());
    let server = serve(vec![Reply::Cut(vec![chat_text("hello")]), Reply::Sse(rest)]).await;
    let r = rig(tokio::runtime::Handle::current());
    let s = spec(&server.addr, ProviderKind::OpenAiChat, true, &r.dir);
    let start = s.items.clone();
    let h = r.engine.start(s, "t");
    let (items, end, evs) = follow(&h, start).await;
    assert_eq!(end, RunEnd::Completed);
    assert_eq!(texts(&items), ["hello world"]);
    assert!(!evs.iter().any(|e| matches!(e, RunEvent::Rollback { .. })));
    let bodies = server.bodies.lock();
    let last = bodies[1]["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "assistant");
    assert_eq!(last["content"], "hello", "the retry must carry what was already delivered");
    std::fs::remove_dir_all(&r.dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_the_server_rejects_fails_at_once_with_its_message() {
    let server = serve(vec![Reply::Status(401, r#"{"error":{"message":"bad api key"}}"#.into())]).await;
    let r = rig(tokio::runtime::Handle::current());
    let s = spec(&server.addr, ProviderKind::OpenAiChat, false, &r.dir);
    let start = s.items.clone();
    let h = r.engine.start(s, "t");
    let (_, end, _) = follow(&h, start).await;
    match end {
        RunEnd::Failed(m) => assert!(m.contains("bad api key"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(server.requests.load(Ordering::SeqCst), 1, "retrying a 401 only gets the same answer");
    std::fs::remove_dir_all(&r.dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn overload_is_retried_until_it_clears() {
    let mut ok = vec![chat_text("fine")];
    ok.extend(chat_done());
    let server = serve(vec![Reply::Status(429, "slow down".into()), Reply::Status(503, "busy".into()), Reply::Sse(ok)]).await;
    let r = rig(tokio::runtime::Handle::current());
    let s = spec(&server.addr, ProviderKind::OpenAiChat, false, &r.dir);
    let start = s.items.clone();
    let h = r.engine.start(s, "t");
    let (items, end, _) = follow(&h, start).await;
    assert_eq!(end, RunEnd::Completed);
    assert_eq!(texts(&items), ["fine"]);
    assert_eq!(server.requests.load(Ordering::SeqCst), 3);
    std::fs::remove_dir_all(&r.dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_tool_gets_an_error_and_the_model_carries_on() {
    let call = vec![
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","function":{"name":"no_such_tool","arguments":"{}"}}]}}]}"#.to_owned(),
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned(),
        "[DONE]".to_owned(),
    ];
    let mut answer = vec![chat_text("sorry")];
    answer.extend(chat_done());
    let server = serve(vec![Reply::Sse(call), Reply::Sse(answer)]).await;
    let r = rig(tokio::runtime::Handle::current());
    let s = spec(&server.addr, ProviderKind::OpenAiChat, false, &r.dir);
    let start = s.items.clone();
    let h = r.engine.start(s, "t");
    let (items, end, _) = follow(&h, start).await;
    assert_eq!(end, RunEnd::Completed);
    let tool = items.iter().find_map(|i| if let Item::Tool(t) = i { Some(t.clone()) } else { None }).unwrap();
    assert_eq!(tool.status, ToolStatus::Error);
    let bodies = server.bodies.lock();
    let m = bodies[1]["messages"].as_array().unwrap();
    let result = m.iter().find(|x| x["role"] == "tool").unwrap();
    assert_eq!(result["tool_call_id"], "x");
    assert!(result["content"].as_str().unwrap().contains("no_such_tool"));
    std::fs::remove_dir_all(&r.dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_run_ends_it_promptly() {
    // A stream that delivers one token and then goes quiet.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut b = [0u8; 65536];
        let _ = sock.read(&mut b).await;
        let frame = format!("data: {}\n\n", chat_text("started"));
        let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{frame}\r\n", frame.len()).as_bytes()).await;
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let r = rig(tokio::runtime::Handle::current());
    let s = spec(&addr, ProviderKind::OpenAiChat, false, &r.dir);
    let start = s.items.clone();
    let h = r.engine.start(s, "t");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t0 = std::time::Instant::now();
    h.stop();
    let (items, end, _) = follow(&h, start).await;
    assert_eq!(end, RunEnd::Cancelled);
    assert!(t0.elapsed() < Duration::from_secs(1), "stop took {:?}", t0.elapsed());
    assert_eq!(texts(&items), ["started"], "what was shown stays");
    std::fs::remove_dir_all(&r.dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_end_to_end_with_thinking_handed_back() {
    let first: Vec<String> = [
        r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":1}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu","name":"no_such_tool"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
        r#"{"type":"message_stop"}"#,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let second: Vec<String> = [
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        r#"{"type":"message_stop"}"#,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let server = serve(vec![Reply::Sse(first), Reply::Sse(second)]).await;
    let r = rig(tokio::runtime::Handle::current());
    let s = spec(&server.addr, ProviderKind::Anthropic, false, &r.dir);
    let start = s.items.clone();
    let h = r.engine.start(s, "t");
    let (items, end, _) = follow(&h, start).await;
    assert_eq!(end, RunEnd::Completed);
    assert_eq!(texts(&items), ["done"]);
    let head = server.headers.lock()[0].to_lowercase();
    assert!(head.contains("x-api-key: secret-key") && head.contains("anthropic-version"));
    assert!(head.contains("post /v1/messages"), "{head}");
    let bodies = server.bodies.lock();
    let m = bodies[1]["messages"].as_array().unwrap();
    assert_eq!(m[1]["content"][0]["signature"], "SIG");
    assert_eq!(m[1]["content"][1]["input"]["a"], 1);
    assert_eq!(m[2]["content"][0]["tool_use_id"], "tu");
    std::fs::remove_dir_all(&r.dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn responses_end_to_end() {
    let events: Vec<String> = [
        r#"{"type":"response.output_text.delta","delta":"ok"}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":1}}}"#,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let server = serve(vec![Reply::Sse(events)]).await;
    let r = rig(tokio::runtime::Handle::current());
    let s = spec(&server.addr, ProviderKind::OpenAiResponses, false, &r.dir);
    let start = s.items.clone();
    let h = r.engine.start(s, "t");
    let (items, end, _) = follow(&h, start).await;
    assert_eq!(end, RunEnd::Completed);
    assert_eq!(texts(&items), ["ok"]);
    assert!(server.headers.lock()[0].to_lowercase().contains("post /v1/responses"));
    std::fs::remove_dir_all(&r.dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_context_is_compacted_and_requests_start_from_the_summary() {
    let usage_big = r#"{"choices":[],"usage":{"prompt_tokens":950,"completion_tokens":5}}"#.to_owned();
    let call = vec![
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","function":{"name":"no_such_tool","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#.to_owned(),
        usage_big,
        "[DONE]".to_owned(),
    ];
    let mut summary = vec![chat_text("the summary")];
    summary.extend(chat_done());
    let mut answer = vec![chat_text("carried on")];
    answer.extend(chat_done());
    let server = serve(vec![Reply::Sse(call), Reply::Sse(summary), Reply::Sse(answer)]).await;
    let r = rig(tokio::runtime::Handle::current());
    let s = spec(&server.addr, ProviderKind::OpenAiChat, false, &r.dir);
    let start = s.items.clone();
    let h = r.engine.start(s, "t");
    let (items, end, _) = follow(&h, start).await;
    assert_eq!(end, RunEnd::Completed);
    assert_eq!(texts(&items), ["carried on"], "the summary's own stream must not stay in the timeline");
    assert!(items.iter().any(|i| matches!(i, Item::Compaction { .. })));
    let bodies = server.bodies.lock();
    let last = bodies[2]["messages"].as_array().unwrap();
    let first = last.iter().find(|m| m["role"] != "system");
    assert!(first.and_then(|m| m["content"].as_str()).is_some_and(|c| c.contains("the summary")), "{last:?}");
    assert!(!last.iter().any(|m| m["role"] == "tool"), "what was summarised must not be resent");
    std::fs::remove_dir_all(&r.dir).ok();
}
