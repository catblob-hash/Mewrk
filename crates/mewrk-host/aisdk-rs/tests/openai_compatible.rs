//! The `openai-chat` and `openai-compatible` families against a mock upstream on a real socket.

mod common;

use std::sync::Arc;

use common::{Mock, Outcome, Reply, Rust};
use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;

const FAMILIES: [&str; 2] = ["openai-chat", "openai-compatible"];

fn request(mock: &Mock, family: &str, model: &str, extra: Json) -> Json {
    let mut payload = jobj! {
        "family": family,
        "baseURL": format!("{}/v1", mock.base),
        "apiKey": "sk-test-secret",
        "modelId": model,
        "messages": vec![jobj! { "role": "user", "content": "hi" }],
        "maxSteps": 1u64,
    };
    if let (Some(object), Some(extra)) = (payload.as_obj_mut(), extra.as_obj()) {
        object.extend_from(extra);
    }
    payload
}

fn chunk(delta: &str, finish: Option<&str>, usage: Option<&str>) -> String {
    let finish = finish.map_or("null".to_owned(), |f| format!("\"{f}\""));
    let usage = usage.map_or(String::new(), |u| format!(",\"usage\":{u}"));
    format!(r#"{{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"served-model","choices":[{{"index":0,"delta":{delta},"finish_reason":{finish}}}]{usage}}}"#)
}

fn sse(events: &[String]) -> Reply {
    Reply::sse(&events.iter().map(String::as_str).collect::<Vec<_>>())
}

fn ok_stream(text: &str) -> Reply {
    sse(&[
        chunk(r#"{"role":"assistant","content":""}"#, None, None),
        chunk(&format!(r#"{{"content":"{text}"}}"#), None, None),
        chunk("{}", Some("stop"), Some(r#"{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}"#)),
    ])
}

fn run_one(mock: &Mock, family: &str, model: &str, extra: Json) -> Outcome {
    Rust::start().run("s", &request(mock, family, model, extra))
}

fn read_tool() -> Vec<Json> {
    vec![jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "path": jobj! { "type": "string" } } } }]
}

#[test]
fn text_streaming_and_the_request_shape() {
    for family in FAMILIES {
        let mock = Mock::script(vec![ok_stream("hello")]);
        let outcome = run_one(&mock, family, "some-model", jobj! { "tools": read_tool(), "system": "S", "maxOutputTokens": 9u64 });
        assert_eq!(outcome.text(), "hello");
        let result = outcome.result();
        assert_eq!(result.str_at("finishReason"), Some("stop"));
        assert_eq!(result.str_at("rawFinishReason"), Some("stop"));
        assert_eq!(result.str_at("model"), Some("served-model"));
        assert_eq!(result.get("responseMessages").unwrap().to_text(), r#"[{"role":"assistant","content":[{"type":"text","text":"hello"}]}]"#);

        let seen = mock.last_request();
        assert_eq!((seen.method.as_str(), seen.target.as_str()), ("POST", "/v1/chat/completions"));
        assert_eq!(seen.header("authorization"), Some("Bearer sk-test-secret"));
        assert_eq!(seen.header("content-type"), Some("application/json"));
        assert_eq!(seen.header("user-agent"), Some("ai-sdk/openai-compatible/3.0.39 ai-sdk/provider-utils/5.0.32 runtime/rust"));
        assert_eq!(
            seen.body,
            r#"{"model":"some-model","max_tokens":9,"messages":[{"role":"system","content":"S"},{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"read","description":"d","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}],"tool_choice":"auto","stream":true,"stream_options":{"include_usage":true}}"#
        );
    }
}

#[test]
fn an_empty_key_sends_no_authorization() {
    let mock = Mock::script(vec![ok_stream("x")]);
    run_one(&mock, "openai-chat", "m", jobj! { "apiKey": "" });
    assert_eq!(mock.last_request().header("authorization"), None);
}

#[test]
fn reasoning_text_and_tool_calls_stream_in_order() {
    let mock = Mock::script(vec![sse(&[
        chunk(r#"{"reasoning_content":"Let me "}"#, None, None),
        chunk(r#"{"reasoning":"not read for this chunk","reasoning_content":"think"}"#, None, None),
        chunk(r#"{"content":"ok"}"#, None, None),
        chunk(r#"{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":""},"extra_content":{"google":{"thought_signature":"SIG"}}}]}"#, None, None),
        chunk(r#"{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a\"}"}}]}"#, None, None),
        chunk("{}", Some("tool_calls"), Some(r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}"#)),
    ])]);
    let outcome = run_one(&mock, "openai-chat", "m", jobj! { "tools": read_tool(), "reasoningContent": "plaintext" });
    let reasoning: String = outcome.events_of("reasoning-delta").iter().filter_map(|e| e.str_at("delta")).collect();
    assert_eq!(reasoning, "Let me think");
    assert_eq!(outcome.events_of("reasoning-done").len(), 1);
    assert_eq!(outcome.text(), "ok");
    assert_eq!(outcome.events_of("tool-call-announced")[0].to_text(), r#"{"k":"tool-call-announced","callId":"call_1","toolName":"read"}"#);
    let result = outcome.result();
    assert_eq!(result.str_at("finishReason"), Some("tool-calls"));
    assert_eq!(result.get("calls").unwrap().to_text(), r#"[{"callId":"call_1","toolName":"read","input":{"path":"a"}}]"#);
    assert_eq!(result.get("reasoning").unwrap().to_text(), r#"["Let me think"]"#);
    assert_eq!(result.get("usage").unwrap().to_text(), r#"{"inputTokens":10,"outputTokens":5,"totalTokens":15,"reasoningTokens":2,"cacheReadTokens":4}"#);
    // The signature persists under the provider's own name, and the host replays it verbatim.
    assert_eq!(
        result.get("responseMessages").unwrap().to_text(),
        r#"[{"role":"assistant","content":[{"type":"reasoning","text":"Let me think"},{"type":"text","text":"ok"},{"type":"tool-call","toolCallId":"call_1","toolName":"read","input":{"path":"a"},"providerOptions":{"openai-chat":{"thoughtSignature":"SIG"}}}]}]"#
    );
}

#[test]
fn the_reasoning_field_serves_gpt_oss_relays() {
    let mock = Mock::script(vec![sse(&[
        chunk(r#"{"reasoning":"a"}"#, None, None),
        chunk(r#"{"content":"b"}"#, None, None),
        chunk(r#"{"reasoning":"c"}"#, None, None),
        chunk(r#"{"content":"d"}"#, Some("length"), None),
    ])]);
    let outcome = run_one(&mock, "openai-compatible", "gpt-oss", jobj! {});
    assert_eq!(outcome.text(), "bd");
    let result = outcome.result();
    assert_eq!(result.get("reasoning").unwrap().to_text(), r#"["a","c"]"#);
    assert_eq!(result.str_at("finishReason"), Some("length"));
}

#[test]
fn history_replays_reasoning_signatures_and_results() {
    let mock = Mock::script(vec![ok_stream("x")]);
    run_one(&mock, "openai-chat", "m", jobj! {
        "tools": read_tool(),
        "messages": vec![
            jobj! { "role": "user", "content": vec![
                jobj! { "type": "text", "text": "look" },
                jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png" },
                jobj! { "type": "file", "data": "AAAA", "mediaType": "audio/mpeg" },
                jobj! { "type": "file", "data": "JVBERg==", "mediaType": "application/pdf" },
                jobj! { "type": "file", "data": "aGVsbG8=", "mediaType": "text/plain" },
            ] },
            jobj! { "role": "assistant", "content": vec![
                jobj! { "type": "reasoning", "text": "why" },
                jobj! { "type": "tool-call", "toolCallId": "c0", "toolName": "read", "input": jobj! { "path": "x" }, "providerOptions": jobj! { "openai-chat": jobj! { "thoughtSignature": "SIG" } } },
            ] },
            jobj! { "role": "tool", "content": vec![
                jobj! { "type": "tool-result", "toolCallId": "c0", "toolName": "read", "output": jobj! { "type": "json", "value": jobj! { "a": 1u64 } } },
            ] },
        ],
    });
    assert_eq!(
        mock.last_request().json().get("messages").unwrap().to_text(),
        concat!(
            r#"[{"role":"user","content":[{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}},"#,
            r#"{"type":"input_audio","input_audio":{"data":"AAAA","format":"mp3"}},"#,
            r#"{"type":"file","file":{"filename":"document.pdf","file_data":"data:application/pdf;base64,JVBERg=="}},"#,
            r#"{"type":"text","text":"hello"}]},"#,
            r#"{"role":"assistant","content":null,"reasoning_content":"why","tool_calls":[{"id":"c0","type":"function","function":{"name":"read","arguments":"{\"path\":\"x\"}"},"extra_content":{"google":{"thought_signature":"SIG"}}}]},"#,
            r#"{"role":"tool","tool_call_id":"c0","content":"{\"a\":1}"}]"#,
        )
    );
}

#[test]
fn options_merge_under_each_name_and_the_rest_reach_the_body() {
    let mock = Mock::script(vec![ok_stream("x")]);
    run_one(&mock, "openai-compatible", "deepseek-r1", jobj! {
        "reasoning": "max",
        "providerOptions": jobj! {
            "openai-compatible": jobj! { "textVerbosity": "low", "custom": 1u64 },
            "openaiCompatible": jobj! { "user": "u", "thinking": jobj! { "type": "enabled" } },
            "openai-chat": jobj! { "ignored": true },
        },
    });
    assert_eq!(
        mock.last_request().body,
        r#"{"model":"deepseek-r1","user":"u","custom":1,"thinking":{"type":"enabled"},"reasoning_effort":"max","verbosity":"low","messages":[{"role":"user","content":"hi"}],"stream":true,"stream_options":{"include_usage":true}}"#
    );

    // A wrong type under any name is refused before anything is sent.
    let mock = Mock::script(vec![ok_stream("x")]);
    let outcome = run_one(&mock, "openai-chat", "m", jobj! { "providerOptions": jobj! { "openaiChat": jobj! { "user": 1u64 } } });
    assert_eq!(outcome.error().str_at("kind"), Some("permanent"));
    assert_eq!(outcome.error().str_at("message"), Some("invalid openaiChat provider options"));
    assert!(mock.requests().is_empty());
}

#[test]
fn the_metadata_key_follows_the_name_the_caller_used() {
    let stream = sse(&[chunk(r#"{"tool_calls":[{"index":0,"id":"c","function":{"name":"read","arguments":"{}"},"extra_content":{"google":{"thought_signature":"SIG"}}}]}"#, Some("tool_calls"), None)]);
    // `openaiCompatible` is what the reasoning plan writes: the family of that name now reads and writes it.
    let mock = Mock::script(vec![stream.clone(), ok_stream("x")]);
    let outcome = run_one(&mock, "openai-compatible", "m", jobj! { "tools": read_tool(), "reasoning": "low" });
    let messages = outcome.result().get("responseMessages").unwrap().to_text();
    assert!(messages.contains(r#""providerOptions":{"openaiCompatible":{"thoughtSignature":"SIG"}}"#), "{messages}");
    // `openai-chat` has another camelCase form, so it keeps its raw name.
    let mock = Mock::script(vec![stream]);
    let outcome = run_one(&mock, "openai-chat", "m", jobj! { "tools": read_tool(), "reasoning": "low" });
    let messages = outcome.result().get("responseMessages").unwrap().to_text();
    assert!(messages.contains(r#""providerOptions":{"openai-chat":{"thoughtSignature":"SIG"}}"#), "{messages}");
    assert_eq!(mock.last_request().json().str_at("reasoning_effort"), Some("low"));
}

#[test]
fn an_empty_base_url_fails_before_any_request() {
    for family in FAMILIES {
        for base in [None, Some(""), Some("relay/v1")] {
            let mut payload = jobj! {
                "family": family,
                "apiKey": "k",
                "modelId": "m",
                "messages": vec![jobj! { "role": "user", "content": "hi" }],
            };
            if let Some(base) = base {
                payload.as_obj_mut().unwrap().insert("baseURL", base);
            }
            let outcome = Rust::start().run("s", &payload);
            assert_eq!(outcome.error().to_text(), r#"{"kind":"permanent","message":"Invalid URL"}"#, "{family} {base:?}");
        }
    }
}

#[test]
fn errors_classify_by_status_and_carry_retry_after() {
    let cases: [(Reply, &str, &str, u16); 4] = [
        (Reply::json(429, r#"{"error":{"message":"slow down","code":"rate_limit_exceeded"}}"#).header("retry-after", "7"), "transient", "slow down", 429),
        (Reply::json(401, r#"{"error":{"message":"bad key","code":401}}"#), "permanent", "bad key", 401),
        (Reply::new(500), "transient", "Internal Server Error", 500),
        (Reply::json(404, r#"{"error":"not here"}"#), "permanent", "Not Found", 404),
    ];
    for (reply, kind, message, status) in cases {
        let mock = Mock::script(vec![reply]);
        let outcome = run_one(&mock, "openai-chat", "m", jobj! {});
        let error = outcome.error();
        assert_eq!(error.str_at("kind"), Some(kind));
        assert_eq!(error.str_at("message"), Some(message));
        assert_eq!(error.get("status").and_then(Json::as_u64), Some(status as u64));
        assert_eq!(error.get("retryAfterMs").and_then(Json::as_u64), (status == 429).then_some(7000));
        // No retry but the dialect's own: the host owns that.
        assert_eq!(mock.requests().len(), 1);
    }
}

#[test]
fn errors_inside_the_stream() {
    let classified = |reply: Reply| {
        let mock = Mock::script(vec![reply]);
        run_one(&mock, "openai-chat", "m", jobj! {}).error().to_text()
    };
    assert_eq!(classified(Reply::sse(&[r#"{"error":{"message":"Overloaded"}}"#])), r#"{"kind":"transient","message":"Overloaded","status":503}"#);
    assert_eq!(classified(Reply::sse(&[r#"{"error":{"message":"m","code":"rate_limit_exceeded"}}"#])), r#"{"kind":"transient","message":"m"}"#);
    assert_eq!(classified(Reply::sse(&[r#"{"error":{"message":"m","code":"invalid_thing"}}"#])), r#"{"kind":"permanent","message":"m"}"#);
    assert_eq!(classified(Reply::sse(&[r#"{"error":{"message":"m","code":429}}"#])), r#"{"kind":"transient","message":"m","status":429}"#);
    // The error schema strips what it does not name; a chunk that also passes the base schema keeps it.
    assert_eq!(classified(Reply::sse(&[r#"{"error":{"message":"m","status":503}}"#])), r#"{"kind":"permanent","message":"m"}"#);
    assert_eq!(classified(Reply::sse(&[r#"{"choices":[],"error":{"message":"m","status":503}}"#])), r#"{"kind":"transient","message":"m","status":503}"#);
    assert_eq!(classified(sse(&[chunk(r#"{"content":"cut"}"#, None, None)])), r#"{"kind":"permanent","message":"Response stream ended without a finish reason."}"#);
    assert_eq!(classified(Reply::sse_raw("")), r#"{"kind":"permanent","message":"Response stream ended without a finish reason."}"#);
    let invalid = classified(sse(&[chunk(r#"{"role":"tool"}"#, None, None)]));
    assert!(invalid.starts_with(r#"{"kind":"permanent","message":"Type validation failed: Value: "#), "{invalid}");
    // A call that never gets a name fails the stream when it ends.
    let unnamed = classified(sse(&[chunk(r#"{"tool_calls":[{"index":0,"id":"a","function":{"arguments":"{}"}}]}"#, Some("tool_calls"), None)]));
    assert_eq!(unnamed, r#"{"kind":"permanent","message":"Expected 'function.name' to be a string."}"#);
}

// ---------------------------------------------------------------- The relay shapes of `selfcheck.mjs`

#[test]
fn legacy_function_call_frames() {
    for family in FAMILIES {
        let mock = Mock::script(vec![sse(&[
            chunk(r#"{"role":"assistant","content":null,"function_call":{"name":"read","arguments":""}}"#, None, None),
            chunk(r#"{"function_call":{"arguments":"{\"path\":\"a\"}"}}"#, None, None),
            chunk("{}", Some("function_call"), None),
        ])]);
        let outcome = run_one(&mock, family, "m", jobj! { "tools": read_tool() });
        let result = outcome.result();
        assert_eq!(result.get("calls").unwrap().to_text(), r#"[{"callId":"legacy_chatcmpl-1","toolName":"read","input":{"path":"a"}}]"#);
        assert_eq!(result.str_at("finishReason"), Some("tool-calls"));
        assert_eq!(result.str_at("rawFinishReason"), Some("tool_calls"));
    }
}

#[test]
fn usage_only_final_chunk() {
    let mock = Mock::script(vec![Reply::sse_raw(&format!(
        "data: {}\n\ndata: {}\n\ndata: {{\"id\":\"x\",\"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}}}\n\ndata: [DONE]\n\n",
        chunk(r#"{"content":"tail-ok"}"#, None, None),
        chunk("{}", Some("stop"), None),
    ))]);
    let outcome = run_one(&mock, "openai-compatible", "m", jobj! {});
    assert_eq!(outcome.result().str_at("text"), Some("tail-ok"));
    assert_eq!(outcome.result().path(&["usage", "totalTokens"]).and_then(Json::as_u64), Some(2));
}

#[test]
fn a_whole_body_for_stream_true() {
    let mock = Mock::script(vec![Reply::json(200, r#"{"id":"j","object":"chat.completion","created":1,"model":"json-body","choices":[{"index":0,"message":{"role":"assistant","content":"json-ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#)]);
    let outcome = run_one(&mock, "openai-compatible", "m", jobj! {});
    assert_eq!(outcome.result().str_at("text"), Some("json-ok"));
    assert_eq!(outcome.result().path(&["usage", "totalTokens"]).and_then(Json::as_u64), Some(5));

    // With no terminal reason it must not pass for a clean finish.
    let mock = Mock::script(vec![Reply::json(200, r#"{"id":"j","choices":[{"index":0,"message":{"role":"assistant","content":"half an ans"}}]}"#)]);
    let outcome = run_one(&mock, "openai-compatible", "m", jobj! {});
    assert_eq!(outcome.error().str_at("message"), Some("Response stream ended without a finish reason."));
}

#[test]
fn rejected_stream_options_are_retried_once_and_remembered() {
    let mock = Mock::start(Arc::new(|seen: &common::Seen, _| {
        if seen.json().get("stream_options").is_some() {
            return Reply::json(400, r#"{"error":{"message":"json: unknown field \"stream_options\""}}"#);
        }
        ok_stream("strict-ok")
    }));
    let rust = Rust::start();
    let first = rust.run("a", &request(&mock, "openai-compatible", "m", jobj! {}));
    assert_eq!(first.result().str_at("text"), Some("strict-ok"));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].json().get("stream_options").is_some());
    assert!(requests[1].json().get("stream_options").is_none());
    // The next request to that URL does not ask again.
    let second = rust.run("b", &request(&mock, "openai-compatible", "m", jobj! {}));
    assert_eq!(second.result().str_at("text"), Some("strict-ok"));
    assert_eq!(mock.requests().len(), 3);
}

#[test]
fn rejected_reasoning_effort_steps_down_and_is_remembered() {
    let refusing = |seen: &common::Seen, _| {
        if seen.json().str_at("reasoning_effort") == Some("max") {
            return Reply::json(400, r#"{"error":{"type":"invalid_request_error","param":"reasoning_effort","message":"Unsupported value: 'reasoning_effort' does not support 'max' with this model. Supported values are: 'low', 'medium', 'high', and 'xhigh'."}}"#);
        }
        ok_stream("level-ok")
    };
    let mock = Mock::start(Arc::new(refusing));
    let rust = Rust::start();
    let efforts = |mock: &Mock| mock.requests().iter().map(|r| r.json().str_at("reasoning_effort").unwrap_or("-").to_owned()).collect::<Vec<_>>();
    assert_eq!(rust.run("a", &request(&mock, "openai-compatible", "level-refuses-max", jobj! { "reasoning": "max" })).result().str_at("text"), Some("level-ok"));
    assert_eq!(efforts(&mock), ["max", "xhigh"]);
    assert_eq!(rust.run("b", &request(&mock, "openai-compatible", "level-refuses-max", jobj! { "reasoning": "max" })).result().str_at("text"), Some("level-ok"));
    assert_eq!(efforts(&mock), ["max", "xhigh", "xhigh"]);

    let mock = Mock::start(Arc::new(|seen: &common::Seen, _| {
        if seen.json().get("reasoning_effort").is_some() {
            return Reply::json(400, r#"{"error":{"message":"`reasoning_effort` is not supported with this model"}}"#);
        }
        ok_stream("level-ok")
    }));
    assert_eq!(rust.run("c", &request(&mock, "openai-chat", "level-refuses-field", jobj! { "reasoning": "low" })).result().str_at("text"), Some("level-ok"));
    assert_eq!(efforts(&mock), ["low", "-"]);

    // `gpt-4o` takes no `reasoning_effort` at all.
    let mock = Mock::script(vec![ok_stream("x")]);
    rust.run("d", &request(&mock, "openai-chat", "gpt-4o", jobj! { "reasoning": "high" }));
    assert_eq!(efforts(&mock), ["-"]);
}

#[test]
fn the_reasoning_sentinel_is_backfilled_only_with_evidence() {
    let history = |evidence: bool| {
        jobj! { "messages": vec![
            jobj! { "role": "user", "content": "read" },
            jobj! {
                "role": "assistant",
                "content": vec![jobj! { "type": "tool-call", "toolCallId": "call_a", "toolName": "read", "input": jobj! {} }],
                "providerOptions"?: evidence.then(|| jobj! { "openaiCompatible": jobj! { "reasoning_content": "" } }),
            },
            jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "call_a", "toolName": "read", "output": jobj! { "type": "text", "value": "ok" } }] },
            jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "call_b", "toolName": "read", "input": jobj! {} }] },
            jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "call_b", "toolName": "read", "output": jobj! { "type": "text", "value": "ok" } }] },
        ] }
    };
    for family in FAMILIES {
        let tool_turns = |mock: &Mock| {
            let body = mock.last_request().json();
            let messages = body.get("messages").unwrap().as_arr().unwrap().clone();
            messages.iter().filter(|m| m.get("tool_calls").is_some()).map(|m| m.get("reasoning_content").map(Json::to_text)).collect::<Vec<_>>()
        };
        let mock = Mock::script(vec![ok_stream("sentinel-ok")]);
        run_one(&mock, family, "empty-reasoning-sentinel", history(false));
        assert_eq!(tool_turns(&mock), [None, None]);
        let mock = Mock::script(vec![ok_stream("sentinel-ok")]);
        assert_eq!(run_one(&mock, family, "empty-reasoning-sentinel", history(true)).result().str_at("text"), Some("sentinel-ok"));
        assert_eq!(tool_turns(&mock), [Some("\"\"".to_owned()), Some("\"\"".to_owned())]);
    }
}

#[test]
fn done_variants_end_the_stream_cleanly() {
    for done in ["data: [DONE]", "data:[DONE]", "data:  [DONE]", "data: [DONE] "] {
        let mock = Mock::script(vec![Reply::sse_raw(&format!("data: {}\n\n{done}\n\n", chunk(r#"{"content":"wide-ok"}"#, Some("stop"), None)))]);
        assert_eq!(run_one(&mock, "openai-compatible", "m", jobj! {}).result().str_at("text"), Some("wide-ok"), "{done:?}");
    }
}

fn alpha_beta() -> Vec<Json> {
    ["alpha", "beta"].iter().map(|name| jobj! { "name": *name, "description": *name, "inputSchema": jobj! { "type": "object", "additionalProperties": true } }).collect()
}

#[test]
fn tool_call_identities_from_selfcheck() {
    let a = r#"{"id":"A","function":{"name":"alpha","arguments":"{\"a\":"}}"#;
    let a_fn = r#""function":{"name":"alpha","arguments":"{\"a\":"}"#;
    let b_fn = r#""function":{"name":"beta","arguments":"{\"b\":"}"#;
    let a1 = r#"{"id":"A","function":{"arguments":"1}"}}"#;
    let b2 = r#"{"id":"B","function":{"arguments":"2}"}}"#;
    let index_a1 = r#"{"index":0,"id":"A","function":{"arguments":"1}"}}"#;
    let cases: Vec<(&str, Option<&str>, usize, Vec<String>)> = vec![
        ("collision", Some("index conflict"), 0, vec![a.into(), format!(r#"{{"index":0,"id":"B",{b_fn}}}"#), a1.into(), b2.into()]),
        ("rebind", Some("index conflict"), 0, vec![a.into(), r#"{"index":1,"id":"A","function":{"arguments":"1}"}}"#.into()]),
        ("late-id", None, 1, vec![format!(r#"{{"index":0,{a_fn}}}"#), index_a1.into()]),
        ("missing-id", None, 1, vec![format!(r#"{{"index":0,{a_fn}}}"#), r#"{"index":0,"function":{"arguments":"1}"}}"#.into()]),
        ("late-two", None, 2, vec![format!(r#"{{"index":0,{a_fn}}}"#), format!(r#"{{"index":1,{b_fn}}}"#), index_a1.into(), r#"{"index":1,"id":"B","function":{"arguments":"2}"}}"#.into()]),
        ("repeated-name", None, 1, vec![format!(r#"{{"index":0,{a_fn}}}"#), r#"{"index":0,"id":"A","function":{"name":"alpha","arguments":"1}"}}"#.into()]),
        ("arguments-first", None, 1, vec![r#"{"index":0,"function":{"arguments":"{\"a\":"}}"#.into(), r#"{"index":0,"function":{"name":"alpha","arguments":""}}"#.into(), index_a1.into()]),
        ("anonymous-single", None, 1, vec![format!(r#"{{{a_fn}}}"#), r#"{"function":{"arguments":"1}"}}"#.into()]),
        ("anonymous-orphan", Some("identity ambiguity"), 0, vec![r#"{"function":{"arguments":"{}"}}"#.into()]),
        ("anonymous-many", Some("identity ambiguity"), 0, vec![r#"{"function":{"name":"alpha","arguments":"{\"a\":1}"}}"#.into(), r#"{"function":{"name":"beta","arguments":"{\"b\":2}"}}"#.into(), r#"{"function":{"arguments":" "}}"#.into()]),
        ("anonymous-interleaved", Some("identity ambiguity"), 0, vec![format!(r#"{{{a_fn}}}"#), format!(r#"{{{b_fn}}}"#), r#"{"function":{"arguments":"1}"}}"#.into(), r#"{"function":{"arguments":"2}"}}"#.into()]),
        ("confirm", None, 1, vec![a.into(), index_a1.into()]),
        ("separate", None, 2, vec![a.into(), format!(r#"{{"index":1,"id":"B",{b_fn}}}"#), a1.into(), b2.into()]),
        ("explicit-first", None, 2, vec![format!(r#"{{"index":0,"id":"A",{a_fn}}}"#), format!(r#"{{"id":"B",{b_fn}}}"#), a1.into(), b2.into()]),
        ("no-index", None, 2, vec![a.into(), format!(r#"{{"id":"B",{b_fn}}}"#), a1.into(), b2.into()]),
    ];
    let rust = Rust::start();
    for family in FAMILIES {
        for (name, error, count, deltas) in &cases {
            let mut events: Vec<String> = deltas.iter().map(|d| chunk(&format!(r#"{{"tool_calls":[{d}]}}"#), None, None)).collect();
            events.push(chunk("{}", Some("tool_calls"), None));
            let mock = Mock::script(vec![sse(&events)]);
            let outcome = rust.run(&format!("{family}-{name}"), &request(&mock, family, "m", jobj! { "tools": alpha_beta() }));
            if let Some(error) = error {
                let message = outcome.error().str_at("message").unwrap().to_owned();
                assert!(message.contains(error), "{name}: {message}");
                continue;
            }
            let result = outcome.result();
            let calls = result.get("calls").unwrap().as_arr().unwrap();
            assert_eq!(calls.len(), *count, "{name}: {result}");
            assert_eq!(calls[0].path(&["input", "a"]).and_then(Json::as_u64), Some(1), "{name}: {result}");
            if *count == 2 {
                assert_eq!(calls[1].path(&["input", "b"]).and_then(Json::as_u64), Some(2), "{name}: {result}");
            }
            let announced = outcome.events_of("tool-call-announced");
            assert_eq!(announced.len(), *count, "{name}");
            for (announcement, call) in announced.iter().zip(calls) {
                assert_eq!(announcement.str_at("callId"), call.str_at("callId"), "{name}");
            }
        }
    }
}

#[test]
fn usage_evidence_from_selfcheck() {
    let shapes: [(&str, &str, u64); 14] = [
        ("hit", r#""prompt_cache_hit_tokens":80"#, 80),
        ("pair", r#""prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20"#, 80),
        ("standard", r#""prompt_tokens_details":{"cached_tokens":30},"prompt_cache_hit_tokens":80"#, 30),
        ("zero", r#""prompt_tokens_details":{"cached_tokens":0},"prompt_cache_hit_tokens":80"#, 0),
        ("null", r#""prompt_tokens_details":{"cached_tokens":null},"prompt_cache_hit_tokens":80"#, 80),
        ("negative", r#""prompt_cache_hit_tokens":-1"#, 0),
        ("fraction", r#""prompt_cache_hit_tokens":1.5"#, 0),
        ("excess", r#""prompt_cache_hit_tokens":101"#, 0),
        ("mismatch", r#""prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":30"#, 0),
        ("badmiss", r#""prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":-1"#, 0),
        ("string", r#""prompt_cache_hit_tokens":"80""#, 0),
        ("infinite", r#""prompt_cache_hit_tokens":1e400"#, 0),
        ("standardonly", r#""prompt_tokens_details":{"cached_tokens":30}"#, 30),
        ("none", r#""x":0"#, 0),
    ];
    let rust = Rust::start();
    for family in FAMILIES {
        for (name, extra, cached) in shapes {
            let usage = format!(r#"{{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110,"completion_tokens_details":{{"reasoning_tokens":3}},{extra}}}"#);
            let replies = [
                sse(&[chunk(r#"{"content":"cache-ok"}"#, None, None), chunk("{}", Some("stop"), Some(&usage))]),
                Reply::sse_raw(&format!("data: {}\n\ndata: {}\n\ndata: {{\"usage\":{usage}}}\n\ndata: [DONE]\n\n", chunk(r#"{"content":"cache-ok"}"#, None, None), chunk("{}", Some("stop"), None))),
                Reply::json(200, &format!(r#"{{"id":"cache","object":"chat.completion","created":1,"model":"m","choices":[{{"index":0,"message":{{"role":"assistant","content":"cache-ok"}},"finish_reason":"stop"}}],"usage":{usage}}}"#)),
            ];
            for reply in replies {
                let mock = Mock::script(vec![reply]);
                let outcome = rust.run("s", &request(&mock, family, "m", jobj! {}));
                let usage = outcome.result().get("usage").unwrap();
                let read = |key: &str| usage.get(key).and_then(Json::as_u64);
                assert_eq!(read("cacheReadTokens"), Some(cached), "{name}");
                assert_eq!((read("inputTokens"), read("outputTokens"), read("totalTokens"), read("reasoningTokens")), (Some(100), Some(10), Some(110), Some(3)), "{name}");
            }
        }
    }
}

#[test]
fn appended_tools_and_system_prompts_fall_back_into_what_chat_has() {
    let mock = Mock::script(vec![ok_stream("x")]);
    run_one(&mock, "openai-compatible", "m", jobj! {
        "toolAppend": true,
        "tools": vec![jobj! { "name": "ls", "description": "d", "inputSchema": jobj! { "type": "object" } }, jobj! { "name": "late", "description": "d", "inputSchema": jobj! { "type": "object" } }],
        "messages": vec![
            jobj! { "role": "user", "content": "q" },
            jobj! { "role": "system", "content": "", "providerOptions": jobj! { "mewrk": jobj! { "toolAddition": vec![Json::from("late")] } } },
        ],
    });
    let body = mock.last_request().json();
    assert!(body.get("messages").unwrap().as_arr().unwrap().iter().all(|m| m.str_at("role") != Some("system")));
    // Chat is non-strict when absent, so no `strict` goes out.
    assert!(body.get("tools").unwrap().as_arr().unwrap().iter().all(|t| t.path(&["function", "strict"]).is_none()), "{body}");
    assert_eq!(body.get("tools").unwrap().as_arr().unwrap().len(), 2);

    let mock = Mock::script(vec![ok_stream("x")]);
    run_one(&mock, "openai-compatible", "m", jobj! {
        "systemAppend": true,
        "messages": vec![
            jobj! { "role": "user", "content": "q" },
            jobj! { "role": "system", "content": "plan", "providerOptions": jobj! { "mewrk": jobj! { "systemAppend": true } } },
        ],
    });
    assert_eq!(mock.last_request().json().get("messages").unwrap().to_text(), r#"[{"role":"user","content":"q"},{"role":"system","content":"plan"}]"#);
}

#[test]
fn native_search_has_no_chat_factory() {
    let mock = Mock::script(vec![ok_stream("x")]);
    run_one(&mock, "openai-chat", "m", jobj! { "nativeSearch": jobj! { "maxUses": 2u64 } });
    assert!(mock.last_request().json().get("tools").is_none());
}

#[test]
fn credentials_stay_out_of_upstream_errors() {
    let mock = Mock::script(vec![Reply::json(401, r#"{"error":{"message":"diagnostic sk-test-secret"}}"#)]);
    let outcome = run_one(&mock, "openai-compatible", "m", jobj! {});
    assert_eq!(outcome.error().str_at("message"), Some("diagnostic [redacted]"));
}
