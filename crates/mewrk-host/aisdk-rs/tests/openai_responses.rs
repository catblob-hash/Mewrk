//! The Responses families against a mock upstream on a real socket, and the model itself where a frame cannot reach.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Mock, Reply, Rust};
use mewrk_aisdk::http::{BoxFuture, Cancel, Fetch, Headers, HttpRequest, HttpResponse};
use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;
use mewrk_aisdk::model::{CallOptions, DynModel};
use mewrk_aisdk::providers::{ProviderSettings, azure, openai_responses};

fn request(family: &str, mock: &Mock, path: &str, model: &str, extra: Json) -> Json {
    let key = if family == "azure" { "azure" } else { "openai" };
    let mut options = mewrk_aisdk::json::Obj::new();
    options.insert(key, jobj! { "store": false, "reasoningSummary": Json::Null, "include": vec![Json::from("reasoning.encrypted_content")], "promptCacheKey": "conv" });
    let mut payload = jobj! {
        "family": family,
        "baseURL": format!("{}{path}", mock.base),
        "apiKey": "sk-test-secret",
        "modelId": model,
        "system": "S",
        "messages": vec![jobj! { "role": "user", "content": "hello" }],
        "providerOptions": Json::Obj(options),
        "maxSteps": 1u64,
    };
    if let (Some(object), Some(extra)) = (payload.as_obj_mut(), extra.as_obj()) {
        object.extend_from(extra);
    }
    payload
}

fn responses(mock: &Mock, model: &str, extra: Json) -> Json {
    request("openai-responses", mock, "/v1", model, extra)
}

fn fixture(name: &str) -> Reply {
    let path = format!("{}/../aisdk-service/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    Reply::sse_raw(&std::fs::read_to_string(path).expect("fixture"))
}

const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1","created_at":1,"model":"gpt-5-real"}}"#;
const IN_PROGRESS: &str = r#"{"type":"response.in_progress","response":{"id":"r1","created_at":1,"model":"gpt-5-real"}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}}"#;
const MESSAGE_ADDED: &str = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m1"}}"#;
const TEXT_HI: &str = r#"{"type":"response.output_text.delta","item_id":"m1","output_index":0,"delta":"hi"}"#;
const TEXT_THERE: &str = r#"{"type":"response.output_text.delta","item_id":"m1","output_index":0,"delta":" there"}"#;
const MESSAGE_DONE: &str = r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"m1"}}"#;

fn text_stream() -> Reply {
    Reply::sse(&[CREATED, MESSAGE_ADDED, TEXT_HI, TEXT_THERE, MESSAGE_DONE, COMPLETED])
}

#[test]
fn text_streaming_and_the_request_shape() {
    let mock = Mock::script(vec![text_stream()]);
    let rust = Rust::start();
    let outcome = rust.run("s", &responses(&mock, "gpt-5", jobj! {
        "tools": vec![jobj! { "name": "t", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "a": jobj! { "type": "string" } } } }],
        "reasoning": "high",
        "reasoningContent": "plaintext",
        "maxOutputTokens": 100u64,
    }));
    assert_eq!(outcome.text(), "hi there");
    let result = outcome.result();
    assert_eq!(result.str_at("text"), Some("hi there"));
    assert_eq!(result.str_at("finishReason"), Some("stop"));
    assert_eq!(result.get("rawFinishReason"), None);
    assert_eq!(result.str_at("model"), Some("gpt-5-real"));
    assert_eq!(result.get("usage").unwrap().to_text(), r#"{"inputTokens":3,"outputTokens":2,"totalTokens":5,"reasoningTokens":0,"cacheReadTokens":0}"#);
    assert_eq!(
        result.get("responseMessages").unwrap().to_text(),
        r#"[{"role":"assistant","content":[{"type":"text","text":"hi there","providerOptions":{"openai":{"itemId":"m1"}}}]}]"#
    );
    let seen = mock.last_request();
    assert_eq!((seen.method.as_str(), seen.target.as_str()), ("POST", "/v1/responses"));
    assert_eq!(seen.header("authorization"), Some("Bearer sk-test-secret"));
    assert_eq!(seen.header("user-agent"), Some("ai-sdk/openai/4.0.50 ai-sdk/provider-utils/5.0.32 runtime/rust"));
    assert_eq!(
        seen.body,
        r#"{"model":"gpt-5","input":[{"role":"developer","content":"S"},{"role":"user","content":[{"type":"input_text","text":"hello"}]}],"max_output_tokens":100,"store":false,"include":["reasoning.encrypted_content"],"prompt_cache_key":"conv","reasoning":{"effort":"high"},"tools":[{"type":"function","name":"t","description":"d","parameters":{"type":"object","properties":{"a":{"type":"string"}}},"strict":false}],"tool_choice":"auto","stream":true}"#
    );
}

