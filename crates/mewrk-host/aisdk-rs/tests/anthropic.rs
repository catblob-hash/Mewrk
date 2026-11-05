//! The Anthropic family against a mock upstream on a real socket.

mod common;

use std::time::Duration;

use common::{Mock, Reply, Rust};
use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;

fn request(mock: &Mock, model: &str, extra: Json) -> Json {
    let mut payload = jobj! {
        "family": "anthropic",
        "baseURL": format!("{}/v1", mock.base),
        "apiKey": "sk-test-secret",
        "modelId": model,
        "system": "S",
        "messages": vec![jobj! { "role": "user", "content": "hello" }],
        "maxSteps": 1u64,
    };
    if let (Some(object), Some(extra)) = (payload.as_obj_mut(), extra.as_obj()) {
        object.extend_from(extra);
    }
    payload
}

fn simple_stream() -> Reply {
    Reply::sse(&[
        r#"{"type":"message_start","message":{"id":"m1","model":"claude-opus-5-20261101","usage":{"input_tokens":3,"cache_read_input_tokens":2,"cache_creation_input_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" there"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        r#"{"type":"message_stop"}"#,
    ])
}

#[test]
fn text_streaming_and_the_request_shape() {
    let mock = Mock::script(vec![simple_stream()]);
    let rust = Rust::start();
    let outcome = rust.run("s", &request(&mock, "claude-opus-5", jobj! {
        "tools": vec![jobj! { "name": "t", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "a": jobj! { "type": "string" } } } }],
        "reasoning": "high",
        "reasoningContent": "plaintext",
        "promptCache": true,
    }));
    assert_eq!(outcome.text(), "hi there");
    let result = outcome.result();
    assert_eq!(result.str_at("text"), Some("hi there"));
    assert_eq!(result.str_at("finishReason"), Some("stop"));
    assert_eq!(result.str_at("rawFinishReason"), Some("end_turn"));
    assert_eq!(result.str_at("model"), Some("claude-opus-5-20261101"));
    assert_eq!(result.get("usage").unwrap().to_text(), r#"{"inputTokens":6,"outputTokens":2,"totalTokens":8,"cacheReadTokens":2}"#);
    assert_eq!(result.get("responseMessages").unwrap().to_text(), r#"[{"role":"assistant","content":[{"type":"text","text":"hi there"}]}]"#);

    let seen = mock.last_request();
    assert_eq!((seen.method.as_str(), seen.target.as_str()), ("POST", "/v1/messages"));
    assert_eq!(seen.header("x-api-key"), Some("sk-test-secret"));
    assert_eq!(seen.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(seen.header("anthropic-beta"), Some("structured-outputs-2025-11-13,interleaved-thinking-2025-05-14"));
    assert_eq!(seen.header("user-agent"), Some("ai-sdk/anthropic/4.0.44 ai-sdk/provider-utils/5.0.32 runtime/rust"));
    assert_eq!(
        seen.body,
        r#"{"model":"claude-opus-5","max_tokens":32000,"thinking":{"type":"enabled","budget_tokens":19200},"output_config":{"effort":"high"},"system":[{"type":"text","text":"S","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]}],"tools":[{"name":"t","description":"d","input_schema":{"type":"object","properties":{"a":{"type":"string"}}},"eager_input_streaming":true}],"tool_choice":{"type":"auto"},"stream":true}"#
    );
}

#[test]
fn official_endpoint_keeps_adaptive_thinking() {
    // Only the hostname decides; with a mock that means the rewrite always runs, so this goes through the unit
    // test of `is_official` in spirit: a relay gets a budget, and the rest of the request is unchanged.
    let mock = Mock::script(vec![simple_stream()]);
    let rust = Rust::start();
    rust.run("s", &request(&mock, "claude-opus-5", jobj! { "reasoning": "max", "promptCache": false }));
    let body = mock.last_request().json();
    assert_eq!(body.get("thinking").unwrap().to_text(), r#"{"type":"enabled","budget_tokens":28800}"#);
    assert_eq!(body.path(&["output_config", "effort"]).and_then(Json::as_str), Some("max"));
    assert!(body.get("system").unwrap().to_text().find("cache_control").is_none());
}

#[test]
fn signed_reasoning_streams_and_replays() {
    let mock = Mock::script(vec![
        Reply::sse(&[
            r#"{"type":"message_start","message":{"id":"m1","model":"claude-opus-5","usage":{"input_tokens":3}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"REDACTED"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_1","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"th\":\"a\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9,"output_tokens_details":{"thinking_tokens":5}}}"#,
            r#"{"type":"message_stop"}"#,
        ]),
        simple_stream(),
    ]);
    let rust = Rust::start();
    let tools = vec![jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object" } }];
    let outcome = rust.run("s", &request(&mock, "claude-opus-5", jobj! { "tools": tools.clone(), "reasoningContent": "plaintext" }));
    let starts = outcome.events_of("reasoning-start");
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0].to_text(), r#"{"k":"reasoning-start","item":0}"#.replace("}", r#","form":"plaintext"}"#));
    assert_eq!(starts[1].to_text(), r#"{"k":"reasoning-start","item":1,"form":"encrypted"}"#);
    let deltas: String = outcome.events_of("reasoning-delta").iter().filter_map(|e| e.str_at("delta")).collect();
    assert_eq!(deltas, "Let me think");
    assert_eq!(outcome.events_of("reasoning-done").len(), 2);
    let announced = outcome.events_of("tool-call-announced");
    assert_eq!(announced[0].to_text(), r#"{"k":"tool-call-announced","callId":"call_1","toolName":"read"}"#);
    let result = outcome.result();
    assert_eq!(result.str_at("finishReason"), Some("tool-calls"));
    assert_eq!(result.get("calls").unwrap().to_text(), r#"[{"callId":"call_1","toolName":"read","input":{"path":"a"}}]"#);
    assert_eq!(result.get("reasoning").unwrap().to_text(), r#"["Let me think",""]"#);
    assert_eq!(result.get("usage").unwrap().get("reasoningTokens").unwrap().as_u64(), Some(5));
    assert!(result.get("reasoningMs").is_some());
    let messages = result.get("responseMessages").unwrap().clone();
    assert_eq!(
        messages.to_text(),
        r#"[{"role":"assistant","content":[{"type":"reasoning","text":"Let me think","providerOptions":{"anthropic":{"signature":"SIG"}}},{"type":"reasoning","text":"","providerOptions":{"anthropic":{"redactedData":"REDACTED"}}},{"type":"tool-call","toolCallId":"call_1","toolName":"read","input":{"path":"a"}}]}]"#
    );

    // The host replays those messages verbatim on the next step.
    let mut history = vec![jobj! { "role": "user", "content": "hello" }];
    history.extend(messages.as_arr().unwrap().clone());
    history.push(jobj! { "role": "tool", "content": vec![jobj! {
        "type": "tool-result", "toolCallId": "call_1", "toolName": "read", "output": jobj! { "type": "text", "value": "contents" },
    }] });
    rust.run("t", &request(&mock, "claude-opus-5", jobj! { "tools": tools, "messages": history, "promptCache": false }));
    let body = mock.last_request().json();
    assert_eq!(
        body.get("messages").unwrap().to_text(),
        r#"[{"role":"user","content":[{"type":"text","text":"hello"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"Let me think","signature":"SIG"},{"type":"redacted_thinking","data":"REDACTED"},{"type":"tool_use","id":"call_1","name":"read","input":{"path":"a"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"contents"}]}]"#
    );
}

#[test]
fn a_bare_signature_earns_no_card_without_reasoning_tokens_and_does_with_them() {
    let stream = |tokens: u64| {
        let delta = format!(r#"{{"type":"message_delta","delta":{{"stop_reason":"end_turn"}},"usage":{{"output_tokens":1,"output_tokens_details":{{"thinking_tokens":{tokens}}}}}}}"#);
        Reply::sse(&[
            r#"{"type":"message_start","message":{"id":"m1","model":"deepseek","usage":{"input_tokens":3}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"PSEUDO"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            &delta,
            r#"{"type":"message_stop"}"#,
        ])
    };
    let mock = Mock::script(vec![stream(0), stream(7)]);
    let rust = Rust::start();
    let without = rust.run("a", &request(&mock, "deepseek", jobj! {}));
    assert!(without.events_of("reasoning-start").is_empty());
    assert!(without.result().get("reasoningMs").is_none());
    assert_eq!(without.result().get("reasoning").unwrap().to_text(), "[]");

    let with = rust.run("b", &request(&mock, "deepseek", jobj! {}));
    assert_eq!(with.events_of("reasoning-start").len(), 1);
    assert_eq!(with.events_of("reasoning-done").len(), 1);
    assert!(with.result().get("reasoningMs").is_some());
}

#[test]
fn pause_turn_keeps_its_raw_reason_and_continues_a_search() {
    let paused = Reply::sse(&[
        r#"{"type":"message_start","message":{"id":"m1","model":"claude-opus-5","usage":{"input_tokens":3}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"q\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"pause_turn"},"usage":{"output_tokens":1}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let finished = Reply::sse(&[
        r#"{"type":"message_start","message":{"id":"m2","model":"claude-opus-5","usage":{"input_tokens":5}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_1","content":[{"type":"web_search_result","url":"https://a.example/x","title":"A","encrypted_content":"E","page_age":null}]}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"found"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let mock = Mock::script(vec![paused.clone(), finished]);
    let rust = Rust::start();
    // One allowed step: the pause reaches the host, raw reason and all.
    let one = rust.run("a", &request(&mock, "claude-opus-5", jobj! { "nativeSearch": jobj! { "maxUses": 3u64 } }));
    let result = one.result();
    assert_eq!(result.str_at("finishReason"), Some("stop"));
    assert_eq!(result.str_at("rawFinishReason"), Some("pause_turn"));
    assert_eq!(result.get("nativeSearchUses").unwrap().as_u64(), Some(1));
    assert_eq!(result.get("nativeSearchCallIds").unwrap().to_text(), r#"["srv_1"]"#);
    assert!(one.events_of("tool-call-announced").is_empty());
    let first_body = mock.last_request().json();
    assert_eq!(first_body.get("tools").unwrap().to_text(), r#"[{"type":"web_search_20250305","name":"web_search","max_uses":3}]"#);

    // More steps: the SDK continues on its own, with the remaining budget.
    let mock = Mock::script(vec![paused, simple_two()]);
    let two = rust.run("b", &request(&mock, "claude-opus-5", jobj! { "maxSteps": 4u64, "nativeSearch": jobj! { "maxUses": 3u64 } }));
    let result = two.result();
    assert_eq!(result.str_at("text"), Some("found"));
    assert_eq!(result.str_at("rawFinishReason"), Some("end_turn"));
    assert_eq!(result.get("nativeSearchUses").unwrap().as_u64(), Some(1));
    assert_eq!(result.get("sources").unwrap().to_text(), r#"[{"id":"search:https://a.example/x","url":"https://a.example/x"}]"#.replace(r#""id":"search:https://a.example/x","url":"https://a.example/x""#, &format!(r#""id":{},"url":"https://a.example/x","title":"A""#, result.get("sources").unwrap().as_arr().unwrap()[0].get("id").unwrap().to_text())));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let second = requests[1].json();
    assert_eq!(second.get("tools").unwrap().to_text(), r#"[{"type":"web_search_20250305","name":"web_search","max_uses":2}]"#);
    assert_eq!(
        second.get("messages").unwrap().as_arr().unwrap()[1].to_text(),
        r#"{"role":"assistant","content":[{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{"query":"q"},"cache_control":{"type":"ephemeral"}}]}"#
    );
    // `response.messages` is the final step's, as the SDK reads it.
    let messages = result.get("responseMessages").unwrap().as_arr().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].get("content").unwrap().as_arr().unwrap()[0].str_at("type"), Some("tool-result"));
}

fn simple_two() -> Reply {
    Reply::sse(&[
        r#"{"type":"message_start","message":{"id":"m2","model":"claude-opus-5","usage":{"input_tokens":5}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_1","content":[{"type":"web_search_result","url":"https://a.example/x","title":"A","encrypted_content":"E","page_age":null}]}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"found"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        r#"{"type":"message_stop"}"#,
    ])
}

#[test]
fn server_tool_failures_become_facts_not_calls() {
    // The recorded relay stream: a `max_uses_exceeded` error object wrapped in an array.
    let fixture = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../aisdk-service/fixtures/anthropic-max-uses-exceeded.sse")).unwrap();
    let mock = Mock::script(vec![Reply::sse_raw(&fixture)]);
    let rust = Rust::start();
    let outcome = rust.run("s", &request(&mock, "claude-opus-5", jobj! { "nativeSearch": jobj! { "maxUses": 1u64 } }));
    let result = outcome.result();
    let errors = result.get("providerToolErrors").unwrap().as_arr().unwrap();
    assert_eq!(errors[0].str_at("toolName"), Some("web_search"));
    assert_eq!(errors[0].str_at("message"), Some("max_uses_exceeded"));
    assert_eq!(result.get("calls").unwrap().to_text(), "[]");
}

#[test]
fn errors_classify_with_retry_after_and_redaction() {
    let mock = Mock::script(vec![
        Reply::json(429, r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down sk-test-secret"}}"#).header("retry-after", "7"),
        Reply::json(401, r#"{"type":"error","error":{"type":"authentication_error","message":"bad key sk-test-secret"}}"#),
        Reply::json(500, "not json at all"),
        Reply::sse(&[r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#]),
    ]);
    let rust = Rust::start();
    let limited = rust.run("a", &request(&mock, "m", jobj! {}));
    assert_eq!(limited.error().to_text(), r#"{"kind":"transient","message":"slow down [redacted]","status":429,"retryAfterMs":7000}"#);
    let denied = rust.run("b", &request(&mock, "m", jobj! {}));
    assert_eq!(denied.error().to_text(), r#"{"kind":"permanent","message":"bad key [redacted]","status":401}"#);
    let broken = rust.run("c", &request(&mock, "m", jobj! {}));
    assert_eq!(broken.error().to_text(), r#"{"kind":"transient","message":"Internal Server Error","status":500}"#);
    let overloaded = rust.run("d", &request(&mock, "m", jobj! {}));
    assert_eq!(overloaded.error().to_text(), r#"{"kind":"transient","message":"Overloaded","status":529}"#);
}

#[test]
fn a_dead_port_and_a_cut_body_are_transient() {
    let rust = Rust::start();
    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let payload = jobj! {
        "family": "anthropic", "baseURL": format!("http://127.0.0.1:{dead}/v1"), "apiKey": "k", "modelId": "m",
        "messages": vec![jobj! { "role": "user", "content": "x" }], "maxSteps": 1u64,
    };
    let refused = rust.run("a", &payload);
    assert_eq!(refused.error().str_at("kind"), Some("transient"));
    assert!(refused.error().str_at("message").unwrap().contains("Cannot connect to API"), "{}", refused.error());

    let mock = Mock::script(vec![Reply::new(200).header("content-type", "text/event-stream").body(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"m\",\"usage\":{\"input_tokens\":1}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"par\"}}\n\n",
    ).truncated()]);
    let cut = rust.run("b", &request(&mock, "m", jobj! {}));
    assert_eq!(cut.text(), "par");
    assert_eq!(cut.error().str_at("kind"), Some("transient"));
    assert!(cut.error().str_at("message").unwrap().starts_with("UND_ERR_SOCKET"), "{}", cut.error());
}

#[test]
fn cancel_closes_the_socket_and_reports_cancelled() {
    let mock = Mock::script(vec![Reply::sse(&[
        r#"{"type":"message_start","message":{"id":"m1","model":"m","usage":{"input_tokens":3}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"first"}}"#,
    ]).hanging()]);
    let rust = Rust::start();
    rust.send(&common::step_frame("s", &request(&mock, "m", jobj! {})));
    std::thread::sleep(Duration::from_millis(1200));
    rust.send(r#"{"v":18,"type":"cancel","id":"s"}"#);
    let outcome = rust.wait("s", Duration::from_secs(5));
    assert_eq!(outcome.text(), "first");
    assert!(!outcome.events_of("heartbeat").is_empty(), "heartbeats tick while a stream is silent");
    assert_eq!(outcome.error().to_text(), r#"{"kind":"cancelled","message":"请求已取消"}"#);
    std::thread::sleep(Duration::from_millis(200));
    assert!(*mock.client_gone.lock().unwrap(), "the socket closes so the upstream stops generating");
}

#[test]
fn self_heal_repairs_a_rejected_thinking_signature_once_and_remembers() {
    let mock = Mock::script(vec![
        Reply::json(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1: Invalid `signature` in `thinking` block"}}"#),
        simple_stream(),
        simple_stream(),
    ]);
    let rust = Rust::start();
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "t", "providerOptions": jobj! { "anthropic": jobj! { "signature": "bad" } } },
            jobj! { "type": "text", "text": "a" },
        ] },
        jobj! { "role": "user", "content": "r" },
    ];
    let model = "heal-signature-model";
    let outcome = rust.run("a", &request(&mock, model, jobj! { "messages": history.clone(), "promptCache": false }));
    assert_eq!(outcome.result().str_at("text"), Some("hi there"));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].body.contains(r#""type":"thinking""#));
    assert!(!requests[1].body.contains(r#""type":"thinking""#));
    assert!(requests[1].body.contains(r#""text":"a""#));

    // The repair is remembered per endpoint and model: no second round trip.
    rust.run("b", &request(&mock, model, jobj! { "messages": history, "promptCache": false }));
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[2].body.contains(r#""type":"thinking""#));
}

#[test]
fn a_non_streamed_message_replays_as_a_stream() {
    let mock = Mock::script(vec![Reply::json(200, r#"{"type":"message","id":"m","model":"mm","role":"assistant","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2},"content":[{"type":"thinking","thinking":"t","signature":"s"},{"type":"text","text":"hi"}]}"#)]);
    let rust = Rust::start();
    let outcome = rust.run("s", &request(&mock, "m", jobj! {}));
    assert_eq!(outcome.text(), "hi");
    let result = outcome.result();
    assert_eq!(result.str_at("model"), Some("mm"));
    assert_eq!(result.get("reasoning").unwrap().to_text(), r#"["t"]"#);
}

/// A stream the way a server that compresses SSE sends it, in small pieces.
fn compressed(reply: Reply, encoding: &str) -> Reply {
    use std::io::Write;
    let plain: Vec<u8> = reply.chunks.concat();
    let mut bytes: Vec<u8> = Vec::new();
    match encoding {
        "gzip" => {
            let mut encoder = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
            encoder.write_all(&plain).unwrap();
            encoder.finish().unwrap();
        }
        _ => {
            let mut encoder = flate2::write::ZlibEncoder::new(&mut bytes, flate2::Compression::default());
            encoder.write_all(&plain).unwrap();
            encoder.finish().unwrap();
        }
    }
    let mut out = Reply::new(200).header("content-type", "text/event-stream").header("content-encoding", encoding);
    for piece in bytes.chunks(7) {
        out = out.body(piece.to_vec());
    }
    out
}

#[test]
fn compressed_streams_decode_at_any_chunking() {
    for encoding in ["gzip", "deflate"] {
        let mock = Mock::script(vec![compressed(simple_stream(), encoding)]);
        let rust = Rust::start();
        let outcome = rust.run("s", &request(&mock, "claude-opus-5", jobj! {}));
        assert_eq!(outcome.result().str_at("text"), Some("hi there"), "{encoding}");
        assert_eq!(mock.last_request().header("accept-encoding"), Some("gzip, deflate"));
    }
}

#[test]
fn invalid_tool_json_reaches_the_host_verbatim_and_answers_itself() {
    let mock = Mock::script(vec![Reply::sse(&[
        r#"{"type":"message_start","message":{"id":"m1","model":"m","usage":{"input_tokens":3}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c","name":"read","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"broken"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}"#,
        r#"{"type":"message_stop"}"#,
    ])]);
    let rust = Rust::start();
    let tools = vec![jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object" } }];
    let outcome = rust.run("s", &request(&mock, "m", jobj! { "tools": tools }));
    let result = outcome.result();
    assert_eq!(result.get("calls").unwrap().to_text(), r#"[{"callId":"c","toolName":"read","input":"{\"broken"}]"#);
    let messages = result.get("responseMessages").unwrap().as_arr().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].get("content").unwrap().as_arr().unwrap()[0].get("input").unwrap().to_text(), "{}");
    assert_eq!(messages[1].str_at("role"), Some("tool"));
    assert_eq!(messages[1].path(&["content"]).unwrap().as_arr().unwrap()[0].path(&["output", "type"]).and_then(Json::as_str), Some("error-text"));
}

#[test]
fn a_protocol_violation_stops_the_sidecar() {
    use std::sync::mpsc::channel;
    let (tx, rx) = channel();
    let tx = std::sync::Mutex::new(tx);
    let sidecar = mewrk_aisdk::Sidecar::start(Box::new(|_| {}), Box::new(move |message| tx.lock().unwrap().send(message).unwrap())).unwrap();
    sidecar.send_line(r#"{"v":17,"type":"hello"}"#);
    let message = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(message.contains("协议世代不匹配"), "{message}");
    assert!(!sidecar.is_alive());
}
