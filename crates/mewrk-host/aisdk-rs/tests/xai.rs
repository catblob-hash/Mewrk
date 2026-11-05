//! The `xai` family against a mock upstream on a real socket.

mod common;

use common::{Mock, Outcome, Reply, Rust};
use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;

fn request(mock: &Mock, model: &str, extra: Json) -> Json {
    let mut payload = jobj! {
        "family": "xai",
        "baseURL": format!("{}/v1", mock.base),
        "apiKey": "xai-test-secret",
        "modelId": model,
        "messages": vec![jobj! { "role": "user", "content": "hi" }],
        "maxSteps": 1u64,
    };
    if let (Some(object), Some(extra)) = (payload.as_obj_mut(), extra.as_obj()) {
        object.extend_from(extra);
    }
    payload
}

fn sse(events: &[String]) -> Reply {
    Reply::sse(&events.iter().map(String::as_str).collect::<Vec<_>>())
}

fn run_one(mock: &Mock, model: &str, extra: Json) -> Outcome {
    Rust::start().run("s", &request(mock, model, extra))
}

const USAGE: &str = r#"{"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":2},"cost_in_usd_ticks":123}"#;

fn created() -> String {
    r#"{"type":"response.created","response":{"id":"resp_1","model":"grok-served","object":"response","output":[]}}"#.to_owned()
}