#[test]
fn the_system_prompt_role_follows_the_model() {
    for (model, role) in [("gpt-5", "developer"), ("gpt-5.1-codex", "developer"), ("o3", "developer"), ("gpt-4o", "system"), ("gpt-5-chat-latest", "system"), ("deepseek-r1", "system")] {
        let mock = Mock::script(vec![text_stream()]);
        Rust::start().run("s", &responses(&mock, model, jobj! {}));
        let body = mock.last_request().json();
        assert_eq!(body.get("input").unwrap().as_arr().unwrap()[0].str_at("role"), Some(role), "{model}");
    }
    let mock = Mock::script(vec![text_stream()]);
    Rust::start().run("s", &responses(&mock, "my-proxy", jobj! { "providerOptions": jobj! { "openai": jobj! { "forceReasoning": true, "store": false } } }));
    let body = mock.last_request().json();
    assert_eq!(body.get("input").unwrap().as_arr().unwrap()[0].str_at("role"), Some("developer"));
    assert_eq!(body.get("include").unwrap().to_text(), r#"["reasoning.encrypted_content"]"#);
}

#[test]
fn tool_calls_and_usage() {
    let mock = Mock::script(vec![Reply::sse(&[
        CREATED,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc1","call_id":"c1","name":"read","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc1","output_index":0,"delta":"{\"path\":"}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc1","output_index":0,"delta":"\"a\"}"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc1","call_id":"c1","name":"read","arguments":"{\"path\":\"a\"}","status":"completed"}}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4,"cache_write_tokens":1},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":2}}}}"#,
    ])]);
    let rust = Rust::start();
    let outcome = rust.run("s", &responses(&mock, "gpt-5", jobj! {
        "tools": vec![jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object" } }],
    }));
    assert_eq!(outcome.events_of("tool-call-announced")[0].to_text(), r#"{"k":"tool-call-announced","callId":"c1","toolName":"read"}"#);
    assert_eq!(outcome.events_of("tool-call")[0].to_text(), r#"{"k":"tool-call","callId":"c1","input":{"path":"a"}}"#);
    let result = outcome.result();
    assert_eq!(result.str_at("finishReason"), Some("tool-calls"));
    assert_eq!(result.get("calls").unwrap().to_text(), r#"[{"callId":"c1","toolName":"read","input":{"path":"a"}}]"#);
    assert_eq!(result.get("usage").unwrap().to_text(), r#"{"inputTokens":10,"outputTokens":7,"totalTokens":17,"reasoningTokens":2,"cacheReadTokens":4}"#);
    assert_eq!(
        result.get("responseMessages").unwrap().to_text(),
        r#"[{"role":"assistant","content":[{"type":"tool-call","toolCallId":"c1","toolName":"read","input":{"path":"a"},"providerOptions":{"openai":{"itemId":"fc1"}}}]}]"#
    );
}

#[test]
fn encrypted_reasoning_streams_and_replays() {
    let mock = Mock::script(vec![fixture("openai-responses-reasoning.sse"), text_stream()]);
    let rust = Rust::start();
    let first = responses(&mock, "responses-reasoning", jobj! { "reasoning": "high", "reasoningContent": "encrypted" });
    let outcome = rust.run("s", &first);
    let result = outcome.result();
    assert_eq!(result.get("usage").unwrap().to_text(), r#"{"inputTokens":31,"outputTokens":940,"totalTokens":971,"reasoningTokens":900,"cacheReadTokens":0}"#);
    assert_eq!(result.get("reasoning").unwrap().to_text(), r#"["先读 step.rs，再决定改哪里"]"#);
    let kinds: Vec<_> = outcome.events().iter().filter_map(|e| e.str_at("k")).filter(|k| k.starts_with("reasoning-")).map(str::to_owned).collect();
    assert_eq!(kinds, ["reasoning-start", "reasoning-delta", "reasoning-delta", "reasoning-done"]);
    assert_eq!(
        result.get("responseMessages").unwrap().to_text(),
        r#"[{"role":"assistant","content":[{"type":"reasoning","text":"先读 step.rs，再决定改哪里","providerOptions":{"openai":{"itemId":"rs_selfcheck_1","reasoningEncryptedContent":"ENCRYPTED_REASONING_SELFCHECK"}}},{"type":"text","text":"读完了","providerOptions":{"openai":{"itemId":"msg_selfcheck_1"}}}]}]"#
    );
    // What the turn persisted is what the next one sends: the id, the encrypted content and the summary.
    let mut history = vec![jobj! { "role": "user", "content": "hello" }];
    history.extend(result.get("responseMessages").unwrap().as_arr().unwrap().iter().cloned());
    history.push(jobj! { "role": "user", "content": "next" });
    rust.run("t", &responses(&mock, "responses-reasoning", jobj! { "messages": history, "reasoning": "high", "reasoningContent": "encrypted" }));
    let body = mock.last_request().json();
    assert_eq!(
        body.get("input").unwrap().to_text(),
        r#"[{"role":"system","content":"S"},{"role":"user","content":[{"type":"input_text","text":"hello"}]},{"type":"reasoning","id":"rs_selfcheck_1","encrypted_content":"ENCRYPTED_REASONING_SELFCHECK","summary":[{"type":"summary_text","text":"先读 step.rs，再决定改哪里"}]},{"role":"assistant","content":[{"type":"output_text","text":"读完了"}],"id":"msg_selfcheck_1"},{"role":"user","content":[{"type":"input_text","text":"next"}]}]"#
    );
}

#[test]
fn encrypted_only_reasoning_is_still_visible() {
    let mock = Mock::script(vec![fixture("openai-responses-encrypted-only.sse")]);
    let outcome = Rust::start().run("s", &responses(&mock, "responses-encrypted-only", jobj! { "reasoning": "high", "reasoningContent": "encrypted" }));
    let result = outcome.result();
    assert_eq!(result.get("reasoning").unwrap().to_text(), r#"[""]"#);
    assert!(result.get("reasoningMs").is_some_and(Json::is_num));
    assert_eq!(result.path(&["usage", "reasoningTokens"]).and_then(Json::as_u64), Some(512));
    assert!(!outcome.events_of("reasoning-start").is_empty());
    assert!(outcome.events_of("reasoning-delta").is_empty());
    assert!(result.get("responseMessages").unwrap().to_text().contains("ENCRYPTED_ONLY_SELFCHECK"));
}

fn summary_parts_stream() -> Reply {
    Reply::sse(&[
        CREATED,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs1","encrypted_content":null}}"#,
        r#"{"type":"response.reasoning_summary_part.added","item_id":"rs1","output_index":0,"summary_index":0}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs1","output_index":0,"summary_index":0,"delta":"one"}"#,
        r#"{"type":"response.reasoning_summary_part.done","item_id":"rs1","output_index":0,"summary_index":0}"#,
        r#"{"type":"response.reasoning_summary_part.added","item_id":"rs1","output_index":0,"summary_index":1}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs1","output_index":0,"summary_index":1,"delta":"two"}"#,
        r#"{"type":"response.reasoning_summary_part.done","item_id":"rs1","output_index":0,"summary_index":1}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs1","encrypted_content":"ENC"}}"#,
        COMPLETED,
    ])
}

#[test]
fn summary_parts_conclude_with_the_encrypted_content_on_the_last() {
    let mock = Mock::script(vec![summary_parts_stream()]);
    let outcome = Rust::start().run("s", &responses(&mock, "gpt-5", jobj! { "reasoning": "high", "reasoningContent": "plaintext" }));
    let result = outcome.result();
    assert_eq!(result.get("reasoning").unwrap().to_text(), r#"["one","two"]"#);
    assert_eq!(
        result.get("responseMessages").unwrap().to_text(),
        r#"[{"role":"assistant","content":[{"type":"reasoning","text":"one","providerOptions":{"openai":{"itemId":"rs1"}}},{"type":"reasoning","text":"two","providerOptions":{"openai":{"itemId":"rs1","reasoningEncryptedContent":"ENC"}}}]}]"#
    );
    let events: Vec<String> = outcome.events().iter().filter(|e| e.str_at("k").is_some_and(|k| k.starts_with("reasoning-"))).map(|e| format!("{}{}", e.str_at("k").unwrap(), e.get("item").unwrap())).collect();
    assert_eq!(events, ["reasoning-start0", "reasoning-delta0", "reasoning-done0", "reasoning-start1", "reasoning-delta1", "reasoning-done1"]);
    // Replayed, the two parts are one item holding both summaries and the content of the last.
    let mut history = vec![jobj! { "role": "user", "content": "hello" }];
    history.extend(result.get("responseMessages").unwrap().as_arr().unwrap().iter().cloned());
    let mock = Mock::script(vec![text_stream()]);
    Rust::start().run("t", &responses(&mock, "gpt-5", jobj! { "messages": history }));
    let body = mock.last_request().json();
    let item = body.get("input").unwrap().as_arr().unwrap().iter().find(|i| i.str_at("type") == Some("reasoning")).unwrap().clone();
    assert_eq!(item.to_text(), r#"{"type":"reasoning","id":"rs1","encrypted_content":"ENC","summary":[{"type":"summary_text","text":"one"},{"type":"summary_text","text":"two"}]}"#);
}

#[test]
fn a_stored_response_concludes_summary_parts_at_once_and_replays_by_reference() {
    let mock = Mock::script(vec![summary_parts_stream()]);
    let stored = jobj! { "providerOptions": jobj! { "openai": jobj! { "store": true } }, "reasoning": "high", "reasoningContent": "plaintext" };
    let outcome = Rust::start().run("s", &responses(&mock, "gpt-5", stored));
    let result = outcome.result();
    // The first part ended with its own done event; the item's end only closes the part still open, and there is none.
    assert!(!result.get("responseMessages").unwrap().to_text().contains("ENC"));
    let mut history = vec![jobj! { "role": "user", "content": "hello" }];
    history.extend(result.get("responseMessages").unwrap().as_arr().unwrap().iter().cloned());
    let mock = Mock::script(vec![text_stream()]);
    Rust::start().run("t", &responses(&mock, "gpt-5", jobj! { "messages": history, "providerOptions": jobj! { "openai": jobj! { "store": true } } }));
    let body = mock.last_request().json();
    assert_eq!(body.get("input").unwrap().as_arr().unwrap()[2].to_text(), r#"{"type":"item_reference","id":"rs1"}"#);
    assert_eq!(body.get("input").unwrap().as_arr().unwrap().len(), 3);
}

#[test]
fn reasoning_without_encrypted_content_is_dropped_when_not_stored() {
    let mock = Mock::script(vec![text_stream()]);
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "plain", "providerOptions": jobj! { "openai": jobj! { "itemId": "a" } } },
            jobj! { "type": "reasoning", "text": "foreign" },
            jobj! { "type": "reasoning", "text": "no id", "providerOptions": jobj! { "openai": jobj! { "reasoningEncryptedContent": "E" } } },
        ] },
        jobj! { "role": "user", "content": "r" },
    ];
    Rust::start().run("s", &responses(&mock, "gpt-4o", jobj! { "messages": history }));
    let body = mock.last_request().json();
    let items: Vec<String> = body.get("input").unwrap().as_arr().unwrap().iter().filter(|i| i.str_at("type") == Some("reasoning")).map(Json::to_text).collect();
    assert_eq!(items, [r#"{"type":"reasoning","encrypted_content":"E","summary":[{"type":"summary_text","text":"no id"}]}"#]);
}

#[test]
fn finish_reasons() {
    let run = |event: &str| {
        let mock = Mock::script(vec![Reply::sse(&[CREATED, MESSAGE_ADDED, TEXT_HI, MESSAGE_DONE, event])]);
        let outcome = Rust::start().run("s", &responses(&mock, "gpt-5", jobj! {}));
        let result = outcome.result();
        (result.str_at("finishReason").map(str::to_owned), result.str_at("rawFinishReason").map(str::to_owned))
    };
    let some = |a: &str, b: &str| (Some(a.to_owned()), Some(b.to_owned()));
    assert_eq!(run(COMPLETED), (Some("stop".to_owned()), None));
    assert_eq!(run(r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":1}}}"#), some("length", "max_output_tokens"));
    assert_eq!(run(r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"usage":{"input_tokens":1,"output_tokens":1}}}"#), some("content-filter", "content_filter"));
    assert_eq!(run(r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"x"},"usage":{"input_tokens":1,"output_tokens":1}}}"#), some("other", "x"));
    assert_eq!(run(r#"{"type":"response.failed","sequence_number":1,"response":{}}"#), some("error", "error"));
    assert_eq!(run(r#"{"type":"response.failed","sequence_number":1,"response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#), some("length", "max_output_tokens"));
}

#[test]
fn http_errors_are_classified_with_retry_after() {
    let run = |reply: Reply| {
        let mock = Mock::script(vec![reply]);
        Rust::start().run("s", &responses(&mock, "gpt-5", jobj! {})).error().to_text()
    };
    assert_eq!(
        run(Reply::json(429, r#"{"error":{"message":"slow","type":"rate_limit","code":"rate_limit_exceeded"}}"#).header("retry-after", "7")),
        r#"{"kind":"transient","message":"slow","status":429,"retryAfterMs":7000}"#
    );
    assert_eq!(run(Reply::json(401, r#"{"error":{"message":"bad key","code":401}}"#)), r#"{"kind":"permanent","message":"bad key","status":401}"#);
    assert_eq!(run(Reply::json(400, r#"{"detail":"nope"}"#)), r#"{"kind":"permanent","message":"Bad Request","status":400}"#);
    assert_eq!(run(Reply::new(503)), r#"{"kind":"transient","message":"Service Unavailable","status":503}"#);
}

#[test]
fn an_error_before_any_output_is_a_failed_call_and_after_it_a_stream_error() {
    let run = |reply: Reply| {
        let mock = Mock::script(vec![reply]);
        Rust::start().run("s", &responses(&mock, "gpt-5", jobj! {})).error().to_text()
    };
    let failed = r#"{"type":"response.failed","sequence_number":1,"response":{"error":{"code":"rate_limit_exceeded","message":"slow down"}}}"#;
    assert_eq!(run(Reply::sse(&[CREATED, failed]).header("retry-after", "2")), r#"{"kind":"transient","message":"slow down","status":429,"retryAfterMs":2000}"#);
    assert_eq!(run(Reply::sse(&[CREATED, MESSAGE_ADDED, failed]).header("retry-after", "2")), r#"{"kind":"transient","message":"slow down","status":429}"#);
    let quota = r#"{"type":"error","sequence_number":1,"error":{"type":"insufficient_quota","code":"insufficient_quota","message":"pay"}}"#;
    assert_eq!(run(Reply::sse(&[quota])), r#"{"kind":"permanent","message":"pay","status":429}"#);
    assert_eq!(run(Reply::sse(&[CREATED, MESSAGE_ADDED, quota])), r#"{"kind":"permanent","message":"pay","status":429}"#);
    let flat = r#"{"type":"error","sequence_number":1,"code":"server_error","message":"boom"}"#;
    assert_eq!(run(Reply::sse(&[flat])), r#"{"kind":"transient","message":"boom","status":500}"#);
    let invalid = r#"{"type":"error","sequence_number":1,"code":"invalid_prompt","message":"no"}"#;
    assert_eq!(run(Reply::sse(&[CREATED, MESSAGE_ADDED, invalid])), r#"{"kind":"permanent","message":"no","status":400}"#);
}

#[test]
fn the_grace_after_in_progress_lets_a_flushed_error_through_and_holds_nothing_back() {
    let quota = r#"{"type":"error","sequence_number":1,"code":"insufficient_quota","message":"pay"}"#;
    // Flushed with the accepted chunk: still a failed call, so the response's headers count.
    let mut flushed = Reply::new(200).header("content-type", "text/event-stream").header("retry-after", "3");
    flushed.chunks.push(format!("data: {IN_PROGRESS}\n\ndata: {quota}\n\n").into_bytes());
    let mock = Mock::script(vec![flushed]);
    assert_eq!(Rust::start().run("s", &responses(&mock, "gpt-5", jobj! {})).error().to_text(), r#"{"kind":"permanent","message":"pay","status":429,"retryAfterMs":3000}"#);
    // Later than the grace: a stream error.
    let mock = Mock::script(vec![Reply::sse(&[IN_PROGRESS, quota]).header("retry-after", "3").pause(Duration::from_millis(250))]);
    assert_eq!(Rust::start().run("s", &responses(&mock, "gpt-5", jobj! {})).error().to_text(), r#"{"kind":"permanent","message":"pay","status":429}"#);
    // A read the grace gave up on must not lose what it was about to deliver.
    let mock = Mock::script(vec![Reply::sse(&[CREATED, IN_PROGRESS, MESSAGE_ADDED, TEXT_HI, TEXT_THERE, MESSAGE_DONE, COMPLETED]).pause(Duration::from_millis(120))]);
    let outcome = Rust::start().run("s", &responses(&mock, "gpt-5", jobj! {}));
    assert_eq!(outcome.text(), "hi there");
    assert_eq!(outcome.result().str_at("model"), Some("gpt-5-real"));
}

#[test]
fn cancelling_a_stream_that_went_quiet_after_in_progress_closes_the_socket() {
    let mock = Mock::script(vec![Reply::sse(&[CREATED, IN_PROGRESS]).hanging()]);
    let rust = Rust::start();
    rust.send(&common::step_frame("s", &responses(&mock, "gpt-5", jobj! {})));
    // Past the grace, so the abandoned read is what the cancel has to reach.
    std::thread::sleep(Duration::from_millis(200));
    rust.send(r#"{"v":18,"type":"cancel","id":"s"}"#);
    let outcome = rust.wait("s", Duration::from_secs(10));
    assert_eq!(outcome.error().to_text(), r#"{"kind":"cancelled","message":"请求已取消"}"#);
    for _ in 0..100 {
        if *mock.client_gone.lock().unwrap() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the socket stayed open");
}

#[test]
fn cancelling_while_the_stream_has_shown_nothing_yet_closes_the_socket() {
    let mock = Mock::script(vec![Reply::sse(&[CREATED]).hanging()]);
    let rust = Rust::start();
    rust.send(&common::step_frame("s", &responses(&mock, "gpt-5", jobj! {})));
    std::thread::sleep(Duration::from_millis(100));
    rust.send(r#"{"v":18,"type":"cancel","id":"s"}"#);
    let outcome = rust.wait("s", Duration::from_secs(10));
    assert_eq!(outcome.error().to_text(), r#"{"kind":"cancelled","message":"请求已取消"}"#);
    for _ in 0..100 {
        if *mock.client_gone.lock().unwrap() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the socket stayed open");
}

#[test]
fn replaying_what_a_plaintext_proxy_returned_keeps_the_encrypted_content() {
    // `selfcheck.mjs`'s two-round replay: `reasoningContent: plaintext` changes the presentation, never the replay data.
    let options = jobj! { "openai": jobj! {
        "store": false,
        "reasoningSummary": Json::Null,
        "include": vec![Json::from("reasoning.encrypted_content"), Json::from("web_search_call.results")],
        "promptCacheKey": "replay",
    } };
    let rust = Rust::start();
    let mut history = vec![jobj! { "role": "user", "content": "Read and continue" }];
    for round in 0..2 {
        let mock = Mock::script(vec![fixture("openai-responses-reasoning.sse")]);
        let outcome = rust.run(&format!("r{round}"), &responses(&mock, "proxy-plaintext-replay", jobj! {
            "system": Json::Null,
            "reasoning": "high",
            "reasoningContent": "plaintext",
            "messages": history.clone(),
            "providerOptions": options.clone(),
        }));
        let result = outcome.result();
        let body = mock.last_request().json();
        assert_eq!(body.get("include").unwrap().to_text(), r#"["reasoning.encrypted_content","web_search_call.results"]"#);
        assert_eq!(body.get("store").and_then(Json::as_bool), Some(false));
        assert_eq!(body.str_at("prompt_cache_key"), Some("replay"));
        assert!(result.get("responseMessages").unwrap().to_text().contains("ENCRYPTED_REASONING_SELFCHECK"));
        assert_eq!(round > 0, body.get("input").unwrap().to_text().contains("ENCRYPTED_REASONING_SELFCHECK"));
        history.extend(result.get("responseMessages").unwrap().as_arr().unwrap().iter().cloned());
        history.push(jobj! { "role": "user", "content": "Continue" });
    }
}

#[test]
fn the_host_tag_on_reasoning_never_reaches_the_wire() {
    let mock = Mock::script(vec![text_stream()]);
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "r", "providerOptions": jobj! { "openai": jobj! { "itemId": "rs", "reasoningEncryptedContent": "E" }, "mewrk": jobj! { "model": "other" } } },
            jobj! { "type": "text", "text": "t" },
        ] },
        jobj! { "role": "user", "content": "next" },
    ];
    Rust::start().run("s", &responses(&mock, "gpt-5", jobj! { "messages": history }));
    let seen = mock.last_request();
    assert!(!seen.body.contains("mewrk"));
    assert!(seen.body.contains(r#"{"type":"reasoning","id":"rs","encrypted_content":"E","summary":[{"type":"summary_text","text":"r"}]}"#));
}

#[test]
fn chunks_failing_the_schema_are_validation_errors() {
    let run = |event: &str| {
        let mock = Mock::script(vec![Reply::sse(&[event])]);
        Rust::start().run("s", &responses(&mock, "gpt-5", jobj! {})).error().clone()
    };
    for event in [
        // The Codex backend's failed event as `selfcheck.mjs` writes it: without `sequence_number` the SDK's schema refuses it.
        r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"rate_limit_exceeded","message":"slow down"}}}"#,
        r#"{"type":"error","message":"m"}"#,
        r#"{"type":"response.completed","response":{}}"#,
        r#"{"type":"response.created","response":{"id":"r","created_at":1}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.output_text.annotation.added","annotation":{"type":"new_kind"}}"#,
    ] {
        let error = run(event);
        assert_eq!(error.str_at("kind"), Some("permanent"), "{event}");
        assert!(error.str_at("message").unwrap().starts_with(&format!("Type validation failed: Value: {event}.")), "{error}");
    }
    let chat = run(r#"{"id":"c","choices":[{"delta":{"content":"x"}}]}"#);
    assert_eq!(chat.str_at("kind"), Some("permanent"));
    assert!(chat.str_at("message").unwrap().starts_with("Received a Chat Completions stream while using the OpenAI Responses API."));
    assert!(chat.get("status").is_none());
    // A kind the SDK does not model is not a failure.
    let mock = Mock::script(vec![Reply::sse(&[r#"{"type":"response.brand_new"}"#, r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"brand_new"}}"#, CREATED, COMPLETED])]);
    assert_eq!(Rust::start().run("s", &responses(&mock, "gpt-5", jobj! {})).result().str_at("finishReason"), Some("stop"));
}

#[test]
fn provider_options_on_the_wire() {
    let run = |model: &str, options: Json| {
        let mock = Mock::script(vec![text_stream()]);
        let outcome = Rust::start().run("s", &responses(&mock, model, jobj! { "providerOptions": jobj! { "openai": options } }));
        outcome.result();
        mock.last_request().json()
    };
    let body = run("gpt-5", jobj! { "serviceTier": "flex", "previousResponseId": "resp_0", "store": false });
    assert_eq!(body.str_at("service_tier"), Some("flex"));
    assert_eq!(body.str_at("previous_response_id"), Some("resp_0"));
    assert!(run("gpt-4o", jobj! { "serviceTier": "flex" }).get("service_tier").is_none());
    assert!(run("gpt-5-nano", jobj! { "serviceTier": "priority" }).get("service_tier").is_none());
    assert_eq!(run("gpt-4o", jobj! { "serviceTier": "priority" }).str_at("service_tier"), Some("priority"));
    assert_eq!(run("gpt-5", jobj! { "reasoningSummary": Json::Null, "reasoningEffort": "low" }).get("reasoning").unwrap().to_text(), r#"{"effort":"low"}"#);
    assert_eq!(run("gpt-5", jobj! { "reasoningEffort": "low" }).get("reasoning").unwrap().to_text(), r#"{"effort":"low","summary":"detailed"}"#);
    assert_eq!(run("gpt-5", jobj! { "reasoningEffort": "none" }).get("reasoning").unwrap().to_text(), r#"{"effort":"none"}"#);
    assert!(run("gpt-4o", jobj! { "reasoningEffort": "low" }).get("reasoning").is_none());
    assert_eq!(run("gpt-5", jobj! { "logprobs": true }).get("top_logprobs").and_then(Json::as_u64), Some(20));
    assert_eq!(run("gpt-5", jobj! { "promptCacheOptions": jobj! { "ttl": "30m", "mode": "explicit", "x": 1u64 } }).get("prompt_cache_options").unwrap().to_text(), r#"{"mode":"explicit","ttl":"30m"}"#);

    let mock = Mock::script(vec![text_stream()]);
    let error = Rust::start().run("s", &responses(&mock, "gpt-5", jobj! { "providerOptions": jobj! { "openai": jobj! { "serviceTier": "nope" } } })).error().clone();
    assert_eq!(error.to_text(), r#"{"kind":"permanent","message":"invalid openai provider options"}"#);
    assert!(mock.requests().is_empty());
}

#[test]
fn replay_under_a_previous_response_skips_what_the_chain_holds() {
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "t", "providerOptions": jobj! { "openai": jobj! { "itemId": "rs", "reasoningEncryptedContent": "E" } } },
            jobj! { "type": "text", "text": "a", "providerOptions": jobj! { "openai": jobj! { "itemId": "m" } } },
            jobj! { "type": "tool-call", "toolCallId": "c", "toolName": "read", "input": jobj! {}, "providerOptions": jobj! { "openai": jobj! { "itemId": "f" } } },
        ] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c", "toolName": "read", "output": jobj! { "type": "text", "value": "v" } }] },
    ];
    let mock = Mock::script(vec![text_stream()]);
    Rust::start().run("s", &responses(&mock, "gpt-5", jobj! {
        "system": Json::Null,
        "messages": history.clone(),
        "providerOptions": jobj! { "openai": jobj! { "store": true, "previousResponseId": "resp_0" } },
    }));
    assert_eq!(
        mock.last_request().json().get("input").unwrap().to_text(),
        r#"[{"role":"user","content":[{"type":"input_text","text":"q"}]},{"type":"item_reference","id":"m"},{"type":"function_call","call_id":"c","name":"read","arguments":"{}"},{"type":"function_call_output","call_id":"c","output":"v"}]"#
    );
    let mock = Mock::script(vec![text_stream()]);
    Rust::start().run("s", &responses(&mock, "gpt-5", jobj! {
        "system": Json::Null,
        "messages": history,
        "providerOptions": jobj! { "openai": jobj! { "store": true, "conversation": "conv_1" } },
    }));
    assert_eq!(
        mock.last_request().json().get("input").unwrap().to_text(),
        r#"[{"role":"user","content":[{"type":"input_text","text":"q"}]},{"type":"function_call_output","call_id":"c","output":"v"}]"#
    );
}

#[test]
fn native_search_reports_sources_and_filters_commentary() {
    let mock = Mock::script(vec![Reply::sse(&[
        CREATED,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws1","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws1","status":"completed","action":{"type":"search","query":"q","sources":[{"type":"url","url":"https://a.example/x"},{"type":"api","name":"n"},{"type":"url","url":"file:///no"}]}}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"web_search_call","id":"ws2","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"web_search_call","id":"ws2","status":"completed","action":{"type":"open_page","url":"https://b.example/"}}}"#,
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"web_search_call","id":"ws3","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"web_search_call","id":"ws3","status":"completed","action":{"type":"find_in_page","url":"https://c.example/","pattern":"p"}}}"#,
        r#"{"type":"response.output_item.added","output_index":3,"item":{"type":"message","id":"m0","phase":"commentary"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"m0","output_index":3,"delta":"PROCESS"}"#,
        r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"message","id":"m0","phase":"commentary"}}"#,
        r#"{"type":"response.output_item.added","output_index":4,"item":{"type":"message","id":"m1"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"m1","output_index":4,"delta":"ANSWER"}"#,
        r#"{"type":"response.output_text.annotation.added","output_index":4,"item_id":"m1","annotation":{"type":"url_citation","url":"https://d.example/","title":"D","start_index":0,"end_index":6}}"#,
        r#"{"type":"response.output_item.done","output_index":4,"item":{"type":"message","id":"m1","phase":"final_answer"}}"#,
        COMPLETED,
    ])]);
    let outcome = Rust::start().run("s", &responses(&mock, "gpt-5", jobj! { "nativeSearch": jobj! { "maxUses": 5u64 } }));
    let body = mock.last_request().json();
    assert_eq!(body.get("tools").unwrap().to_text(), r#"[{"type":"web_search"}]"#);
    assert_eq!(body.get("include").unwrap().to_text(), r#"["reasoning.encrypted_content","web_search_call.action.sources"]"#);
    let result = outcome.result();
    assert_eq!(result.str_at("text"), Some("ANSWER"));
    assert_eq!(result.get("calls").unwrap().to_text(), "[]");
    assert_eq!(result.get("nativeSearchUses").and_then(Json::as_u64), Some(3));
    let urls: Vec<&str> = result.get("sources").unwrap().as_arr().unwrap().iter().filter_map(|s| s.str_at("url")).collect();
    assert_eq!(urls, ["https://a.example/x", "https://b.example/", "https://c.example/", "https://d.example/"]);
    let messages = result.get("responseMessages").unwrap().to_text();
    assert!(messages.contains(r#""output":{"type":"json","value":{"action":{"type":"openPage","url":"https://b.example/"}}}"#), "{messages}");
    assert!(messages.contains(r#""providerOptions":{"openai":{"itemId":"m0","phase":"commentary"}}"#), "{messages}");
    assert!(messages.contains(r#""phase":"final_answer","annotations":[{"type":"url_citation","start_index":0,"end_index":6,"url":"https://d.example/","title":"D"}]"#), "{messages}");
}

#[test]
fn compaction_items_round_trip() {
    let mock = Mock::script(vec![
        Reply::sse(&[
            CREATED,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"compaction","id":"cp1"}}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","id":"cp1","encrypted_content":"CP"}}"#,
            COMPLETED,
        ]),
        text_stream(),
    ]);
    let rust = Rust::start();
    let options = jobj! { "providerOptions": jobj! { "openai": jobj! { "store": false, "compactionTrigger": true } } };
    let result = rust.run("s", &responses(&mock, "gpt-5", options));
    let body = mock.last_request().json();
    assert_eq!(body.get("input").unwrap().as_arr().unwrap().last().unwrap().to_text(), r#"{"type":"compaction_trigger"}"#);
    let messages = result.result().get("responseMessages").unwrap().clone();
    assert_eq!(
        messages.to_text(),
        r#"[{"role":"assistant","content":[{"type":"custom","kind":"openai.compaction","providerOptions":{"openai":{"type":"compaction","itemId":"cp1","encryptedContent":"CP"}}}]}]"#
    );
    let mut history = messages.as_arr().unwrap().clone();
    history.push(jobj! { "role": "user", "content": "next" });
    rust.run("t", &responses(&mock, "gpt-5", jobj! { "system": Json::Null, "messages": history }));
    assert_eq!(
        mock.last_request().json().get("input").unwrap().to_text(),
        r#"[{"type":"compaction","id":"cp1","encrypted_content":"CP"},{"role":"user","content":[{"type":"input_text","text":"next"}]}]"#
    );
}

#[test]
fn codex_transport_and_dialect() {
    let mut no_content_type = Reply::new(200);
    no_content_type.chunks = text_stream().chunks;
    let mock = Mock::script(vec![no_content_type]);
    let frame = request("openai-codex", &mock, "/backend-api/codex", "gpt-5", jobj! {
        "maxOutputTokens": 64u64,
        "providerOptions": jobj! { "openai": jobj! { "store": false } },
        "headers": jobj! { "chatgpt-account-id": "acct", "originator": "mewrk" },
    });
    assert_eq!(Rust::start().run("s", &frame).result().str_at("text"), Some("hi there"));
    let seen = mock.last_request();
    assert_eq!(seen.target, "/backend-api/codex/responses");
    assert_eq!(seen.header("authorization"), Some("Bearer sk-test-secret"));
    assert_eq!(seen.header("chatgpt-account-id"), Some("acct"));
    assert_eq!(seen.header("originator"), Some("mewrk"));
    let body = seen.json();
    assert_eq!(body.get("stream").and_then(Json::as_bool), Some(true));
    assert_eq!(body.get("store").and_then(Json::as_bool), Some(false));
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("include").unwrap().to_text().contains("reasoning.encrypted_content"));

    let mock = Mock::script(vec![Reply::json(400, r#"{"detail":"Stream must be set to true"}"#)]);
    let frame = request("openai-codex", &mock, "/backend-api/codex", "gpt-5", jobj! {});
    assert_eq!(Rust::start().run("s", &frame).error().str_at("kind"), Some("permanent"));

    let mut failed = Reply::new(200);
    failed.chunks = vec![br#"data: {"type":"response.failed","sequence_number":1,"response":{"error":{"code":"rate_limit_exceeded","message":"slow down"}}}

"#.to_vec()];
    let mock = Mock::script(vec![failed]);
    let frame = request("openai-codex", &mock, "/backend-api/codex", "gpt-5", jobj! {});
    assert_eq!(Rust::start().run("s", &frame).error().to_text(), r#"{"kind":"transient","message":"slow down","status":429}"#);
}

#[test]
fn azure_headers_options_and_metadata() {
    let mock = Mock::script(vec![text_stream()]);
    let frame = request("azure", &mock, "/gw/", "my-deployment", jobj! { "settings": jobj! { "apiVersion": "2025-04-01-preview" }, "reasoning": "max" });
    let outcome = Rust::start().run("s", &frame);
    let seen = mock.last_request();
    // A gateway owns Azure's routing and versioning.
    assert_eq!(seen.target, "/gw/responses");
    assert_eq!(seen.header("api-key"), Some("sk-test-secret"));
    assert_eq!(seen.header("authorization"), None);
    assert_eq!(seen.header("user-agent"), Some("ai-sdk/azure/4.0.52 ai-sdk/provider-utils/5.0.32 runtime/rust"));
    assert_eq!(
        outcome.result().get("responseMessages").unwrap().to_text(),
        r#"[{"role":"assistant","content":[{"type":"text","text":"hi there","providerOptions":{"azure":{"itemId":"m1"}}}]}]"#
    );
    // `openai` options are the fallback only when there are no `azure` ones.
    let mock = Mock::script(vec![text_stream()]);
    let frame = request("azure", &mock, "/gw", "gpt-5", jobj! { "providerOptions": jobj! { "openai": jobj! { "store": false, "user": "u" } } });
    Rust::start().run("s", &frame);
    assert_eq!(mock.last_request().json().str_at("user"), Some("u"));
    let mock = Mock::script(vec![text_stream()]);
    let frame = request("azure", &mock, "/gw", "gpt-5", jobj! { "providerOptions": jobj! { "azure": jobj! { "user": "a" }, "openai": jobj! { "user": "o" } } });
    Rust::start().run("s", &frame);
    assert_eq!(mock.last_request().json().str_at("user"), Some("a"));
    // No base URL is no resource name.
    let mock = Mock::script(vec![text_stream()]);
    let mut frame = request("azure", &mock, "", "gpt-5", jobj! {});
    frame.as_obj_mut().unwrap().remove("baseURL");
    assert_eq!(
        Rust::start().run("s", &frame).error().str_at("message"),
        Some("Azure OpenAI resource name setting is missing. Pass it using the 'resourceName' parameter or the AZURE_RESOURCE_NAME environment variable.")
    );
}

/// Records the request and answers with an empty stream, for what a mock on a socket cannot show.
struct Capture(Mutex<Vec<HttpRequest>>);

impl Fetch for Capture {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, mewrk_aisdk::error::AiError>> {
        self.0.lock().unwrap().push(request);
        Box::pin(async { Ok(HttpResponse::with_text(200, "OK", Headers::new(), "data: [DONE]\n\n")) })
    }
}

fn settings(base_url: Option<&str>, capture: &Arc<Capture>, api_version: Option<&str>) -> ProviderSettings {
    let mut named = std::collections::HashMap::new();
    if let Some(version) = api_version {
        named.insert("apiVersion".to_owned(), version.to_owned());
    }
    ProviderSettings {
        base_url: base_url.map(str::to_owned),
        api_key: "k".to_owned(),
        headers: Headers::new(),
        settings: named,
        model_id: "gpt-5".to_owned(),
        fetch: capture.clone(),
    }
}

/// Drives one `doStream` to its end.
fn drive(model: &DynModel, options: CallOptions) -> Result<(), mewrk_aisdk::error::AiError> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let mut stream = model.do_stream(options).await?;
        while stream.next().await.is_some() {}
        Ok(())
    })
}

fn call_options(json: &Json) -> CallOptions {
    CallOptions {
        prompt: json.get("prompt").and_then(Json::as_arr).cloned().unwrap_or_default(),
        tools: json.get("tools").and_then(Json::as_arr).cloned().filter(|tools| !tools.is_empty()),
        tool_choice: json.get("toolChoice").cloned().unwrap_or_default(),
        max_output_tokens: None,
        reasoning: None,
        provider_options: json.get("providerOptions").cloned(),
        cancel: Cancel::new(),
    }
}

#[test]
fn default_base_urls() {
    let prompt = vec![jobj! { "role": "user", "content": vec![jobj! { "type": "text", "text": "x" }] }];
    let options = jobj! { "prompt": prompt, "toolChoice": jobj! { "type": "auto" } };
    let url_of = |model: DynModel, capture: &Arc<Capture>| {
        drive(&model, call_options(&options)).unwrap();
        capture.0.lock().unwrap().last().unwrap().url.clone()
    };
    let capture = Arc::new(Capture(Mutex::new(Vec::new())));
    let model = openai_responses::create(settings(None, &capture, None), openai_responses::Flavor::OpenAi).unwrap();
    assert_eq!(url_of(model, &capture), "https://api.openai.com/v1/responses");
    let model = openai_responses::create(settings(None, &capture, None), openai_responses::Flavor::Codex).unwrap();
    assert_eq!(url_of(model, &capture), "https://chatgpt.com/backend-api/codex/responses");
    let model = openai_responses::create(settings(Some("https://relay.example/v1/"), &capture, None), openai_responses::Flavor::Codex).unwrap();
    assert_eq!(url_of(model, &capture), "https://relay.example/v1/responses");
}

#[test]
fn azure_official_endpoint_form() {
    let prompt = vec![jobj! { "role": "user", "content": vec![jobj! { "type": "text", "text": "x" }] }];
    let options = jobj! { "prompt": prompt, "toolChoice": jobj! { "type": "auto" } };
    let capture = Arc::new(Capture(Mutex::new(Vec::new())));
    let model = azure::create(settings(Some("https://res.openai.azure.com/openai"), &capture, None)).unwrap();
    drive(&model, call_options(&options)).unwrap();
    let model = azure::create(settings(Some("https://res.openai.azure.com/openai/"), &capture, Some("2025-04-01-preview"))).unwrap();
    drive(&model, call_options(&options)).unwrap();
    let seen = capture.0.lock().unwrap();
    assert_eq!(seen[0].url, "https://res.openai.azure.com/openai/v1/responses?api-version=v1");
    assert_eq!(seen[1].url, "https://res.openai.azure.com/openai/v1/responses?api-version=2025-04-01-preview");
    assert_eq!(seen[0].headers.get("api-key"), Some("k"));
}

/// Each `(options, body or error)` is what `createOpenAI(...).responses('gpt-5').doStream(options)` did under Node,
/// for the tool paths no frame can reach (a host cannot set a tool's `providerOptions` or a tool choice).
const GOLDEN: &[(&str, &str, Result<&str, &str>)] = &[
    (
        "namespaces",
        r#"{"tools":[{"type":"function","name":"a","description":"d","inputSchema":{"type":"object"},"strict":false,"providerOptions":{"openai":{"namespace":{"name":"ns","description":"nd"},"deferLoading":true}}},{"type":"function","name":"b","description":"d","inputSchema":{"type":"object"},"strict":false},{"type":"function","name":"c","description":"d","inputSchema":{"type":"object"},"strict":false,"providerOptions":{"openai":{"namespace":{"name":"ns","description":"nd"},"allowedCallers":["direct"],"outputSchema":{"type":"object"}}}}],"toolChoice":{"type":"auto"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"tools":[{"type":"namespace","name":"ns","description":"nd","tools":[{"type":"function","name":"a","description":"d","parameters":{"type":"object"},"strict":false,"defer_loading":true},{"type":"function","name":"c","description":"d","parameters":{"type":"object"},"strict":false,"allowed_callers":["direct"],"output_schema":{"type":"object"}}]},{"type":"function","name":"b","description":"d","parameters":{"type":"object"},"strict":false}],"tool_choice":"auto","stream":true}"#),
    ),
    (
        "conflict",
        r#"{"tools":[{"type":"function","name":"a","description":"d","inputSchema":{"type":"object"},"strict":false,"providerOptions":{"openai":{"namespace":{"name":"ns","description":"x"}}}},{"type":"function","name":"c","description":"d","inputSchema":{"type":"object"},"strict":false,"providerOptions":{"openai":{"namespace":{"name":"ns","description":"y"}}}}],"toolChoice":{"type":"auto"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
        Err(r#"'conflicting descriptions for OpenAI tool namespace "ns"' functionality not supported."#),
    ),
    (
        "outputSchema",
        r#"{"tools":[{"type":"function","name":"o","description":"d","inputSchema":{"type":"object"},"strict":false,"providerOptions":{"openai":{"outputSchema":{"type":"object"}}}},{"type":"function","name":"p","description":"d","inputSchema":{"type":"object"},"strict":false}],"toolChoice":{"type":"none"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]},{"role":"assistant","content":[{"type":"tool-call","toolCallId":"c1","toolName":"o","input":{}},{"type":"tool-call","toolCallId":"c2","toolName":"p","input":{}}]},{"role":"tool","content":[{"type":"tool-result","toolCallId":"c1","toolName":"o","output":{"type":"text","value":"t\"x"}},{"type":"tool-result","toolCallId":"c1","toolName":"o","output":{"type":"execution-denied"}},{"type":"tool-result","toolCallId":"c1","toolName":"o","output":{"type":"json","value":{"a":1}}},{"type":"tool-result","toolCallId":"c2","toolName":"p","output":{"type":"execution-denied"}}]}]}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]},{"type":"function_call","call_id":"c1","name":"o","arguments":"{}"},{"type":"function_call","call_id":"c2","name":"p","arguments":"{}"},{"type":"function_call_output","call_id":"c1","output":"\"t\\\"x\""},{"type":"function_call_output","call_id":"c1","output":"\"Tool call execution denied.\""},{"type":"function_call_output","call_id":"c1","output":"{\"a\":1}"},{"type":"function_call_output","call_id":"c2","output":"Tool call execution denied."}],"tools":[{"type":"function","name":"o","description":"d","parameters":{"type":"object"},"strict":false,"output_schema":{"type":"object"}},{"type":"function","name":"p","description":"d","parameters":{"type":"object"},"strict":false}],"tool_choice":"none","stream":true}"#),
    ),
    (
        "choiceFunction",
        r#"{"tools":[{"type":"function","name":"a","description":"d","inputSchema":{"type":"object"},"strict":false},{"type":"provider","name":"web_search","id":"openai.web_search","args":{}}],"toolChoice":{"type":"tool","toolName":"a"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"include":["web_search_call.action.sources"],"tools":[{"type":"function","name":"a","description":"d","parameters":{"type":"object"},"strict":false},{"type":"web_search"}],"tool_choice":{"type":"function","name":"a"},"stream":true}"#),
    ),
    (
        "choiceSearch",
        r#"{"tools":[{"type":"function","name":"a","description":"d","inputSchema":{"type":"object"},"strict":false},{"type":"provider","name":"web_search","id":"openai.web_search","args":{}}],"toolChoice":{"type":"tool","toolName":"web_search"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"include":["web_search_call.action.sources"],"tools":[{"type":"function","name":"a","description":"d","parameters":{"type":"object"},"strict":false},{"type":"web_search"}],"tool_choice":{"type":"web_search"},"stream":true}"#),
    ),
    (
        "choiceRequired",
        r#"{"tools":[{"type":"function","name":"a","description":"d","inputSchema":{"type":"object"},"strict":false}],"toolChoice":{"type":"required"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"tools":[{"type":"function","name":"a","description":"d","parameters":{"type":"object"},"strict":false}],"tool_choice":"required","stream":true}"#),
    ),
    (
        "noChoice",
        r#"{"tools":[{"type":"function","name":"a","description":"d","inputSchema":{"type":"object"},"strict":false}],"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"tools":[{"type":"function","name":"a","description":"d","parameters":{"type":"object"},"strict":false}],"stream":true}"#),
    ),
    (
        "emptyTools",
        r#"{"tools":[],"toolChoice":{"type":"auto"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"stream":true}"#),
    ),
    (
        "allowedHidden",
        r#"{"tools":[{"type":"function","name":"a","description":"d","inputSchema":{"type":"object"},"strict":false,"providerOptions":{"openai":{"deferLoading":true}}},{"type":"function","name":"b","description":"d","inputSchema":{"type":"object"},"strict":false,"providerOptions":{"openai":{"namespace":{"name":"n","description":"d"}}}}],"toolChoice":{"type":"auto"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}],"providerOptions":{"openai":{"allowedTools":{"toolNames":["a","b"]}}}}"#,
        Err(r#"'allowedTools with only tools that cannot be allow-listed (a, b)' functionality not supported."#),
    ),
    (
        "allowedMixed",
        r#"{"tools":[{"type":"function","name":"a","description":"d","inputSchema":{"type":"object"},"strict":false,"providerOptions":{"openai":{"deferLoading":true}}},{"type":"function","name":"b","description":"d","inputSchema":{"type":"object"},"strict":false},{"type":"provider","name":"web_search","id":"openai.web_search","args":{}}],"toolChoice":{"type":"auto"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}],"providerOptions":{"openai":{"allowedTools":{"toolNames":["a","b","web_search","z"]}}}}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"include":["web_search_call.action.sources"],"tools":[{"type":"function","name":"a","description":"d","parameters":{"type":"object"},"strict":false,"defer_loading":true},{"type":"function","name":"b","description":"d","parameters":{"type":"object"},"strict":false},{"type":"web_search"}],"tool_choice":{"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"b"},{"type":"web_search"},{"type":"function","name":"z"}]},"stream":true}"#),
    ),
    (
        "searchArgs",
        r#"{"tools":[{"type":"provider","name":"web_search","id":"openai.web_search","args":{"searchContextSize":"low","externalWebAccess":false,"filters":{"allowedDomains":["a.example"],"blockedDomains":["b.example"]},"userLocation":{"type":"approximate","city":"c"}}}],"toolChoice":{"type":"auto"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"include":["web_search_call.action.sources"],"tools":[{"type":"web_search","filters":{"allowed_domains":["a.example"],"blocked_domains":["b.example"]},"external_web_access":false,"search_context_size":"low","user_location":{"type":"approximate","city":"c"}}],"tool_choice":"auto","stream":true}"#),
    ),
    (
        "noStrictNoDesc",
        r#"{"tools":[{"type":"function","name":"a","inputSchema":{"type":"object"}}],"toolChoice":{"type":"auto"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"tools":[{"type":"function","name":"a","parameters":{"type":"object"}}],"tool_choice":"auto","stream":true}"#),
    ),
    (
        "denied",
        r#"{"tools":[{"type":"function","name":"a","description":"d","inputSchema":{"type":"object"},"strict":false}],"toolChoice":{"type":"auto"},"prompt":[{"role":"user","content":[{"type":"text","text":"x"}]},{"role":"assistant","content":[{"type":"tool-result","toolCallId":"t","toolName":"x","output":{"type":"execution-denied"}},{"type":"tool-result","toolCallId":"t2","toolName":"x","output":{"type":"json","value":{"type":"execution-denied"}}},{"type":"tool-result","toolCallId":"t3","toolName":"x","output":{"type":"json","value":{}},"providerOptions":{"openai":{"itemId":"it3"}}}]},{"role":"tool","content":[{"type":"tool-approval-response","approvalId":"ap","approved":true},{"type":"tool-approval-response","approvalId":"ap","approved":true},{"type":"tool-result","toolCallId":"t4","toolName":"a","output":{"type":"execution-denied","providerOptions":{"openai":{"approvalId":"ap"}}}}]}]}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]},{"type":"item_reference","id":"it3"},{"type":"item_reference","id":"ap"},{"type":"mcp_approval_response","approval_request_id":"ap","approve":true}],"tools":[{"type":"function","name":"a","description":"d","parameters":{"type":"object"},"strict":false}],"tool_choice":"auto","stream":true}"#),
    ),
    (
        "emptyText",
        r#"{"tools":[],"toolChoice":{"type":"auto"},"prompt":[{"role":"system","content":"s","providerOptions":{"openai":{"promptCacheBreakpoint":{"mode":"explicit"}}}},{"role":"user","content":[{"type":"text","text":"x"}]},{"role":"assistant","content":[{"type":"text","text":"","providerOptions":{"openai":{"itemId":"m"}}}]}],"providerOptions":{"openai":{"store":false}}}"#,
        Ok(r#"{"model":"gpt-5","input":[{"role":"developer","content":[{"type":"input_text","text":"s","prompt_cache_breakpoint":{"mode":"explicit"}}]},{"role":"user","content":[{"type":"input_text","text":"x"}]},{"role":"assistant","content":[{"type":"output_text","text":""}],"id":"m"}],"store":false,"include":["reasoning.encrypted_content"],"stream":true}"#),
    ),
];

#[test]
fn the_model_matches_upstream_where_no_frame_reaches() {
    for (name, options, expected) in GOLDEN {
        let capture = Arc::new(Capture(Mutex::new(Vec::new())));
        let model = openai_responses::create(settings(Some("http://mock/v1"), &capture, None), openai_responses::Flavor::OpenAi).unwrap();
        let outcome = drive(&model, call_options(&Json::parse(options).unwrap()));
        match expected {
            Ok(body) => {
                outcome.unwrap_or_else(|error| panic!("{name}: {error}"));
                assert_eq!(capture.0.lock().unwrap().last().unwrap().body.as_deref(), Some(*body), "{name}");
            }
            Err(message) => assert_eq!(outcome.unwrap_err().message(), *message, "{name}"),
        }
    }
}
