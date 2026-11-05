//! The Bedrock family against a mock upstream on a real socket, and its signing and tool preparation against the
//! real SDK's output, recorded under node. No AWS endpoint is contacted: where a test names one, the fetch is a fake.

mod common;

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Mock, Reply, Rust};
use mewrk_aisdk::error::AiError;
use mewrk_aisdk::http::{Body, BoxFuture, Cancel, DynFetch, Fetch, Headers, HttpRequest, HttpResponse};
use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;
use mewrk_aisdk::model::{CallOptions, DynModel};
use mewrk_aisdk::providers::ProviderSettings;
use mewrk_aisdk::providers::bedrock::eventstream::{HeaderValue, encode_frame};
use mewrk_aisdk::providers::bedrock::sigv4::Credentials;
use mewrk_aisdk::providers::bedrock::{create, create_signed};

const OPUS: &str = "us.anthropic.claude-opus-5";
const SONNET: &str = "anthropic.claude-sonnet-4-5-20250929-v1:0";
const NOVA: &str = "us.amazon.nova-pro-v1:0";
const HAIKU: &str = "anthropic.claude-3-haiku-20240307-v1:0";

fn string(text: &str) -> HeaderValue {
    HeaderValue::Str(text.to_owned())
}

fn event(kind: &str, payload: &str) -> Vec<u8> {
    encode_frame(&[(":message-type", string("event")), (":event-type", string(kind)), (":content-type", string("application/json"))], payload.as_bytes())
}

fn exception(kind: &str, payload: &str) -> Vec<u8> {
    encode_frame(&[(":message-type", string("exception")), (":exception-type", string(kind)), (":content-type", string("application/json"))], payload.as_bytes())
}

fn frames(chunks: Vec<Vec<u8>>) -> Reply {
    let mut reply = Reply::new(200).header("content-type", "application/vnd.amazon.eventstream").header("x-amzn-requestid", "req-1");
    reply.chunks = chunks;
    reply
}