fn completed() -> String {
    format!(r#"{{"type":"response.completed","response":{{"object":"response","output":[],"status":"completed","usage":{USAGE}}}}}"#)
}

fn delta(text: &str) -> String {
    format!(r#"{{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"{text}"}}"#)
}

fn text_stream(text: &str) -> Reply {
    sse(&[created(), delta(text), completed()])
}

fn function_call(id: &str, name: &str, arguments: &str) -> [String; 2] {
    [
        format!(r#"{{"type":"response.output_item.added","output_index":1,"item":{{"type":"function_call","id":"fc_{id}","call_id":"{id}","name":"{name}","arguments":""}}}}"#),
        format!(r#"{{"type":"response.output_item.done","output_index":1,"item":{{"type":"function_call","id":"fc_{id}","call_id":"{id}","name":"{name}","arguments":{}}}}}"#, Json::from(arguments).to_text()),
    ]
}

#[test]
fn text_streaming_and_the_request_shape() {
    let mock = Mock::script(vec![text_stream("hello")]);
    let outcome = run_one(&mock, "grok-4.3", jobj! {
        "system": "S",
        "maxOutputTokens": 9u64,
        "tools": vec![jobj! { "name": "read", "description": "d", "inputSchema": jobj! {
            "type": "object",
            "properties": jobj! { "inner": jobj! { "type": "object", "additionalProperties": false }, "open": jobj! { "additionalProperties": true } },
            "additionalProperties": false,
        } }],
    });
    assert_eq!(outcome.text(), "hello");
    let result = outcome.result();
    assert_eq!(result.str_at("finishReason"), Some("stop"));
    assert_eq!(result.str_at("rawFinishReason"), Some("completed"));
    assert_eq!(result.str_at("model"), Some("grok-served"));
    assert_eq!(result.get("usage").unwrap().to_text(), r#"{"inputTokens":10,"outputTokens":5,"totalTokens":15,"reasoningTokens":2,"cacheReadTokens":4}"#);
    assert_eq!(result.get("responseMessages").unwrap().to_text(), r#"[{"role":"assistant","content":[{"type":"text","text":"hello"}]}]"#);

    let seen = mock.last_request();
    assert_eq!((seen.method.as_str(), seen.target.as_str()), ("POST", "/v1/responses"));
    assert_eq!(seen.header("authorization"), Some("Bearer xai-test-secret"));
    assert_eq!(seen.header("user-agent"), Some("ai-sdk/xai/4.0.47 ai-sdk/provider-utils/5.0.32 runtime/rust"));
    // Responses tools say `strict: false` for the same reason OpenAI's do, and xAI takes no `additionalProperties: false`.
    assert_eq!(
        seen.body,
        r#"{"model":"grok-4.3","input":[{"role":"system","content":"S"},{"role":"user","content":[{"type":"input_text","text":"hi"}]}],"max_output_tokens":9,"tools":[{"type":"function","name":"read","description":"d","parameters":{"type":"object","properties":{"inner":{"type":"object"},"open":{"additionalProperties":true}}},"strict":false}],"tool_choice":"auto","stream":true}"#
    );
}

#[test]
fn an_empty_key_still_sends_the_header() {
    let mock = Mock::script(vec![text_stream("x")]);
    run_one(&mock, "grok-4.3", jobj! { "apiKey": "" });
    assert_eq!(mock.last_request().header("authorization"), Some("Bearer"));
}

#[test]
fn encrypted_reasoning_and_tool_calls_round_trip() {
    let [added, done] = function_call("call_1", "read", r#"{"path":"a"}"#);
    let mock = Mock::script(vec![
        sse(&[
            created(),
            r#"{"type":"response.reasoning_summary_part.added","item_id":"rs_1","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#.to_owned(),
            r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"Let me think"}"#.to_owned(),
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"status":"completed","encrypted_content":"ENC"}}"#.to_owned(),
            added,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_call_1","output_index":1,"delta":"{\"path\":\"a\"}"}"#.to_owned(),
            done,
            completed(),
        ]),
        text_stream("done"),
    ]);
    let rust = Rust::start();
    let tools = vec![jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object" } }];
    let outcome = rust.run("a", &request(&mock, "grok-4.3", jobj! { "tools": tools.clone(), "reasoningContent": "plaintext", "reasoning": "high" }));
    assert_eq!(mock.last_request().json().get("reasoning").unwrap().to_text(), r#"{"effort":"high"}"#);
    let deltas: String = outcome.events_of("reasoning-delta").iter().filter_map(|e| e.str_at("delta")).collect();
    assert_eq!(deltas, "Let me think");
    assert_eq!(outcome.events_of("tool-call-announced")[0].to_text(), r#"{"k":"tool-call-announced","callId":"call_1","toolName":"read"}"#);
    let result = outcome.result();
    assert_eq!(result.str_at("finishReason"), Some("tool-calls"));
    assert_eq!(result.str_at("rawFinishReason"), Some("completed"));
    assert_eq!(result.get("calls").unwrap().to_text(), r#"[{"callId":"call_1","toolName":"read","input":{"path":"a"}}]"#);
    let messages = result.get("responseMessages").unwrap().clone();
    assert_eq!(
        messages.to_text(),
        r#"[{"role":"assistant","content":[{"type":"reasoning","text":"Let me think","providerOptions":{"xai":{"reasoningEncryptedContent":"ENC","itemId":"rs_1"}}},{"type":"tool-call","toolCallId":"call_1","toolName":"read","input":{"path":"a"}}]}]"#
    );

    // What was persisted goes back as the items it came from.
    let mut history = vec![jobj! { "role": "user", "content": "hi" }];
    history.extend(messages.as_arr().unwrap().iter().cloned());
    history.push(jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "call_1", "toolName": "read", "output": jobj! { "type": "text", "value": "body" } }] });
    rust.run("b", &request(&mock, "grok-4.3", jobj! { "tools": tools, "messages": history }));
    assert_eq!(
        mock.last_request().json().get("input").unwrap().to_text(),
        concat!(
            r#"[{"role":"user","content":[{"type":"input_text","text":"hi"}]},"#,
            r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Let me think"}],"status":"completed","encrypted_content":"ENC"},"#,
            r#"{"type":"function_call","id":"call_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a\"}","status":"completed"},"#,
            r#"{"type":"function_call_output","call_id":"call_1","output":"body"}]"#,
        )
    );
}

#[test]
fn reasoning_without_a_summary_opens_at_its_item() {
    let mock = Mock::script(vec![sse(&[
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_e","summary":[],"status":"completed","encrypted_content":"OPAQUE"}}"#.to_owned(),
        r#"{"type":"response.reasoning_text.delta","item_id":"rs_t","output_index":1,"content_index":0,"delta":"raw"}"#.to_owned(),
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_t","summary":[],"status":"completed"}}"#.to_owned(),
        completed(),
    ])]);
    let result = run_one(&mock, "grok-4.3", jobj! { "reasoningContent": "encrypted" }).result().clone();
    assert_eq!(result.get("reasoning").unwrap().to_text(), r#"["","raw"]"#);
    assert_eq!(
        result.get("responseMessages").unwrap().to_text(),
        r#"[{"role":"assistant","content":[{"type":"reasoning","text":"","providerOptions":{"xai":{"reasoningEncryptedContent":"OPAQUE","itemId":"rs_e"}}},{"type":"reasoning","text":"raw","providerOptions":{"xai":{"itemId":"rs_t"}}}]}]"#
    );
}

#[test]
fn reasoning_levels_follow_the_model() {
    let effort = |model: &str, level: &str| {
        let mock = Mock::script(vec![text_stream("x")]);
        run_one(&mock, model, jobj! { "reasoning": level });
        mock.last_request().json().path(&["reasoning", "effort"]).and_then(Json::as_str).map(str::to_owned)
    };
    assert_eq!(effort("grok-4.3", "max").as_deref(), Some("xhigh"));
    assert_eq!(effort("grok-4.3", "low").as_deref(), Some("low"));
    assert_eq!(effort("grok-3-mini", "high").as_deref(), Some("high"));
    assert_eq!(effort("grok-3-mini", "medium").as_deref(), Some("low"));
    assert_eq!(effort("grok-4.20-reasoning", "high"), None);
    assert_eq!(effort("grok-2", "high"), None);
}

#[test]
fn provider_options_become_request_fields() {
    let mock = Mock::script(vec![text_stream("x")]);
    run_one(&mock, "grok-4.3", jobj! { "providerOptions": jobj! { "xai": jobj! {
        "reasoningEffort": "none",
        "reasoningSummary": "detailed",
        "topLogprobs": 2u64,
        "serviceTier": "priority",
        "store": false,
        "previousResponseId": "resp_0",
        "include": vec![Json::from("file_search_call.results")],
    } } });
    assert_eq!(
        mock.last_request().body,
        r#"{"model":"grok-4.3","input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}],"logprobs":true,"top_logprobs":2,"reasoning":{"effort":"none","summary":"detailed"},"store":false,"include":["file_search_call.results","reasoning.encrypted_content"],"previous_response_id":"resp_0","service_tier":"priority","stream":true}"#
    );

    let mock = Mock::script(vec![text_stream("x")]);
    let outcome = run_one(&mock, "grok-4.3", jobj! { "providerOptions": jobj! { "xai": jobj! { "reasoningEffort": "max" } } });
    assert_eq!(outcome.error().to_text(), r#"{"kind":"permanent","message":"invalid xai provider options"}"#);
    assert!(mock.requests().is_empty());
}

#[test]
fn files_map_to_images_urls_and_references() {
    let mock = Mock::script(vec![text_stream("x")]);
    run_one(&mock, "grok-4.3", jobj! { "messages": vec![jobj! { "role": "user", "content": vec![
        jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png", "providerOptions": jobj! { "xai": jobj! { "imageDetail": "low" } } },
        jobj! { "type": "file", "data": jobj! { "type": "url", "url": "https://files.example/a.png" }, "mediaType": "image/png" },
        jobj! { "type": "file", "data": jobj! { "type": "url", "url": "https://files.example/a.pdf" }, "mediaType": "application/pdf" },
        jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "xai": "file-1" } }, "mediaType": "application/pdf" },
    ] }] });
    assert_eq!(
        mock.last_request().json().get("input").unwrap().to_text(),
        r#"[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo=","detail":"low"},{"type":"input_image","image_url":"https://files.example/a.png"},{"type":"input_file","file_url":"https://files.example/a.pdf"},{"type":"input_file","file_id":"file-1"}]}]"#
    );

    let mock = Mock::script(vec![text_stream("x")]);
    let outcome = run_one(&mock, "grok-4.3", jobj! { "messages": vec![jobj! { "role": "user", "content": vec![jobj! { "type": "file", "data": "JVBERg==", "mediaType": "application/pdf" }] }] });
    assert_eq!(outcome.error().str_at("message"), Some("'file part media type application/pdf as inline data (xAI Responses requires a URL or a Files API reference for non-image files)' functionality not supported."));
    assert!(mock.requests().is_empty());
}

#[test]
fn native_search_runs_on_the_server_and_cites_its_sources() {
    let mock = Mock::script(vec![sse(&[
        created(),
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress","name":"web_search","arguments":"{\"query\":\"q\"}"}}"#.to_owned(),
        r#"{"type":"response.web_search_call.searching","item_id":"ws_1","output_index":0}"#.to_owned(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed","name":"web_search","arguments":"{\"query\":\"q\"}"}}"#.to_owned(),
        delta("found"),
        r#"{"type":"response.output_text.annotation.added","item_id":"msg_1","output_index":0,"content_index":0,"annotation_index":0,"annotation":{"type":"url_citation","url":"https://a.example/x","title":"A"}}"#.to_owned(),
        r#"{"type":"response.output_text.annotation.added","item_id":"msg_1","output_index":0,"content_index":0,"annotation_index":1,"annotation":{"type":"url_citation","url":"https://b.example/y"}}"#.to_owned(),
        completed(),
    ])]);
    let outcome = run_one(&mock, "grok-4.3", jobj! { "nativeSearch": jobj! { "maxUses": 3u64 } });
    let result = outcome.result();
    assert_eq!(result.str_at("text"), Some("found"));
    assert_eq!(result.str_at("finishReason"), Some("stop"));
    assert_eq!(result.get("nativeSearchUses").and_then(Json::as_u64), Some(1));
    assert_eq!(result.get("nativeSearchCallIds").unwrap().to_text(), r#"["ws_1"]"#);
    let sources = result.get("sources").unwrap().as_arr().unwrap();
    assert_eq!(sources.iter().map(|s| (s.str_at("url").unwrap(), s.str_at("title").unwrap())).collect::<Vec<_>>(), [("https://a.example/x", "A"), ("https://b.example/y", "https://b.example/y")]);
    // `xai.web_search`'s input schema keeps none of the call's arguments.
    assert_eq!(
        result.get("responseMessages").unwrap().to_text(),
        r#"[{"role":"assistant","content":[{"type":"tool-call","toolCallId":"ws_1","toolName":"web_search","input":{},"providerExecuted":true},{"type":"tool-result","toolCallId":"ws_1","toolName":"web_search","output":{"type":"json","value":{}}},{"type":"text","text":"found"}]}]"#
    );
    assert_eq!(mock.last_request().json().get("tools").unwrap().to_text(), r#"[{"type":"web_search"}]"#);
    // Forcing a server tool is not something the API has.
    assert_eq!(mock.last_request().json().str_at("tool_choice"), Some("auto"));
}

#[test]
fn a_replayed_search_is_not_sent_back() {
    let mock = Mock::script(vec![text_stream("x")]);
    run_one(&mock, "grok-4.3", jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "tool-call", "toolCallId": "ws_1", "toolName": "web_search", "input": jobj! {}, "providerExecuted": true },
            jobj! { "type": "tool-result", "toolCallId": "ws_1", "toolName": "web_search", "output": jobj! { "type": "json", "value": jobj! {} } },
            jobj! { "type": "text", "text": "found", "providerOptions": jobj! { "xai": jobj! { "itemId": "msg_0" } } },
        ] },
        jobj! { "role": "user", "content": "more" },
    ] });
    assert_eq!(
        mock.last_request().json().get("input").unwrap().to_text(),
        r#"[{"role":"user","content":[{"type":"input_text","text":"q"}]},{"role":"assistant","content":"found","id":"msg_0"},{"role":"user","content":[{"type":"input_text","text":"more"}]}]"#
    );
}

#[test]
fn finish_reasons_and_usage_mapping() {
    let finish = |events: &[String]| {
        let mock = Mock::script(vec![sse(events)]);
        let outcome = run_one(&mock, "grok-4.3", jobj! {});
        let result = outcome.result();
        (result.str_at("finishReason").map(str::to_owned), result.str_at("rawFinishReason").map(str::to_owned), result.get("usage").unwrap().to_text())
    };
    let incomplete = |reason: &str| format!(r#"{{"type":"response.incomplete","response":{{"incomplete_details":{{"reason":"{reason}"}}}}}}"#);
    let zero = r#"{"inputTokens":0,"outputTokens":0,"totalTokens":0,"reasoningTokens":0,"cacheReadTokens":0}"#.to_owned();
    assert_eq!(finish(&[incomplete("max_output_tokens")]), (Some("length".into()), Some("max_output_tokens".into()), zero.clone()));
    assert_eq!(finish(&[incomplete("content_filter")]), (Some("content-filter".into()), Some("content_filter".into()), zero.clone()));
    assert_eq!(finish(&[r#"{"type":"response.incomplete","response":{}}"#.to_owned()]), (Some("other".into()), Some("incomplete".into()), zero.clone()));
    // A stream that just ends is `other`, not an error, with the zero usage the model reports for it.
    assert_eq!(finish(&[created()]), (Some("other".into()), None, zero));
    // Cached tokens reported on top of the input are added to it.
    let on_top = format!(r#"{{"type":"response.completed","response":{{"object":"response","output":[],"status":"completed","usage":{}}}}}"#, r#"{"input_tokens":2,"output_tokens":1,"input_tokens_details":{"cached_tokens":8}}"#);
    assert_eq!(finish(&[on_top]).2, r#"{"inputTokens":10,"outputTokens":1,"totalTokens":11,"reasoningTokens":0,"cacheReadTokens":8}"#);
}

#[test]
fn errors_classify_by_status_and_carry_retry_after() {
    let failed = |reply: Reply| {
        let mock = Mock::script(vec![reply]);
        run_one(&mock, "grok-4.3", jobj! {}).error().to_text()
    };
    assert_eq!(failed(Reply::json(429, r#"{"error":{"message":"slow"}}"#).header("retry-after", "7")), r#"{"kind":"transient","message":"slow","status":429,"retryAfterMs":7000}"#);
    assert_eq!(failed(Reply::json(400, r#"{"code":"invalid_argument","error":"bad field"}"#)), r#"{"kind":"permanent","message":"invalid_argument: bad field","status":400}"#);
    assert_eq!(failed(Reply::json(403, r#"{"error":"forbidden"}"#)), r#"{"kind":"permanent","message":"forbidden","status":403}"#);
    assert_eq!(failed(Reply::json(422, r#"{"message":"flat"}"#)), r#"{"kind":"permanent","message":"Unprocessable Entity","status":422}"#);
    assert_eq!(failed(Reply::new(503)), r#"{"kind":"transient","message":"Service Unavailable","status":503}"#);
}

#[test]
fn errors_inside_the_stream() {
    let failed = |events: &[String]| {
        let mock = Mock::script(vec![sse(events)]);
        run_one(&mock, "grok-4.3", jobj! {}).error().to_text()
    };
    let event = |code: &str| format!(r#"{{"type":"error","code":"{code}","message":"m"}}"#);
    assert_eq!(failed(&[event("rate_limit_exceeded")]), r#"{"kind":"transient","message":"m","status":429}"#);
    // Quota is not coming back on a retry, whatever its status says.
    assert_eq!(failed(&[event("insufficient_quota")]), r#"{"kind":"permanent","message":"m","status":429}"#);
    assert_eq!(failed(&[event("service_unavailable")]), r#"{"kind":"transient","message":"m","status":503}"#);
    assert_eq!(failed(&[event("invalid_api_key")]), r#"{"kind":"permanent","message":"m","status":401}"#);
    assert_eq!(failed(&[event("503")]), r#"{"kind":"transient","message":"m","status":503}"#);
    assert_eq!(failed(&[event("unknown_code")]), r#"{"kind":"permanent","message":"m"}"#);
    // `classify` reads the transient forms out of a code with no status of its own.
    assert_eq!(failed(&[event("please_try_again")]), r#"{"kind":"transient","message":"m"}"#);
    assert_eq!(failed(&[r#"{"type":"error","message":"Overloaded"}"#.to_owned()]), r#"{"kind":"transient","message":"Overloaded","status":503}"#);
    assert_eq!(
        failed(&[created(), r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"boom"}}}"#.to_owned()]),
        r#"{"kind":"transient","message":"boom","status":500}"#
    );
    // A chunk outside the union fails the turn: the SDK's schema names every event it takes.
    for event in [
        r#"{"type":"response.brand_new"}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"brand_new"}}"#,
        r#"{"type":"response.completed","response":{"object":"response","output":[]}}"#,
    ] {
        let text = failed(&[event.to_owned()]);
        assert!(text.starts_with(r#"{"kind":"permanent","message":"Type validation failed: Value: "#), "{text}");
    }
    // Truncation is transient, a failed response with no error only a finish reason.
    let mock = Mock::script(vec![Reply::sse_raw(&format!("data: {}\n\n", delta("par"))).truncated()]);
    let cut = run_one(&mock, "grok-4.3", jobj! {});
    assert_eq!(cut.text(), "par");
    assert_eq!(cut.error().to_text(), r#"{"kind":"transient","message":"UND_ERR_SOCKET: terminated"}"#);
    let mock = Mock::script(vec![sse(&[r#"{"type":"response.failed","response":{}}"#.to_owned()])]);
    let outcome = run_one(&mock, "grok-4.3", jobj! {});
    assert_eq!(outcome.result().str_at("finishReason"), Some("error"));
}

#[test]
fn credentials_stay_out_of_upstream_errors() {
    let mock = Mock::script(vec![Reply::json(401, r#"{"error":"bad key xai-test-secret"}"#)]);
    assert_eq!(run_one(&mock, "grok-4.3", jobj! {}).error().str_at("message"), Some("bad key [redacted]"));
}