fn request(mock: &Mock, model: &str, extra: Json) -> Json {
    let mut payload = jobj! {
        "family": "bedrock",
        "baseURL": mock.base.as_str(),
        "apiKey": "bedrock-secret-key",
        "settings": jobj! { "region": "us-east-1" },
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

fn read_tool() -> Vec<Json> {
    vec![jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object" } }]
}

fn simple_stream() -> Reply {
    frames(vec![
        event("messageStart", r#"{"role":"assistant","p":"abcdefg"}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"hi"},"p":"abc"}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":" there"}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
        event("metadata", r#"{"usage":{"inputTokens":3,"outputTokens":2,"totalTokens":5,"cacheReadInputTokens":20,"cacheWriteInputTokens":7},"metrics":{"latencyMs":1}}"#),
    ])
}

#[test]
fn text_streaming_and_the_request_shape() {
    let mock = Mock::script(vec![simple_stream()]);
    let rust = Rust::start();
    let outcome = rust.run("s", &request(&mock, SONNET, jobj! { "maxOutputTokens": 100u64, "tools": read_tool() }));
    assert_eq!(outcome.text(), "hi there");
    let result = outcome.result();
    assert_eq!(result.str_at("text"), Some("hi there"));
    assert_eq!(result.str_at("finishReason"), Some("stop"));
    assert_eq!(result.str_at("rawFinishReason"), Some("end_turn"));
    assert_eq!(result.str_at("model"), Some(SONNET));
    // Cache reads and writes count into the input total, and Converse has no reasoning count.
    assert_eq!(result.get("usage").unwrap().to_text(), r#"{"inputTokens":30,"outputTokens":2,"totalTokens":32,"cacheReadTokens":20}"#);
    assert_eq!(result.get("responseMessages").unwrap().to_text(), r#"[{"role":"assistant","content":[{"type":"text","text":"hi there"}]}]"#);

    let seen = mock.last_request();
    assert_eq!(seen.method, "POST");
    // `encodeURIComponent` of the model id.
    assert_eq!(seen.target, "/model/anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse-stream");
    assert_eq!(seen.header("authorization"), Some("Bearer bedrock-secret-key"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    // The SDK adds its suffix twice: in the provider's headers and in the fetch wrapper.
    assert_eq!(seen.header("user-agent"), Some("ai-sdk/amazon-bedrock/5.0.66 ai-sdk/provider-utils/5.0.32 runtime/rust ai-sdk/amazon-bedrock/5.0.66 runtime/rust"));
    assert_eq!(
        seen.body,
        r#"{"system":[{"text":"S"}],"messages":[{"role":"user","content":[{"text":"hello"}]}],"additionalModelResponseFieldPaths":["/delta/stop_sequence"],"inferenceConfig":{"maxTokens":100},"toolConfig":{"tools":[{"toolSpec":{"name":"read","description":"d","inputSchema":{"json":{"type":"object"}}}}],"toolChoice":{"auto":{}}}}"#
    );
}

#[test]
fn the_default_ceiling_and_a_model_without_a_vendor_rule() {
    let mock = Mock::script(vec![simple_stream()]);
    let rust = Rust::start();
    rust.run("s", &request(&mock, NOVA, jobj! {}));
    assert_eq!(mock.last_request().body, r#"{"system":[{"text":"S"}],"messages":[{"role":"user","content":[{"text":"hello"}]}],"inferenceConfig":{"maxTokens":32000}}"#);
}

fn signed_stream() -> Reply {
    frames(vec![
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"Let me "}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"think"}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"SIG"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"data":"REDACTED"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":1}"#),
        event("contentBlockStart", r#"{"contentBlockIndex":2,"start":{"toolUse":{"toolUseId":"tooluse_1","name":"read"}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":2,"delta":{"toolUse":{"input":"{\"pa"}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":2,"delta":{"toolUse":{"input":"th\":\"a\"}"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":2}"#),
        event("messageStop", r#"{"stopReason":"tool_use"}"#),
        event("metadata", r#"{"usage":{"inputTokens":3,"outputTokens":9}}"#),
    ])
}

#[test]
fn signed_reasoning_streams_and_replays_through_converse() {
    let mock = Mock::script(vec![signed_stream(), simple_stream()]);
    let rust = Rust::start();
    let outcome = rust.run("s", &request(&mock, OPUS, jobj! { "tools": read_tool(), "reasoning": "high", "reasoningContent": "plaintext" }));
    // The level became Bedrock's own thinking fields.
    let body = mock.last_request().json();
    assert_eq!(body.get("additionalModelRequestFields").unwrap().to_text(), r#"{"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"high"}}"#);

    // Converse delivers redacted thinking as a delta, not on a block start as Messages does, and a plaintext stream
    // gives the host no card for an item with no text. It still replays, below.
    let starts = outcome.events_of("reasoning-start");
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].to_text(), r#"{"k":"reasoning-start","item":0,"form":"plaintext"}"#);
    let deltas: String = outcome.events_of("reasoning-delta").iter().filter_map(|e| e.str_at("delta")).collect();
    assert_eq!(deltas, "Let me think");
    assert_eq!(outcome.events_of("reasoning-done").len(), 1);
    assert_eq!(outcome.events_of("tool-call-announced")[0].to_text(), r#"{"k":"tool-call-announced","callId":"tooluse_1","toolName":"read"}"#);

    let result = outcome.result();
    assert_eq!(result.str_at("finishReason"), Some("tool-calls"));
    assert_eq!(result.str_at("rawFinishReason"), Some("tool_use"));
    assert_eq!(result.get("calls").unwrap().to_text(), r#"[{"callId":"tooluse_1","toolName":"read","input":{"path":"a"}}]"#);
    assert_eq!(result.get("reasoning").unwrap().to_text(), r#"["Let me think"]"#);
    assert_eq!(result.get("usage").unwrap().to_text(), r#"{"inputTokens":3,"outputTokens":9,"totalTokens":12,"cacheReadTokens":0}"#);
    let messages = result.get("responseMessages").unwrap();
    assert_eq!(
        messages.to_text(),
        r#"[{"role":"assistant","content":[{"type":"reasoning","text":"Let me think","providerOptions":{"amazonBedrock":{"signature":"SIG"},"bedrock":{"signature":"SIG"}}},{"type":"reasoning","text":"","providerOptions":{"amazonBedrock":{"redactedData":"REDACTED"},"bedrock":{"redactedData":"REDACTED"}}},{"type":"tool-call","toolCallId":"tooluse_1","toolName":"read","input":{"path":"a"}}]}]"#
    );

    // What the host persisted goes back verbatim, and comes out as Converse's own reasoning blocks.
    let mut history = vec![jobj! { "role": "user", "content": "hello" }];
    history.extend(messages.as_arr().unwrap().iter().cloned());
    history.push(jobj! {
        "role": "tool",
        "content": vec![jobj! { "type": "tool-result", "toolCallId": "tooluse_1", "toolName": "read", "output": jobj! { "type": "text", "value": "contents" } }],
    });
    rust.run("t", &request(&mock, OPUS, jobj! { "tools": read_tool(), "messages": history }));
    assert_eq!(
        mock.last_request().json().get("messages").unwrap().to_text(),
        r#"[{"role":"user","content":[{"text":"hello"}]},{"role":"assistant","content":[{"reasoningContent":{"reasoningText":{"text":"Let me think","signature":"SIG"}}},{"reasoningContent":{"redactedReasoning":{"data":"REDACTED"}}},{"toolUse":{"toolUseId":"tooluse_1","name":"read","input":{"path":"a"}}}]},{"role":"user","content":[{"toolResult":{"toolUseId":"tooluse_1","content":[{"text":"contents"}]}}]}]"#
    );
}

#[test]
fn reasoning_signed_by_another_model_and_unsigned_reasoning_stay_home() {
    let mock = Mock::script(vec![simple_stream()]);
    let rust = Rust::start();
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": " mine ", "providerOptions": jobj! { "bedrock": jobj! { "signature": "A" }, "mewrk": jobj! { "model": OPUS } } },
            jobj! { "type": "reasoning", "text": "theirs", "providerOptions": jobj! { "bedrock": jobj! { "signature": "B" }, "mewrk": jobj! { "model": SONNET } } },
            jobj! { "type": "reasoning", "text": "bare" },
            jobj! { "type": "text", "text": "answer  " },
        ] },
        jobj! { "role": "user", "content": "again" },
    ];
    rust.run("s", &request(&mock, OPUS, jobj! { "messages": history }));
    assert_eq!(
        mock.last_request().json().get("messages").unwrap().to_text(),
        r#"[{"role":"user","content":[{"text":"q"}]},{"role":"assistant","content":[{"reasoningContent":{"reasoningText":{"text":" mine ","signature":"A"}}},{"text":"answer  "}]},{"role":"user","content":[{"text":"again"}]}]"#
    );
}

#[test]
fn encrypted_form_gives_every_item_a_card() {
    let mock = Mock::script(vec![signed_stream()]);
    let rust = Rust::start();
    let outcome = rust.run("s", &request(&mock, OPUS, jobj! { "tools": read_tool(), "reasoningContent": "encrypted" }));
    assert_eq!(outcome.events_of("reasoning-start").len(), 2);
    assert_eq!(outcome.events_of("reasoning-done").len(), 2);
    assert_eq!(outcome.result().get("reasoning").unwrap().to_text(), r#"["Let me think",""]"#);
}

#[test]
fn a_budget_model_carves_thinking_out_of_the_ceiling() {
    let mock = Mock::script(vec![simple_stream()]);
    let rust = Rust::start();
    rust.run("s", &request(&mock, SONNET, jobj! { "reasoning": "medium", "maxOutputTokens": 32000u64 }));
    let body = mock.last_request().json();
    assert_eq!(body.get("additionalModelRequestFields").unwrap().to_text(), r#"{"thinking":{"type":"enabled","budget_tokens":9600}}"#);
    // reasoning.rs took the budget off the ceiling, and the SDK adds it back on: the sum is what was asked for.
    assert_eq!(body.get("inferenceConfig").unwrap().to_text(), r#"{"maxTokens":32000}"#);
}

#[test]
fn finish_reasons_reach_the_result() {
    for (raw, unified) in [
        ("end_turn", "stop"),
        ("stop_sequence", "stop"),
        ("max_tokens", "length"),
        ("guardrail_intervened", "content-filter"),
        ("content_filtered", "content-filter"),
        ("tool_use", "tool-calls"),
        ("malformed_model_output", "other"),
    ] {
        let mock = Mock::script(vec![frames(vec![
            event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#),
            event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            event("messageStop", &format!(r#"{{"stopReason":"{raw}"}}"#)),
        ])]);
        let rust = Rust::start();
        let outcome = rust.run("s", &request(&mock, SONNET, jobj! {}));
        assert_eq!(outcome.result().str_at("finishReason"), Some(unified), "{raw}");
        assert_eq!(outcome.result().str_at("rawFinishReason"), Some(raw), "{raw}");
    }
}

#[test]
fn http_errors_classify_with_retry_after_and_redaction() {
    let run = |reply: Reply| {
        let mock = Mock::script(vec![reply]);
        let rust = Rust::start();
        rust.run("s", &request(&mock, SONNET, jobj! {})).error().to_text()
    };
    // Bedrock's own error bodies have no `type`, and upstream's template prints that faithfully.
    assert_eq!(
        run(Reply::json(429, r#"{"message":"Too many requests"}"#).header("retry-after", "7")),
        r#"{"kind":"transient","message":"undefined: Too many requests","status":429,"retryAfterMs":7000}"#
    );
    assert_eq!(run(Reply::json(400, r#"{"message":"Bad","type":"ValidationException"}"#)), r#"{"kind":"permanent","message":"ValidationException: Bad","status":400}"#);
    assert_eq!(run(Reply::json(403, r#"{"Message":"denied"}"#)), r#"{"kind":"permanent","message":"Forbidden","status":403}"#);
    assert_eq!(run(Reply::new(503)), r#"{"kind":"transient","message":"Service Unavailable","status":503}"#);
    assert_eq!(
        run(Reply::json(401, r#"{"message":"key bedrock-secret-key rejected"}"#)),
        r#"{"kind":"permanent","message":"undefined: key [redacted] rejected","status":401}"#
    );
}

#[test]
fn stream_exceptions_carry_status_and_retryability() {
    for (kind, status, class) in [
        ("internalServerException", 500, "transient"),
        ("modelStreamErrorException", 424, "transient"),
        ("serviceUnavailableException", 503, "transient"),
        ("throttlingException", 429, "transient"),
        ("validationException", 400, "permanent"),
    ] {
        let mock = Mock::script(vec![frames(vec![
            event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"partial"}}"#),
            exception(kind, r#"{"message":"boom"}"#),
        ])
        .pause(Duration::from_millis(20))]);
        let rust = Rust::start();
        let outcome = rust.run("s", &request(&mock, SONNET, jobj! {}));
        assert_eq!(outcome.text(), "partial");
        assert_eq!(outcome.error().to_text(), format!(r#"{{"kind":"{class}","message":"boom","status":{status}}}"#));
    }
}

#[test]
fn an_exception_the_schema_does_not_name_is_a_no_op() {
    let mock = Mock::script(vec![frames(vec![
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#),
        exception("ThrottlingException", r#"{"message":"boom"}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
    ])]);
    let rust = Rust::start();
    assert_eq!(rust.run("s", &request(&mock, SONNET, jobj! {})).result().str_at("finishReason"), Some("stop"));
}

#[test]
fn a_chunk_failing_the_schema_reads_as_zod_wrote_it() {
    let mock = Mock::script(vec![frames(vec![event("contentBlockStop", "{}")])]);
    let rust = Rust::start();
    let outcome = rust.run("s", &request(&mock, SONNET, jobj! {}));
    assert_eq!(outcome.error().str_at("kind"), Some("permanent"));
    assert_eq!(
        outcome.error().str_at("message"),
        Some("Type validation failed: Value: {\"contentBlockStop\":{}}.\nError message: [\n  {\n    \"expected\": \"number\",\n    \"code\": \"invalid_type\",\n    \"path\": [\n      \"contentBlockStop\",\n      \"contentBlockIndex\"\n    ],\n    \"message\": \"Invalid input: expected number, received undefined\"\n  }\n]")
    );
}

/// Upstream throws inside a `TransformStream`, and an errored stream drops what is queued behind it, so whether a
/// delta decoded just before the failure survives there depends on timing. Here it always does: the step fails either
/// way, and the host retries it whole.
#[test]
fn decoding_failures_keep_earlier_frames() {
    let good = event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#);
    let mut bad = good.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    let mock = Mock::script(vec![frames(vec![[good, bad].concat()])]);
    let rust = Rust::start();
    let outcome = rust.run("s", &request(&mock, SONNET, jobj! {}));
    assert_eq!(outcome.text(), "t");
    let error = outcome.error();
    assert_eq!(error.str_at("kind"), Some("permanent"));
    assert!(error.str_at("message").unwrap().starts_with("The message checksum ("), "{error}");
}

#[test]
fn a_stream_cut_mid_frame_is_an_error() {
    let good = event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#);
    let mock = Mock::script(vec![frames(vec![good[..10].to_vec()])]);
    let rust = Rust::start();
    assert_eq!(
        rust.run("s", &request(&mock, SONNET, jobj! {})).error().to_text(),
        r#"{"kind":"permanent","message":"Incomplete Amazon Bedrock event-stream frame: 10 buffered bytes remain at end of stream."}"#
    );
    // A dropped connection is the transport's failure, which the host retries.
    let mock = Mock::script(vec![frames(vec![good[..10].to_vec()]).truncated()]);
    assert_eq!(rust.run("t", &request(&mock, SONNET, jobj! {})).error().str_at("kind"), Some("transient"));
}

#[test]
fn cancel_closes_the_socket_and_reports_cancelled() {
    let mock = Mock::script(vec![frames(vec![event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"first"}}"#)]).hanging()]);
    let rust = Rust::start();
    rust.send(&common::step_frame("s", &request(&mock, SONNET, jobj! {})));
    std::thread::sleep(Duration::from_millis(300));
    rust.send(r#"{"v":18,"type":"cancel","id":"s"}"#);
    let outcome = rust.wait("s", Duration::from_secs(5));
    assert_eq!(outcome.text(), "first");
    assert_eq!(outcome.error().to_text(), r#"{"kind":"cancelled","message":"请求已取消"}"#);
    std::thread::sleep(Duration::from_millis(200));
    assert!(*mock.client_gone.lock().unwrap());
}

#[test]
fn native_search_only_takes_the_newer_tool_on_bedrock() {
    let mock = Mock::script(vec![simple_stream()]);
    let rust = Rust::start();
    // The default version is dropped, leaving nothing to declare.
    rust.run("s", &request(&mock, OPUS, jobj! { "nativeSearch": jobj! { "maxUses": 3u64 } }));
    assert!(mock.last_request().json().get("toolConfig").is_none());
    rust.run("t", &request(&mock, OPUS, jobj! { "nativeSearch": jobj! { "maxUses": 3u64, "toolType": "web_search_20260209" } }));
    let body = mock.last_request().json();
    assert_eq!(
        body.get("toolConfig").unwrap().to_text(),
        r##"{"tools":[{"toolSpec":{"name":"web_search","inputSchema":{"json":{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}}}}]}"##
    );
    assert_eq!(body.get("additionalModelRequestFields").unwrap().to_text(), r#"{"tool_choice":{"type":"auto"},"anthropic_beta":["code-execution-web-tools-2026-02-09"]}"#);
}

#[test]
fn an_s3_url_goes_through_and_anything_else_is_the_downloader_s() {
    let mock = Mock::script(vec![simple_stream()]);
    let rust = Rust::start();
    let messages = vec![jobj! { "role": "user", "content": vec![
        jobj! { "type": "file", "data": "S3://bucket/a b.png", "mediaType": "image/png" },
        jobj! { "type": "file", "data": "s3://bucket/c.mp4", "mediaType": "video/mp4" },
    ] }];
    rust.run("s", &request(&mock, SONNET, jobj! { "messages": messages }));
    assert_eq!(
        mock.last_request().json().get("messages").unwrap().to_text(),
        r#"[{"role":"user","content":[{"image":{"format":"png","source":{"s3Location":{"uri":"s3://bucket/a%20b.png"}}}},{"video":{"format":"mp4","source":{"s3Location":{"uri":"s3://bucket/c.mp4"}}}}]}]"#
    );
    // Only `image/*` and `video/*` are taken as URLs, and the SDK's downloader will not fetch a private address.
    let messages = vec![jobj! { "role": "user", "content": vec![jobj! { "type": "file", "data": "http://127.0.0.1:9/a.png", "mediaType": "image/png" }] }];
    let outcome = rust.run("t", &request(&mock, SONNET, jobj! { "messages": messages }));
    assert_eq!(outcome.error().str_at("message"), Some("URL with IP address 127.0.0.1 is not allowed"));
}

// ------------------------------------------------------------------ Credentials

const SIGV4_MISSING: &str = "AWS SigV4 authentication requires AWS credentials. Please provide either:\n1. Set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY environment variables\n2. Provide accessKeyId and secretAccessKey in options\n3. Use a credentialProvider function\n4. Use API key authentication with AWS_BEARER_TOKEN_BEDROCK or apiKey option\nOriginal error: AWS access key ID setting is missing. Pass it using the 'accessKeyId' parameter or the AWS_ACCESS_KEY_ID environment variable.";
const REGION_MISSING: &str = "AWS region setting is missing. Pass it using the 'region' parameter or the AWS_REGION environment variable.";

fn assert_no_credentials() {
    let mock = Mock::script(Vec::new());
    let rust = Rust::start();
    for key in ["", "   ", "\t\n"] {
        let outcome = rust.run("s", &request(&mock, SONNET, jobj! { "apiKey": key }));
        assert_eq!(outcome.error().str_at("kind"), Some("permanent"));
        assert_eq!(outcome.error().str_at("message"), Some(SIGV4_MISSING));
    }
    let outcome = rust.run("t", &request(&mock, SONNET, jobj! { "apiKey": "", "settings": jobj! {} }));
    assert_eq!(outcome.error().str_at("message"), Some(REGION_MISSING));
    let no_endpoint = request(&mock, SONNET, jobj! { "settings": jobj! {} });
    let mut no_endpoint = no_endpoint.as_obj().unwrap().clone();
    no_endpoint.remove("baseURL");
    let outcome = rust.run("u", &Json::Obj(no_endpoint));
    assert_eq!(outcome.error().str_at("message"), Some(REGION_MISSING));
    assert!(mock.requests().is_empty(), "nothing is sent without credentials");
}

#[test]
fn an_empty_key_has_nothing_to_sign_with() {
    assert_no_credentials();
}

/// The sidecar ran with a cleared environment. This runs the same assertions in a child of this test binary whose
/// environment holds every variable the SDK would have looked at.
#[test]
fn the_aws_environment_is_never_read() {
    if std::env::var_os("MEWRK_BEDROCK_ENV_PROBE").is_some() {
        assert_eq!(std::env::var("AWS_ACCESS_KEY_ID").as_deref(), Ok("AKIDLEAK"));
        assert_no_credentials();
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "the_aws_environment_is_never_read", "--nocapture"])
        .env("MEWRK_BEDROCK_ENV_PROBE", "1")
        .env("AWS_ACCESS_KEY_ID", "AKIDLEAK")
        .env("AWS_SECRET_ACCESS_KEY", "leak")
        .env("AWS_SESSION_TOKEN", "leak")
        .env("AWS_REGION", "eu-west-9")
        .env("AWS_BEARER_TOKEN_BEDROCK", "leak")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "the child ran the probe");
}

// ------------------------------------------------------------------ Against the real SDK's recorded output

/// A `fetch` that records and refuses, so a test can name an AWS endpoint without reaching it.
#[derive(Default)]
struct Capture {
    seen: Mutex<Vec<HttpRequest>>,
}

impl Fetch for Capture {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        self.seen.lock().unwrap().push(request);
        Box::pin(async {
            let mut headers = Headers::new();
            headers.set("content-type", "application/json");
            Ok(HttpResponse { status: 400, status_text: "Bad Request".to_owned(), headers, body: Body::text(r#"{"message":"stop"}"#.to_owned()) })
        })
    }
}

fn settings(model: &str, base_url: Option<&str>, api_key: &str, headers: &[(&str, &str)], region: Option<&str>, fetch: DynFetch) -> ProviderSettings {
    ProviderSettings {
        base_url: base_url.map(str::to_owned),
        api_key: api_key.to_owned(),
        headers: Headers::from_pairs(headers.iter().copied()),
        settings: region.map(|region| ("region".to_owned(), region.to_owned())).into_iter().collect(),
        model_id: model.to_owned(),
        fetch,
    }
}

fn call(prompt: Vec<Json>, tools: Option<Vec<Json>>, tool_choice: Json, max: Option<u64>, reasoning: Option<&str>, provider_options: Option<Json>) -> CallOptions {
    CallOptions { prompt, tools, tool_choice, max_output_tokens: max, reasoning: reasoning.map(str::to_owned), provider_options, cancel: Cancel::new() }
}

fn stop_at_the_mock(model: &DynModel, options: CallOptions) {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let error = runtime.block_on(model.do_stream(options)).map(|_| ()).unwrap_err();
    assert_eq!(error.message(), "undefined: stop");
}

fn hello_prompt() -> Vec<Json> {
    vec![
        jobj! { "role": "system", "content": "S" },
        jobj! { "role": "user", "content": vec![jobj! { "type": "text", "text": "h\u{e9}llo" }] },
    ]
}

const RECORDED_USER_AGENT: &str = "ai-sdk/amazon-bedrock/5.0.66 ai-sdk/provider-utils/5.0.32 runtime/rust ai-sdk/amazon-bedrock/5.0.66 runtime/rust";

fn example_credentials(token: Option<&str>) -> Credentials {
    Credentials {
        region: "us-east-1".to_owned(),
        access_key_id: "AKIDEXAMPLE".to_owned(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
        session_token: token.map(str::to_owned),
    }
}

/// Recorded from `createAmazonBedrock({ region, accessKeyId, secretAccessKey, sessionToken, headers, fetch })` under
/// node with `Date` frozen at 2026-01-02T03:04:05.678Z: the signature covers the path, the body and the headers.
#[test]
fn the_signed_request_is_what_the_real_provider_signs() {
    let capture = Arc::new(Capture::default());
    let model = create_signed(
        settings(SONNET, None, "", &[("x-extra", "a   b")], None, capture.clone()),
        example_credentials(Some("SESSION")),
        Box::new(|| "20260102T030405Z".to_owned()),
    )
    .unwrap();
    stop_at_the_mock(&model, call(hello_prompt(), None, jobj! { "type": "auto" }, Some(100), None, None));
    let seen = capture.seen.lock().unwrap().pop().unwrap();
    assert_eq!(seen.url, "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse-stream");
    assert_eq!(seen.body.as_deref(), Some("{\"system\":[{\"text\":\"S\"}],\"messages\":[{\"role\":\"user\",\"content\":[{\"text\":\"h\u{e9}llo\"}]}],\"additionalModelResponseFieldPaths\":[\"/delta/stop_sequence\"],\"inferenceConfig\":{\"maxTokens\":100}}"));
    assert_eq!(
        seen.headers.get("authorization"),
        Some("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260102/us-east-1/bedrock/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token;x-extra, Signature=927a2cd6ca8ce262e8722a62066f424b791720ddb669eb59c9701cb586bf0644")
    );
    assert_eq!(seen.headers.get("x-amz-date"), Some("20260102T030405Z"));
    assert_eq!(seen.headers.get("x-amz-security-token"), Some("SESSION"));
    assert_eq!(seen.headers.get("x-extra"), Some("a   b"));
    assert_eq!(seen.headers.get("content-type"), Some("application/json"));
    assert_eq!(seen.headers.get("user-agent"), Some(RECORDED_USER_AGENT));
    assert_eq!(seen.headers.0.len(), 6, "{:?}", seen.headers);

    // A base URL with a path and a trailing slash, and no session token.
    let capture = Arc::new(Capture::default());
    let model = create_signed(
        settings(NOVA, Some("https://relay.example/bedrock/"), "", &[], None, capture.clone()),
        example_credentials(None),
        Box::new(|| "20260102T030405Z".to_owned()),
    )
    .unwrap();
    stop_at_the_mock(&model, call(hello_prompt(), None, jobj! { "type": "auto" }, None, None, None));
    let seen = capture.seen.lock().unwrap().pop().unwrap();
    assert_eq!(seen.url, "https://relay.example/bedrock/model/us.amazon.nova-pro-v1%3A0/converse-stream");
    assert_eq!(seen.body.as_deref(), Some("{\"system\":[{\"text\":\"S\"}],\"messages\":[{\"role\":\"user\",\"content\":[{\"text\":\"h\u{e9}llo\"}]}]}"));
    assert_eq!(
        seen.headers.get("authorization"),
        Some("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260102/us-east-1/bedrock/aws4_request, SignedHeaders=host;x-amz-date, Signature=fbca150b38cfab54bf43f5767c13619ac41e92b71e065f1091eb6e7f215d8467")
    );
    assert_eq!(seen.headers.get("x-amz-security-token"), None);
}

/// Recorded the same way, with `apiKey`: the key is trimmed, the region picks the endpoint, and an `Authorization`
/// the caller set survives next to the bearer token.
#[test]
fn the_api_key_path_is_what_the_real_provider_sends() {
    let capture = Arc::new(Capture::default());
    let model = create(settings("m", None, "  key  ", &[("Authorization", "Custom")], Some("us-east-1"), capture.clone())).unwrap();
    stop_at_the_mock(&model, call(hello_prompt(), None, jobj! { "type": "auto" }, None, None, None));
    let seen = capture.seen.lock().unwrap().pop().unwrap();
    assert_eq!(seen.url, "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse-stream");
    assert_eq!(seen.headers.get("authorization"), Some("Custom, Bearer key"));
    assert_eq!(seen.headers.get("user-agent"), Some(RECORDED_USER_AGENT));
    assert_eq!(seen.headers.0.len(), 3, "{:?}", seen.headers);

    let capture = Arc::new(Capture::default());
    let model = create(settings("m", None, "key", &[], Some("ap-south-2"), capture.clone())).unwrap();
    stop_at_the_mock(&model, call(hello_prompt(), None, jobj! { "type": "auto" }, None, None, None));
    assert_eq!(capture.seen.lock().unwrap()[0].url, "https://bedrock-runtime.ap-south-2.amazonaws.com/model/m/converse-stream");
}

/// The tool choices no frame can ask for (the sidecar sends `auto`, or `none` with no tools), and reasoning levels no
/// plan produces. Each expected body came out of the real `AmazonBedrockChatLanguageModel.doStream` under node.
/// `<...>` stands for the long pieces the rows share.
#[test]
fn tool_preparation_and_reasoning_are_what_the_real_model_sends() {
    const HISTORY: &str = r#"{"system":[],"messages":[{"role":"user","content":[{"text":"q"}]},{"role":"assistant","content":[{"toolUse":{"toolUseId":"c","name":"read","input":{}}}]},{"role":"user","content":[{"toolResult":{"toolUseId":"c","content":[{"text":"r"}]}}]}],"#;
    const ASK: &str = r#"{"system":[],"messages":[{"role":"user","content":[{"text":"q"}]}],"#;
    const PATHS: &str = r#""additionalModelResponseFieldPaths":["/delta/stop_sequence"],"#;
    const MAX: &str = r#""inferenceConfig":{"maxTokens":100}"#;
    const S: &str = r##"{"toolSpec":{"name":"web_search","inputSchema":{"json":{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}}}}"##;
    const F: &str = r##"{"toolSpec":{"name":"web_fetch","inputSchema":{"json":{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","properties":{"url":{"type":"string"}},"required":["url"],"additionalProperties":false}}}}"##;
    const R: &str = r#"{"toolSpec":{"name":"read","description":"d","inputSchema":{"json":{"type":"object"}}}}"#;
    const W: &str = r#"{"toolSpec":{"name":"write","inputSchema":{"json":{"type":"object","properties":{}}}}}"#;

    let provider_tool = |id: &str, name: &str| jobj! { "type": "provider", "id": id, "name": name, "args": jobj! {} };
    let read = jobj! { "type": "function", "name": "read", "description": "d", "inputSchema": jobj! { "type": "object" } };
    let tools = |key: &str| -> Option<Vec<Json>> {
        Some(match key {
            "function" => vec![read.clone(), jobj! { "type": "function", "name": "write", "description": "   ", "inputSchema": jobj! { "type": "object", "properties": jobj! {} } }],
            "provider" => vec![provider_tool("anthropic.web_search_20260209", "web_search"), provider_tool("anthropic.web_fetch_20250910", "web_fetch"), read.clone()],
            "strict" => vec![jobj! { "type": "function", "name": "strict", "inputSchema": jobj! { "type": "object" }, "strict": true }],
            "old" => vec![provider_tool("anthropic.web_search_20250305", "web_search")],
            "old_and_read" => vec![provider_tool("anthropic.web_search_20250305", "web_search"), read.clone()],
            "empty" => Vec::new(),
            _ => return None,
        })
    };
    let prompt = vec![
        jobj! { "role": "user", "content": vec![jobj! { "type": "text", "text": "q" }] },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c", "toolName": "read", "input": jobj! {} }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c", "toolName": "read", "output": jobj! { "type": "text", "value": "r" } }] },
    ];
    // Model, tools, tool choice, `disableParallelToolUse`, reasoning, ceiling, and the body.
    type Row<'a> = (&'a str, &'a str, &'a str, Option<bool>, Option<&'a str>, Option<u64>, &'a str);
    let rows: &[Row<'_>] = &[
        (SONNET, "function", "auto", None, None, Some(100), r#"<HISTORY><PATHS><MAX>,"toolConfig":{"tools":[<R>,<W>],"toolChoice":{"auto":{}}}}"#),
        (SONNET, "provider", "auto", None, None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"auto"},"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (SONNET, "function", "auto", Some(true), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"auto","disable_parallel_tool_use":true}},<PATHS><MAX>,"toolConfig":{"tools":[<R>,<W>]}}"#),
        (SONNET, "provider", "auto", Some(true), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"auto","disable_parallel_tool_use":true},"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (SONNET, "function", "auto", Some(false), None, Some(100), r#"<HISTORY><PATHS><MAX>,"toolConfig":{"tools":[<R>,<W>],"toolChoice":{"auto":{}}}}"#),
        (SONNET, "provider", "auto", Some(false), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"auto","disable_parallel_tool_use":false},"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (NOVA, "function", "auto", None, None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>,<W>],"toolChoice":{"auto":{}}}}"#),
        (NOVA, "provider", "auto", None, None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"auto":{}}}}"#),
        (NOVA, "function", "auto", Some(true), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>,<W>],"toolChoice":{"auto":{}}}}"#),
        (NOVA, "provider", "auto", Some(true), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"auto":{}}}}"#),
        (NOVA, "function", "auto", Some(false), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>,<W>],"toolChoice":{"auto":{}}}}"#),
        (NOVA, "provider", "auto", Some(false), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"auto":{}}}}"#),
        (SONNET, "function", "required", None, None, Some(100), r#"<HISTORY><PATHS><MAX>,"toolConfig":{"tools":[<R>,<W>],"toolChoice":{"any":{}}}}"#),
        (SONNET, "provider", "required", None, None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"any"},"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (SONNET, "function", "required", Some(true), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"any","disable_parallel_tool_use":true}},<PATHS><MAX>,"toolConfig":{"tools":[<R>,<W>]}}"#),
        (SONNET, "provider", "required", Some(true), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"any","disable_parallel_tool_use":true},"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (SONNET, "function", "required", Some(false), None, Some(100), r#"<HISTORY><PATHS><MAX>,"toolConfig":{"tools":[<R>,<W>],"toolChoice":{"any":{}}}}"#),
        (SONNET, "provider", "required", Some(false), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"any","disable_parallel_tool_use":false},"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (NOVA, "function", "required", None, None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>,<W>],"toolChoice":{"any":{}}}}"#),
        (NOVA, "provider", "required", None, None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"any":{}}}}"#),
        (NOVA, "function", "required", Some(true), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>,<W>],"toolChoice":{"any":{}}}}"#),
        (NOVA, "provider", "required", Some(true), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"any":{}}}}"#),
        (NOVA, "function", "required", Some(false), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>,<W>],"toolChoice":{"any":{}}}}"#),
        (NOVA, "provider", "required", Some(false), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"any":{}}}}"#),
        (SONNET, "function", "none", None, None, Some(100), r#"<ASK><PATHS><MAX>}"#),
        (SONNET, "provider", "none", None, None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (SONNET, "function", "none", Some(true), None, Some(100), r#"<ASK><PATHS><MAX>}"#),
        (SONNET, "provider", "none", Some(true), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (SONNET, "function", "none", Some(false), None, Some(100), r#"<ASK><PATHS><MAX>}"#),
        (SONNET, "provider", "none", Some(false), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (NOVA, "function", "none", None, None, Some(100), r#"<ASK><MAX>}"#),
        (NOVA, "provider", "none", None, None, Some(100), r#"<ASK><MAX>}"#),
        (NOVA, "function", "none", Some(true), None, Some(100), r#"<ASK><MAX>}"#),
        (NOVA, "provider", "none", Some(true), None, Some(100), r#"<ASK><MAX>}"#),
        (NOVA, "function", "none", Some(false), None, Some(100), r#"<ASK><MAX>}"#),
        (NOVA, "provider", "none", Some(false), None, Some(100), r#"<ASK><MAX>}"#),
        (SONNET, "function", "tool", None, None, Some(100), r#"<HISTORY><PATHS><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"tool":{"name":"read"}}}}"#),
        (SONNET, "provider", "tool", None, None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"tool","name":"read"},"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (SONNET, "function", "tool", Some(true), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"tool","name":"read","disable_parallel_tool_use":true}},<PATHS><MAX>,"toolConfig":{"tools":[<R>]}}"#),
        (SONNET, "provider", "tool", Some(true), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"tool","name":"read","disable_parallel_tool_use":true},"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (SONNET, "function", "tool", Some(false), None, Some(100), r#"<HISTORY><PATHS><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"tool":{"name":"read"}}}}"#),
        (SONNET, "provider", "tool", Some(false), None, Some(100), r#"<HISTORY>"additionalModelRequestFields":{"tool_choice":{"type":"tool","name":"read","disable_parallel_tool_use":false},"anthropic_beta":["code-execution-web-tools-2026-02-09","web-fetch-2025-09-10"]},<PATHS><MAX>,"toolConfig":{"tools":[<S>,<F>,<R>]}}"#),
        (NOVA, "function", "tool", None, None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"tool":{"name":"read"}}}}"#),
        (NOVA, "provider", "tool", None, None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"tool":{"name":"read"}}}}"#),
        (NOVA, "function", "tool", Some(true), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"tool":{"name":"read"}}}}"#),
        (NOVA, "provider", "tool", Some(true), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"tool":{"name":"read"}}}}"#),
        (NOVA, "function", "tool", Some(false), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"tool":{"name":"read"}}}}"#),
        (NOVA, "provider", "tool", Some(false), None, Some(100), r#"<HISTORY><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"tool":{"name":"read"}}}}"#),
        (SONNET, "strict", "auto", None, None, Some(100), r#"<HISTORY><PATHS><MAX>,"toolConfig":{"tools":[{"toolSpec":{"name":"strict","strict":true,"inputSchema":{"json":{"type":"object"}}}}],"toolChoice":{"auto":{}}}}"#),
        (OPUS, "strict", "auto", None, None, Some(100), r#"<HISTORY><PATHS><MAX>,"toolConfig":{"tools":[{"toolSpec":{"name":"strict","inputSchema":{"json":{"type":"object"}}}}],"toolChoice":{"auto":{}}}}"#),
        (SONNET, "old", "auto", None, None, Some(100), r#"<ASK><PATHS><MAX>}"#),
        (SONNET, "old_and_read", "auto", None, None, Some(100), r#"<HISTORY><PATHS><MAX>,"toolConfig":{"tools":[<R>],"toolChoice":{"auto":{}}}}"#),
        (SONNET, "none", "auto", None, None, Some(100), r#"<ASK><PATHS><MAX>}"#),
        (SONNET, "empty", "auto", None, None, Some(100), r#"<ASK><PATHS><MAX>}"#),
        (SONNET, "none", "auto", None, Some("none"), Some(100), r#"<ASK><PATHS><MAX>}"#),
        (NOVA, "none", "auto", None, Some("none"), Some(100), r#"<ASK><MAX>}"#),
        (OPUS, "none", "auto", None, Some("minimal"), Some(100), r#"<ASK>"additionalModelRequestFields":{"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}},<PATHS><MAX>}"#),
        (SONNET, "none", "auto", None, Some("minimal"), Some(100), r#"<ASK>"additionalModelRequestFields":{"thinking":{"type":"enabled","budget_tokens":1280}},<PATHS>"inferenceConfig":{"maxTokens":1380}}"#),
        (OPUS, "none", "auto", None, Some("xhigh"), Some(100), r#"<ASK>"additionalModelRequestFields":{"thinking":{"type":"adaptive"},"output_config":{"effort":"max"}},<PATHS><MAX>}"#),
        (OPUS, "none", "auto", None, Some("provider-default"), Some(100), r#"<ASK><PATHS><MAX>}"#),
        (SONNET, "none", "auto", None, Some("high"), None, r#"<ASK>"additionalModelRequestFields":{"thinking":{"type":"enabled","budget_tokens":38400}},<PATHS>"inferenceConfig":{"maxTokens":42496}}"#),
        (HAIKU, "none", "auto", None, Some("xhigh"), None, r#"<ASK>"additionalModelRequestFields":{"thinking":{"type":"enabled","budget_tokens":3686}},<PATHS>"inferenceConfig":{"maxTokens":7782}}"#),
    ];
    for (at, (model_id, tool_key, choice, disable, reasoning, max, expected)) in rows.iter().enumerate() {
        let capture = Arc::new(Capture::default());
        let model = create(settings(model_id, None, "k", &[], Some("us-east-1"), capture.clone())).unwrap();
        let tool_choice = if *choice == "tool" { jobj! { "type": "tool", "toolName": "read" } } else { jobj! { "type": *choice } };
        let provider_options = disable.map(|flag| jobj! { "anthropic": jobj! { "disableParallelToolUse": flag } });
        stop_at_the_mock(&model, call(prompt.clone(), tools(tool_key), tool_choice, *max, *reasoning, provider_options));
        let expected = expected
            .replace("<HISTORY>", HISTORY)
            .replace("<ASK>", ASK)
            .replace("<PATHS>", PATHS)
            .replace("<MAX>", MAX)
            .replace("<S>", S)
            .replace("<F>", F)
            .replace("<R>", R)
            .replace("<W>", W);
        let seen = capture.seen.lock().unwrap().pop().unwrap();
        assert_eq!(seen.body.as_deref(), Some(expected.as_str()), "row {at}: {model_id} {tool_key} {choice} {disable:?} {reasoning:?}");
    }
}
