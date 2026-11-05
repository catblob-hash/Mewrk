//! The `google` and `vertex` families against a mock upstream on a real socket, and Vertex's credential flow against
//! mock token and metadata endpoints. Nothing here reads the real `HOME` or leaves the machine: Vertex's standard
//! mode is built directly over a `fetch` that sends Google's fixed hosts to the mock and refuses everything else.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use common::{Mock, Reply, Rust};
use graviola::hashing::Sha256;
use graviola::signing::ecdsa::{P256, SigningKey as EcKey, VerifyingKey as EcVerifyingKey};
use graviola::signing::rsa::{KeySize, SigningKey};
use mewrk_aisdk::error::{AiError, classify};
use mewrk_aisdk::http::{BoxFuture, Cancel, DynFetch, Fetch, Headers, HttpRequest, HttpResponse, NetFetch};
use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;
use mewrk_aisdk::model::{CallOptions, DynModel, StreamPart};
use mewrk_aisdk::providers::ProviderSettings;
use mewrk_aisdk::providers::bedrock::sigv4::{Credentials, SignInput, sign};
use mewrk_aisdk::providers::vertex::{Adc, create_with_adc};

const UA_GOOGLE: &str = "ai-sdk/google/4.0.54 ai-sdk/provider-utils/5.0.32 runtime/rust";
const UA_VERTEX: &str = "ai-sdk/google-vertex/5.0.66 ai-sdk/provider-utils/5.0.32 runtime/rust";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

fn request(mock: &Mock, family: &str, model: &str, extra: Json) -> Json {
    let mut payload = jobj! {
        "family": family,
        "baseURL": format!("{}/base", mock.base),
        "apiKey": "secret-key",
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
        r#"{"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"}}],"responseId":"r1"}"#,
        r#"{"candidates":[{"content":{"parts":[{"text":" there"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"thoughtsTokenCount":7,"cachedContentTokenCount":4,"totalTokenCount":20}}"#,
    ])
}

fn read_tool() -> Json {
    jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "path": jobj! { "type": "string" } } } }
}

// ------------------------------------------------------------------ Gemini API

#[test]
fn text_streaming_and_the_request_shape() {
    let mock = Mock::script(vec![simple_stream()]);
    let outcome = Rust::start().run("s", &request(&mock, "google", "gemini-2.5-flash", jobj! { "tools": vec![read_tool()], "reasoning": "high", "maxOutputTokens": 100u64 }));
    assert_eq!(outcome.text(), "hi there");
    let result = outcome.result();
    assert_eq!(result.str_at("finishReason"), Some("stop"));
    assert_eq!(result.str_at("rawFinishReason"), Some("STOP"));
    assert_eq!(result.str_at("model"), Some("gemini-2.5-flash"));
    assert_eq!(result.get("usage").unwrap().to_text(), r#"{"inputTokens":10,"outputTokens":10,"totalTokens":20,"reasoningTokens":7,"cacheReadTokens":4}"#);
    assert_eq!(result.get("responseMessages").unwrap().to_text(), r#"[{"role":"assistant","content":[{"type":"text","text":"hi there"}]}]"#);

    let seen = mock.last_request();
    assert_eq!((seen.method.as_str(), seen.target.as_str()), ("POST", "/base/models/gemini-2.5-flash:streamGenerateContent?alt=sse"));
    assert_eq!(seen.header("x-goog-api-key"), Some("secret-key"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("user-agent"), Some(UA_GOOGLE));
    assert_eq!(
        seen.body,
        r#"{"generationConfig":{"maxOutputTokens":100,"thinkingConfig":{"thinkingBudget":24576}},"contents":[{"role":"user","parts":[{"text":"hello"}]}],"systemInstruction":{"parts":[{"text":"S"}]},"tools":[{"functionDeclarations":[{"name":"read","description":"d","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}]}],"toolConfig":{"functionCallingConfig":{"mode":"AUTO"}}}"#
    );
}

#[test]
fn thinking_config_by_generation_and_include_thoughts() {
    let thinking = |model: &str, extra: Json| {
        let mock = Mock::script(vec![simple_stream()]);
        Rust::start().run("s", &request(&mock, "google", model, extra));
        mock.last_request().json().path(&["generationConfig", "thinkingConfig"]).map(Json::to_text)
    };
    assert_eq!(thinking("gemini-3-pro-preview", jobj! { "reasoning": "high" }).as_deref(), Some(r#"{"thinkingLevel":"high"}"#));
    // The host's plan clamps Gemini 3 Pro Preview to `low` or `high` before it reaches the SDK's mapping.
    assert_eq!(thinking("gemini-3-pro-preview", jobj! { "reasoning": "medium" }).as_deref(), Some(r#"{"thinkingLevel":"low"}"#));
    assert_eq!(thinking("gemini-3.5-flash", jobj! { "reasoning": "medium" }).as_deref(), Some(r#"{"thinkingLevel":"medium"}"#));
    assert_eq!(thinking("gemini-2.5-pro", jobj! { "reasoning": "high" }).as_deref(), Some(r#"{"thinkingBudget":32768}"#));
    assert_eq!(thinking("gemini-2.5-flash", jobj! {}), None);
    // The schema's key order, whatever the caller's, and the shared level first.
    let options = jobj! { "google": jobj! { "thinkingConfig": jobj! { "includeThoughts": true, "thinkingBudget": 5u64 } } };
    assert_eq!(thinking("gemini-3-pro-preview", jobj! { "reasoning": "high", "providerOptions": options }).as_deref(), Some(r#"{"thinkingLevel":"high","thinkingBudget":5,"includeThoughts":true}"#));
}

#[test]
fn signatures_stream_into_response_messages_and_replay() {
    let mock = Mock::script(vec![
        Reply::sse(&[
            r#"{"candidates":[{"content":{"parts":[{"text":"Let me ","thought":true,"thoughtSignature":"SIG-R1"}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"think","thought":true,"thoughtSignature":"SIG-R2"}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"answer"}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"SIG-T"}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_1","name":"read","args":{"path":"a"}},"thoughtSignature":"SIG-C"},{"functionCall":{"id":"call_2","name":"read","args":{"path":"b"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}}"#,
        ]),
        simple_stream(),
    ]);
    let rust = Rust::start();
    let first = rust.run("s", &request(&mock, "google", "gemini-3-pro-preview", jobj! { "tools": vec![read_tool()], "reasoning": "high", "reasoningContent": "plaintext" }));
    let result = first.result();
    assert_eq!(result.str_at("finishReason"), Some("tool-calls"));
    assert_eq!(first.events_of("reasoning-delta").iter().filter_map(|e| e.str_at("delta")).collect::<String>(), "Let me think");
    let messages = result.get("responseMessages").unwrap();
    assert_eq!(
        messages.to_text(),
        r#"[{"role":"assistant","content":[{"type":"reasoning","text":"Let me think","providerOptions":{"google":{"thoughtSignature":"SIG-R2"}}},{"type":"text","text":"answer","providerOptions":{"google":{"thoughtSignature":"SIG-T"}}},{"type":"tool-call","toolCallId":"call_1","toolName":"read","input":{"path":"a"},"providerOptions":{"google":{"thoughtSignature":"SIG-C"}}},{"type":"tool-call","toolCallId":"call_2","toolName":"read","input":{"path":"b"}}]}]"#
    );

    let mut history = vec![jobj! { "role": "user", "content": "hello" }];
    history.extend(messages.as_arr().unwrap().iter().cloned());
    history.push(jobj! { "role": "tool", "content": vec![
        jobj! { "type": "tool-result", "toolCallId": "call_1", "toolName": "read", "output": jobj! { "type": "text", "value": "A" } },
        jobj! { "type": "tool-result", "toolCallId": "call_2", "toolName": "read", "output": jobj! { "type": "text", "value": "B" } },
    ] });
    rust.run("t", &request(&mock, "google", "gemini-3-pro-preview", jobj! { "tools": vec![read_tool()], "messages": history }));
    let body = mock.last_request().json();
    assert_eq!(
        body.get("contents").unwrap().to_text(),
        r#"[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"text":"Let me think","thought":true,"thoughtSignature":"SIG-R2"},{"text":"answer","thoughtSignature":"SIG-T"},{"functionCall":{"id":"call_1","name":"read","args":{"path":"a"}},"thoughtSignature":"SIG-C"},{"functionCall":{"id":"call_2","name":"read","args":{"path":"b"}}}]},{"role":"user","parts":[{"functionResponse":{"id":"call_1","name":"read","response":{"name":"read","content":"A"}}},{"functionResponse":{"id":"call_2","name":"read","response":{"name":"read","content":"B"}}}]}]"#
    );
}

#[test]
fn a_replayed_call_with_no_signature_gets_the_sentinel_on_gemini_3_only() {
    let history = || jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c", "toolName": "read", "input": jobj! {} }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c", "toolName": "read", "output": jobj! { "type": "text", "value": "x" } }] },
    ], "tools": vec![read_tool()] };
    let call = |model: &str| {
        let mock = Mock::script(vec![simple_stream()]);
        Rust::start().run("s", &request(&mock, "google", model, history()));
        mock.last_request().json().get("contents").unwrap().as_arr().unwrap()[1].to_text()
    };
    assert_eq!(call("gemini-3-pro-preview"), r#"{"role":"model","parts":[{"functionCall":{"id":"c","name":"read","args":{}},"thoughtSignature":"skip_thought_signature_validator"}]}"#);
    assert_eq!(call("gemini-2.5-flash"), r#"{"role":"model","parts":[{"functionCall":{"id":"c","name":"read","args":{}}}]}"#);
}

#[test]
fn tool_results_with_images_by_generation() {
    let history = || jobj! { "messages": vec![
        jobj! { "role": "user", "content": vec![jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png" }] },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c", "toolName": "read", "input": jobj! {} }] },
        jobj! { "role": "tool", "content": vec![jobj! {
            "type": "tool-result", "toolCallId": "c", "toolName": "read",
            "output": jobj! { "type": "content", "value": vec![jobj! { "type": "text", "text": "body" }, jobj! { "type": "file-data", "data": "iVBORw0KGgo=", "mediaType": "image/png" }] },
        }] },
    ] };
    let contents = |model: &str| {
        let mock = Mock::script(vec![simple_stream()]);
        Rust::start().run("s", &request(&mock, "google", model, history()));
        let body = mock.last_request().json();
        let list = body.get("contents").unwrap().as_arr().unwrap().clone();
        (list[0].to_text(), list[2].to_text())
    };
    let (first, tool) = contents("gemini-3-pro-preview");
    assert_eq!(first, r#"{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgo="}}]}"#);
    assert_eq!(tool, r#"{"role":"user","parts":[{"functionResponse":{"id":"c","name":"read","response":{"name":"read","content":"body"},"parts":[{"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgo="}}]}}]}"#);
    let (_, legacy) = contents("gemini-2.5-flash");
    assert_eq!(legacy, r#"{"role":"user","parts":[{"functionResponse":{"id":"c","name":"read","response":{"name":"read","content":"body"}}},{"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgo="}},{"text":"Tool executed successfully and returned this image as a response"}]}"#);
}

#[test]
fn google_search_declares_the_tool_and_grounding_becomes_sources() {
    let grounded = || {
        Reply::sse(&[
            r#"{"candidates":[{"content":{"parts":[{"text":"found"}]},"groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://a.example/x","title":"A"}},{"web":{"uri":"https://a.example/x"}},{"retrievedContext":{"uri":"gs://bucket/doc.pdf","title":"D"}}]}}]}"#,
            r#"{"candidates":[{"finishReason":"STOP","groundingMetadata":{"groundingChunks":[{"image":{"sourceUri":"https://b.example/","imageUri":"https://b.example/i.png"}}]}}]}"#,
        ])
    };
    let search = jobj! { "nativeSearch": jobj! { "maxUses": 2u64 } };
    let mock = Mock::script(vec![grounded()]);
    let outcome = Rust::start().run("s", &request(&mock, "google", "gemini-2.5-flash", search));
    let body = mock.last_request().json();
    assert_eq!(body.get("tools").unwrap().to_text(), r#"[{"googleSearch":{}}]"#);
    assert!(body.get("toolConfig").is_none());
    let sources = outcome.result().get("sources").unwrap().as_arr().unwrap().clone();
    let urls: Vec<_> = sources.iter().map(|s| s.str_at("url").unwrap().to_owned()).collect();
    assert_eq!(urls, ["https://a.example/x", "https://b.example/"]);
    assert_eq!(sources[0].str_at("title"), Some("A"));

    // Gemini 3 combines it with function tools, and only the Gemini API takes `includeServerSideToolInvocations`.
    let both = jobj! { "nativeSearch": jobj! { "maxUses": 2u64 }, "tools": vec![read_tool()] };
    let mock = Mock::script(vec![grounded()]);
    Rust::start().run("s", &request(&mock, "google", "gemini-3-pro-preview", both.clone()));
    let body = mock.last_request().json();
    assert_eq!(body.get("tools").unwrap().as_arr().unwrap()[0].to_text(), r#"{"googleSearch":{}}"#);
    assert_eq!(body.get("toolConfig").unwrap().to_text(), r#"{"functionCallingConfig":{"mode":"VALIDATED"},"includeServerSideToolInvocations":true}"#);
    let mock = Mock::script(vec![grounded()]);
    Rust::start().run("s", &request(&mock, "vertex", "gemini-3-pro-preview", both));
    assert_eq!(mock.last_request().json().get("toolConfig").unwrap().to_text(), r#"{"functionCallingConfig":{"mode":"VALIDATED"}}"#);
}

#[test]
fn finish_reasons_map_and_tool_calls_decide_stop() {
    let finish = |chunk: &str| {
        let mock = Mock::script(vec![Reply::sse(&[chunk])]);
        let outcome = Rust::start().run("s", &request(&mock, "google", "gemini-2.5-flash", jobj! { "tools": vec![read_tool()] }));
        let result = outcome.result().clone();
        (result.str_at("finishReason").unwrap().to_owned(), result.str_at("rawFinishReason").map(str::to_owned))
    };
    assert_eq!(finish(r#"{"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"MAX_TOKENS"}]}"#), ("length".to_owned(), Some("MAX_TOKENS".to_owned())));
    assert_eq!(finish(r#"{"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"SAFETY"}]}"#).0, "content-filter");
    assert_eq!(finish(r#"{"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"MALFORMED_FUNCTION_CALL"}]}"#).0, "error");
    assert_eq!(finish(r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"c","name":"read","args":{}}}]},"finishReason":"STOP"}]}"#).0, "tool-calls");
    // An error chunk is nothing to the schema, so the stream just ends.
    assert_eq!(finish(r#"{"candidates":[{"content":{"parts":[{"text":"x"}]}}],"error":{"code":503,"message":"m","status":"UNAVAILABLE"}}"#), ("other".to_owned(), None));
}

#[test]
fn error_classification_and_retry_after() {
    let failure = |reply: Reply| {
        let mock = Mock::script(vec![reply]);
        let outcome = Rust::start().run("s", &request(&mock, "google", "gemini-2.5-flash", jobj! {}));
        assert_eq!(mock.requests().len(), 1, "the sidecar does not retry");
        outcome.error().clone()
    };
    let limited = failure(Reply::json(429, r#"{"error":{"code":429,"message":"slow down","status":"RESOURCE_EXHAUSTED"}}"#).header("retry-after", "7"));
    assert_eq!(limited.to_text(), r#"{"kind":"transient","message":"slow down","status":429,"retryAfterMs":7000}"#);
    let bad = failure(Reply::json(400, r#"{"error":{"code":400,"message":"nope","status":"INVALID_ARGUMENT"}}"#));
    assert_eq!(bad.to_text(), r#"{"kind":"permanent","message":"nope","status":400}"#);
    // Not the error schema: the message falls back to the status line.
    let odd = failure(Reply::json(500, r#"{"error":{"message":"no status field"}}"#));
    assert_eq!(odd.to_text(), r#"{"kind":"transient","message":"Internal Server Error","status":500}"#);
    let invalid = failure(Reply::sse(&[r#"{"candidates":[{"content":{"parts":[{"text":5}]}}]}"#]));
    assert_eq!(invalid.str_at("kind"), Some("permanent"));
    assert!(invalid.str_at("message").unwrap().starts_with("Type validation failed"), "{invalid}");
}

#[test]
fn the_files_endpoint_of_the_base_url_is_taken_as_it_is() {
    let mock = Mock::script(vec![simple_stream()]);
    let url = format!("{}/base/files/abc", mock.base);
    let history = vec![jobj! { "role": "user", "content": vec![jobj! { "type": "file", "data": url.as_str(), "mediaType": "application/pdf" }] }];
    Rust::start().run("s", &request(&mock, "google", "gemini-2.0-flash", jobj! { "messages": history }));
    assert_eq!(mock.requests().len(), 1, "nothing was downloaded");
    assert_eq!(mock.last_request().json().path(&["contents"]).unwrap().to_text(), format!(r#"[{{"role":"user","parts":[{{"fileData":{{"mimeType":"application/pdf","fileUri":"{url}"}}}}]}}]"#));
}

#[test]
fn a_system_message_after_the_conversation_starts_is_refused() {
    let mock = Mock::script(vec![simple_stream()]);
    let history = vec![jobj! { "role": "user", "content": "q" }, jobj! { "role": "system", "content": "late" }];
    let outcome = Rust::start().run("s", &request(&mock, "google", "gemini-2.5-flash", jobj! { "messages": history }));
    assert_eq!(outcome.error().str_at("message"), Some("'system messages are only supported at the beginning of the conversation' functionality not supported."));
    assert!(mock.requests().is_empty());
}

#[test]
fn gemma_takes_the_system_prompt_in_the_first_message() {
    let mock = Mock::script(vec![simple_stream()]);
    Rust::start().run("s", &request(&mock, "google", "gemma-3-27b-it", jobj! {}));
    let body = mock.last_request().json();
    assert!(body.get("systemInstruction").is_none());
    assert_eq!(body.get("contents").unwrap().to_text(), r#"[{"role":"user","parts":[{"text":"S\n\n"},{"text":"hello"}]}]"#);
}

// ------------------------------------------------------------------ Vertex Express Mode

#[test]
fn express_mode_keys_every_request_and_writes_both_metadata_names() {
    let mock = Mock::script(vec![
        Reply::sse(&[
            r#"{"candidates":[{"content":{"parts":[{"text":"t","thought":true,"thoughtSignature":"SIG-R"},{"functionCall":{"id":"dropped-on-replay","name":"read","args":{}},"thoughtSignature":"SIG-C"}]},"finishReason":"STOP"}]}"#,
        ]),
        simple_stream(),
    ]);
    let rust = Rust::start();
    let headers = jobj! { "headers": jobj! { "X-Goog-Api-Key": "loses", "x-extra": "1" }, "tools": vec![read_tool()] };
    let outcome = rust.run("s", &request(&mock, "vertex", "gemini-3-pro-preview", headers));
    let seen = mock.last_request();
    assert_eq!(seen.target, "/base/models/gemini-3-pro-preview:streamGenerateContent?alt=sse");
    assert_eq!(seen.header("x-goog-api-key"), Some("secret-key"));
    assert_eq!(seen.header("x-extra"), Some("1"));
    assert_eq!(seen.header("authorization"), None);
    assert_eq!(seen.header("user-agent"), Some(UA_VERTEX));
    let messages = outcome.result().get("responseMessages").unwrap().clone();
    assert_eq!(
        messages.to_text(),
        r#"[{"role":"assistant","content":[{"type":"reasoning","text":"t","providerOptions":{"googleVertex":{"thoughtSignature":"SIG-R"},"vertex":{"thoughtSignature":"SIG-R"}}},{"type":"tool-call","toolCallId":"dropped-on-replay","toolName":"read","input":{},"providerOptions":{"googleVertex":{"thoughtSignature":"SIG-C"},"vertex":{"thoughtSignature":"SIG-C"}}}]}]"#
    );

    let mut history = vec![jobj! { "role": "user", "content": "q" }];
    history.extend(messages.as_arr().unwrap().iter().cloned());
    history.push(jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "dropped-on-replay", "toolName": "read", "output": jobj! { "type": "text", "value": "x" } }] });
    rust.run("t", &request(&mock, "vertex", "gemini-3-pro-preview", jobj! { "messages": history, "tools": vec![read_tool()] }));
    // Vertex takes no call ids on `functionCall` or `functionResponse`.
    assert_eq!(
        mock.last_request().json().get("contents").unwrap().as_arr().unwrap()[1..].iter().map(Json::to_text).collect::<Vec<_>>().join(","),
        r#"{"role":"model","parts":[{"text":"t","thought":true,"thoughtSignature":"SIG-R"},{"functionCall":{"name":"read","args":{}},"thoughtSignature":"SIG-C"}]},{"role":"user","parts":[{"functionResponse":{"name":"read","response":{"name":"read","content":"x"}}}]}"#
    );
}

#[test]
fn vertex_request_types_are_headers_and_service_tier_goes() {
    let mock = Mock::script(vec![simple_stream()]);
    let options = jobj! { "providerOptions": jobj! { "googleVertex": jobj! { "sharedRequestType": "flex", "requestType": "shared", "serviceTier": "flex", "labels": jobj! { "a": "b" } } } };
    Rust::start().run("s", &request(&mock, "vertex", "gemini-2.5-flash", options));
    let seen = mock.last_request();
    assert_eq!(seen.header("x-vertex-ai-llm-shared-request-type"), Some("flex"));
    assert_eq!(seen.header("x-vertex-ai-llm-request-type"), Some("shared"));
    let body = seen.json();
    assert!(body.get("serviceTier").is_none());
    assert_eq!(body.get("labels").unwrap().to_text(), r#"{"a":"b"}"#);
}

#[test]
fn streamed_function_arguments_assemble_on_vertex() {
    let mock = Mock::script(vec![Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"c1","name":"read","willContinue":true},"thoughtSignature":"SIG"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.path","stringValue":"a","willContinue":true}],"willContinue":true}}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.path","stringValue":"b"}],"willContinue":true}}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{}}]},"finishReason":"STOP"}]}"#,
    ])]);
    let options = jobj! { "tools": vec![read_tool()], "providerOptions": jobj! { "googleVertex": jobj! { "streamFunctionCallArguments": true } } };
    let outcome = Rust::start().run("s", &request(&mock, "vertex", "gemini-3-pro-preview", options));
    assert_eq!(mock.last_request().json().get("toolConfig").unwrap().to_text(), r#"{"functionCallingConfig":{"mode":"AUTO","streamFunctionCallArguments":true}}"#);
    assert_eq!(
        outcome.result().get("responseMessages").unwrap().to_text(),
        r#"[{"role":"assistant","content":[{"type":"tool-call","toolCallId":"c1","toolName":"read","input":{"path":"ab"},"providerOptions":{"googleVertex":{"thoughtSignature":"SIG"},"vertex":{"thoughtSignature":"SIG"}}}]}]"#
    );
    assert_eq!(outcome.result().str_at("finishReason"), Some("tool-calls"));
}

// ------------------------------------------------------------------ Vertex standard mode

/// Sends Google's fixed hosts to the mock and refuses anything else, so no test can reach a real endpoint.
struct Redirect {
    mock: String,
    /// `false`: the metadata hosts do not answer, as off Google Cloud.
    metadata: bool,
    inner: DynFetch,
}

impl Fetch for Redirect {
    fn fetch(&self, mut request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        let fixed = [
            ("https://oauth2.googleapis.com", true),
            ("https://iamcredentials.googleapis.com", true),
            ("https://sts.googleapis.com", true),
            ("http://169.254.169.254", self.metadata),
            ("http://metadata.google.internal.", self.metadata),
        ];
        for (real, to_mock) in fixed {
            if request.url.starts_with(real) {
                if !to_mock {
                    return Box::pin(async { Err(AiError::Network { code: "ENOTFOUND", message: "no metadata server here".to_owned() }) });
                }
                request.url = request.url.replacen(real, &self.mock, 1);
                return self.inner.fetch(request);
            }
        }
        if !request.url.starts_with(&self.mock) {
            let url = request.url.clone();
            return Box::pin(async move { Err(AiError::other(format!("refused to contact {url}"))) });
        }
        self.inner.fetch(request)
    }
}

/// A new directory holding `.config/gcloud/application_default_credentials.json`, when given its contents.
struct Home(std::path::PathBuf);

impl Home {
    fn new(credentials: Option<&str>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("mewrk-aisdk-adc-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let gcloud = dir.join(".config/gcloud");
        std::fs::create_dir_all(&gcloud).unwrap();
        if let Some(credentials) = credentials {
            std::fs::write(gcloud.join("application_default_credentials.json"), credentials).unwrap();
        }
        Self(dir)
    }

    fn adc(&self) -> Adc {
        Adc::at_home(Some(self.0.to_string_lossy().into_owned()))
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn der_to_pem(label: &str, der: &[u8]) -> String {
    let body = base64_simd::STANDARD.encode_to_string(der);
    let lines: Vec<&str> = body.as_bytes().chunks(64).map(|line| std::str::from_utf8(line).unwrap()).collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

/// One throwaway key per run: generating RSA in a debug build is slow.
fn throwaway_key() -> &'static SigningKey {
    static KEY: OnceLock<SigningKey> = OnceLock::new();
    KEY.get_or_init(|| SigningKey::generate(KeySize::Rsa2048).expect("generate"))
}

fn pkcs8_pem() -> String {
    let mut buffer = vec![0u8; 8192];
    der_to_pem("PRIVATE KEY", throwaway_key().to_pkcs8_der(&mut buffer).unwrap())
}

fn pkcs1_pem() -> String {
    let mut buffer = vec![0u8; 8192];
    der_to_pem("RSA PRIVATE KEY", throwaway_key().to_pkcs1_der(&mut buffer).unwrap())
}

fn service_account(pem: &str) -> String {
    jobj! {
        "type": "service_account",
        "project_id": "p",
        "private_key_id": "not-sent",
        "private_key": pem,
        "client_email": "sa@p.iam.gserviceaccount.com",
        "token_uri": "https://attacker.example/token",
    }
    .to_text()
}

fn authorized_user() -> String {
    jobj! { "type": "authorized_user", "client_id": "cid.apps", "client_secret": "sec/ret+", "refresh_token": "1//ref tok", "quota_project_id": "not-used" }.to_text()
}

fn token_reply(body: &str) -> Reply {
    Reply::json(200, body)
}

/// Routes by path: the model's stream, or `token` for `/token` and `metadata` for the metadata paths.
fn upstream(token: Reply, metadata: Option<Reply>) -> Mock {
    let metadata = metadata.unwrap_or_else(|| Reply::new(404));
    Mock::start(Arc::new(move |seen, _| {
        if seen.target == "/token" {
            token.clone()
        } else if seen.target.starts_with("/computeMetadata/") {
            metadata.clone()
        } else {
            simple_stream()
        }
    }))
}

fn target(mock: &Mock, fetch_metadata: bool, headers: Headers) -> ProviderSettings {
    let settings: HashMap<String, String> = [("project", "p"), ("location", "us-central1")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    ProviderSettings {
        base_url: Some(format!("{}/base", mock.base)),
        api_key: String::new(),
        headers,
        settings,
        model_id: "gemini-2.5-flash".to_owned(),
        fetch: Arc::new(Redirect { mock: mock.base.clone(), metadata: fetch_metadata, inner: NetFetch::shared() }),
    }
}

fn call_options(cancel: Cancel) -> CallOptions {
    CallOptions {
        prompt: vec![jobj! { "role": "user", "content": vec![jobj! { "type": "text", "text": "hello" }] }],
        tools: None,
        tool_choice: jobj! { "type": "auto" },
        max_output_tokens: None,
        reasoning: None,
        provider_options: None,
        cancel,
    }
}

/// One `doStream`, read to its end: the text or the error.
async fn stream_text(model: &DynModel) -> Result<String, AiError> {
    let mut stream = model.do_stream(call_options(Cancel::new())).await?;
    let mut text = String::new();
    while let Some(part) = stream.next().await {
        match part {
            StreamPart::TextDelta { delta, .. } => text.push_str(&delta),
            StreamPart::Error(error) => return Err(error),
            _ => {}
        }
    }
    Ok(text)
}

fn token_requests(mock: &Mock) -> Vec<common::Seen> {
    mock.requests().into_iter().filter(|r| r.target == "/token").collect()
}

fn model_requests(mock: &Mock) -> Vec<common::Seen> {
    mock.requests().into_iter().filter(|r| r.target.starts_with("/base/")).collect()
}

fn base64url(text: &str) -> Vec<u8> {
    base64_simd::URL_SAFE_NO_PAD.decode_to_vec(text.as_bytes()).expect("base64url")
}

fn check_assertion(assertion: &str) {
    let parts: Vec<&str> = assertion.split('.').collect();
    assert_eq!(parts.len(), 3, "{assertion}");
    // `{"alg":"RS256"}` byte for byte, with no `kid` from `private_key_id` and no `typ`, as `jws` writes it upstream.
    assert_eq!(parts[0], "eyJhbGciOiJSUzI1NiJ9");
    let payload = Json::parse(std::str::from_utf8(&base64url(parts[1])).unwrap()).unwrap();
    let keys: Vec<&str> = payload.as_obj().unwrap().keys().collect();
    assert_eq!(keys, ["iss", "scope", "aud", "exp", "iat"]);
    assert_eq!(payload.str_at("iss"), Some("sa@p.iam.gserviceaccount.com"));
    assert_eq!(payload.str_at("scope"), Some(SCOPE));
    // Fixed upstream too: a `token_uri` in the file is not read.
    assert_eq!(payload.str_at("aud"), Some("https://oauth2.googleapis.com/token"));
    let iat = payload.get("iat").unwrap().as_u64().unwrap();
    assert_eq!(payload.get("exp").unwrap().as_u64().unwrap(), iat + 3600);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(iat <= now && now - iat < 300, "iat {iat}, now {now}");
    let message = format!("{}.{}", parts[0], parts[1]);
    throwaway_key().public_key().verify_pkcs1_sha256(&base64url(parts[2]), message.as_bytes()).expect("the signature verifies");
    assert!(throwaway_key().public_key().verify_pkcs1_sha256(&base64url(parts[2]), b"something else").is_err());
}

#[tokio::test]
async fn a_service_account_signs_a_jwt_and_exchanges_it() {
    let home = Home::new(Some(&service_account(&pkcs8_pem())));
    let mock = upstream(token_reply(r#"{"access_token":"tok-1","expires_in":3600,"token_type":"Bearer"}"#), None);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");

    let tokens = token_requests(&mock);
    assert_eq!(tokens.len(), 1);
    let exchange = &tokens[0];
    assert_eq!(exchange.method, "POST");
    assert_eq!(exchange.header("content-type"), Some("application/x-www-form-urlencoded;charset=UTF-8"));
    assert_eq!(exchange.header("accept"), Some("application/json"));
    assert_eq!(exchange.header("user-agent"), Some("google-api-nodejs-client/10.9.1"));
    assert_eq!(exchange.header("x-goog-api-client"), Some("gl-rust"));
    let assertion = exchange.body.strip_prefix("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion=").expect(&exchange.body);
    check_assertion(assertion);

    let calls = model_requests(&mock);
    assert_eq!(calls[0].target, "/base/models/gemini-2.5-flash:streamGenerateContent?alt=sse");
    assert_eq!(calls[0].header("authorization"), Some("Bearer tok-1"));
    assert_eq!(calls[0].header("x-goog-api-key"), None);
    assert_eq!(calls[0].header("x-goog-user-project"), None);
    assert_eq!(calls[0].header("user-agent"), Some(UA_VERTEX));

    // A live token is reused.
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");
    assert_eq!(token_requests(&mock).len(), 1);
    assert_eq!(model_requests(&mock).len(), 2);
}

#[tokio::test]
async fn a_pkcs1_key_signs_too() {
    let home = Home::new(Some(&service_account(&pkcs1_pem())));
    let mock = upstream(token_reply(r#"{"access_token":"tok","expires_in":3600}"#), None);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    let exchange = token_requests(&mock).remove(0);
    check_assertion(exchange.body.rsplit('=').next().unwrap());
}

#[tokio::test]
async fn a_key_it_cannot_read_fails_without_a_request() {
    let home = Home::new(Some(&service_account("not a pem")));
    let mock = upstream(token_reply("{}"), None);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    let error = stream_text(&model).await.unwrap_err();
    assert_eq!(classify(&error).kind, "permanent");
    assert_eq!(error.message(), "error:1E08010C:DECODER routines::unsupported");
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn an_authorized_user_refreshes_its_token() {
    let home = Home::new(Some(&authorized_user()));
    let mock = upstream(token_reply(r#"{"access_token":"tok-u","expires_in":3599,"scope":"x","token_type":"Bearer"}"#), None);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");
    let tokens = token_requests(&mock);
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].body, "refresh_token=1%2F%2Fref+tok&client_id=cid.apps&client_secret=sec%2Fret%2B&grant_type=refresh_token");
    assert_eq!(tokens[0].header("accept"), Some("*/*"));
    assert_eq!(model_requests(&mock)[1].header("authorization"), Some("Bearer tok-u"));
}

#[tokio::test]
async fn token_lifetimes_follow_each_client() {
    // Under the five-minute eager threshold: fetched again every call.
    let home = Home::new(Some(&authorized_user()));
    let mock = upstream(token_reply(r#"{"access_token":"t","expires_in":60}"#), None);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    stream_text(&model).await.unwrap();
    assert_eq!(token_requests(&mock).len(), 2);

    // A user token with no `expires_in` has no expiry at all.
    let mock = upstream(token_reply(r#"{"access_token":"t"}"#), None);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    stream_text(&model).await.unwrap();
    assert_eq!(token_requests(&mock).len(), 1);

    // A service account's is expired at once.
    let home = Home::new(Some(&service_account(&pkcs8_pem())));
    let mock = upstream(token_reply(r#"{"access_token":"t"}"#), None);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    stream_text(&model).await.unwrap();
    assert_eq!(token_requests(&mock).len(), 2);
}

#[tokio::test]
async fn the_callers_authorization_wins_but_the_token_is_still_fetched() {
    let home = Home::new(Some(&authorized_user()));
    let mock = upstream(token_reply(r#"{"access_token":"tok","expires_in":3600}"#), None);
    let mut headers = Headers::new();
    headers.set("Authorization", "Bearer mine");
    let model = create_with_adc(target(&mock, false, headers), home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    assert_eq!(token_requests(&mock).len(), 1);
    assert_eq!(model_requests(&mock)[0].header("authorization"), Some("Bearer mine"));
}

#[tokio::test]
async fn token_failures_read_as_upstream_words_them() {
    let failing = |credentials: String, reply: Reply| async move {
        let home = Home::new(Some(&credentials));
        let mock = upstream(reply, None);
        let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
        let error = stream_text(&model).await.unwrap_err();
        assert_eq!(token_requests(&mock).len(), 1, "the sidecar does not retry");
        assert!(model_requests(&mock).is_empty());
        classify(&error)
    };
    let invalid = r#"{"error":"invalid_grant","error_description":"Invalid JWT Signature."}"#;
    let step = failing(service_account(&pkcs8_pem()), Reply::json(400, invalid)).await;
    assert_eq!((step.kind, step.message.as_str(), step.status), ("permanent", "invalid_grant: Invalid JWT Signature.", None));
    let step = failing(authorized_user(), Reply::json(400, invalid)).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "invalid_grant"));
    let reauth = r#"{"error":"invalid_grant","error_description":"reauth related error (invalid_rapt)","error_subtype":"invalid_rapt"}"#;
    let step = failing(authorized_user(), Reply::json(400, reauth)).await;
    assert_eq!(step.message, reauth);
    let step = failing(service_account(&pkcs8_pem()), Reply::new(401).header("content-type", "text/plain").body("nope")).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "nope"));
    let step = failing(authorized_user(), Reply::json(403, r#"{"error":{"code":403,"message":"denied","status":"PERMISSION_DENIED"}}"#)).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "denied"));
    let step = failing(authorized_user(), token_reply(r#"{"expires_in":3600}"#)).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "Could not refresh access token."));
    let step = failing(service_account(&pkcs8_pem()), token_reply(r#"{"expires_in":3600}"#)).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "Could not refresh access token."));
    // What gaxios would have retried is the host's to retry.
    let step = failing(service_account(&pkcs8_pem()), Reply::json(503, r#"{"error":"backend_error"}"#).header("retry-after", "2")).await;
    assert_eq!((step.kind, step.status, step.retry_after_ms), ("transient", Some(503), Some(2000)));
    let step = failing(authorized_user(), Reply::new(429).header("content-type", "text/plain").body("slow")).await;
    assert_eq!((step.kind, step.message.as_str(), step.status), ("transient", "slow", Some(429)));
}

#[tokio::test]
async fn a_token_endpoint_that_cannot_be_reached_is_transient() {
    let home = Home::new(Some(&authorized_user()));
    // A port nothing listens on: bound, read, and released.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port())
    };
    let mock = Mock::script(vec![simple_stream()]);
    let mut settings = target(&mock, false, Headers::new());
    settings.fetch = Arc::new(Redirect { mock: closed, metadata: false, inner: NetFetch::shared() });
    let model = create_with_adc(settings, home.adc()).unwrap();
    assert_eq!(classify(&stream_text(&model).await.unwrap_err()).kind, "transient");
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn a_cancelled_step_stops_waiting_for_its_token() {
    let home = Home::new(Some(&authorized_user()));
    let mock = upstream(Reply::new(200).header("content-type", "application/json").body("{").hanging(), None);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    let cancel = Cancel::new();
    let call = model.do_stream(call_options(cancel.clone()));
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        canceller.cancel();
    });
    let outcome = tokio::time::timeout(Duration::from_secs(10), call).await.expect("the cancel is honoured");
    assert!(matches!(outcome, Err(AiError::Abort)));
}

#[tokio::test]
async fn the_metadata_server_is_the_last_resort() {
    let flavored = Reply::json(200, r#"{"access_token":"tok-m","expires_in":3599,"token_type":"Bearer"}"#).header("metadata-flavor", "Google");
    let mock = upstream(token_reply("{}"), Some(flavored));
    let home = Home::new(None);
    let adc = home.adc();
    let model = create_with_adc(target(&mock, true, Headers::new()), adc.clone()).unwrap();
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");
    let requests = mock.requests();
    let probes: Vec<_> = requests.iter().filter(|r| r.target == "/computeMetadata/v1/instance").collect();
    assert!(!probes.is_empty() && probes.len() <= 2, "{probes:?}");
    assert!(probes.iter().all(|p| p.method == "GET" && p.header("metadata-flavor") == Some("Google")));
    let fetched: Vec<_> = requests.iter().filter(|r| r.target.contains("/service-accounts/")).collect();
    assert_eq!(fetched.len(), 1, "the token is reused");
    assert_eq!(fetched[0].target, "/computeMetadata/v1/instance/service-accounts/default/token?scopes=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform");
    assert_eq!(fetched[0].header("metadata-flavor"), Some("Google"));
    assert_eq!(model_requests(&mock)[0].header("authorization"), Some("Bearer tok-m"));
    assert!(token_requests(&mock).is_empty());

    // The answer to the probe outlives the model, as `isAvailable`'s does the `GoogleAuth`.
    let before = probes.len();
    let again = create_with_adc(target(&mock, true, Headers::new()), adc).unwrap();
    stream_text(&again).await.unwrap();
    let probes_after = mock.requests().iter().filter(|r| r.target == "/computeMetadata/v1/instance").count();
    assert_eq!(probes_after, before);
}

#[tokio::test]
async fn no_credentials_anywhere_is_refused_without_a_model_request() {
    const NO_ADC: &str = "Could not load the default credentials. Browse to https://cloud.google.com/docs/authentication/getting-started for more information.";
    for home in [Some(Home::new(None)), None] {
        let mock = upstream(token_reply("{}"), None);
        let adc = home.as_ref().map_or_else(|| Adc::at_home(None), Home::adc);
        let model = create_with_adc(target(&mock, false, Headers::new()), adc).unwrap();
        let step = classify(&stream_text(&model).await.unwrap_err());
        assert_eq!((step.kind, step.message.as_str()), ("permanent", NO_ADC));
        assert!(mock.requests().is_empty());
    }
    // Something answers, but it is not a metadata server.
    let mock = upstream(token_reply("{}"), Some(Reply::json(200, "{}")));
    let home = Home::new(None);
    let model = create_with_adc(target(&mock, true, Headers::new()), home.adc()).unwrap();
    assert_eq!(stream_text(&model).await.unwrap_err().message(), NO_ADC);
}

#[tokio::test]
async fn metadata_token_failures_read_as_upstream_words_them() {
    let failing = |reply: Reply| async move {
        // The probe needs a flavoured answer; the token path below it is what fails.
        let mock = Mock::start(Arc::new(move |seen, _| {
            if seen.target == "/computeMetadata/v1/instance" { Reply::new(200).header("metadata-flavor", "Google").body("{}") } else { reply.clone() }
        }));
        let home = Home::new(None);
        let model = create_with_adc(target(&mock, true, Headers::new()), home.adc()).unwrap();
        classify(&stream_text(&model).await.unwrap_err())
    };
    let step = failing(Reply::new(403).header("metadata-flavor", "Google").body("denied")).await;
    assert_eq!(step.message, "A Forbidden error was returned while attempting to retrieve an access token for the Compute Engine built-in service account. This may be because the Compute Engine instance does not have the correct permission scopes specified: Could not refresh access token: denied");
    assert_eq!(step.kind, "permanent");
    let step = failing(Reply::new(404).header("metadata-flavor", "Google").body("gone")).await;
    assert_eq!(step.message, "A Not Found error was returned while attempting to retrieve an accesstoken for the Compute Engine built-in service account. This may be because the Compute Engine instance does not have any permission scopes specified: Could not refresh access token: gone");
    let step = failing(Reply::json(200, r#"{"access_token":"x"}"#)).await;
    assert_eq!(step.message, "Invalid response from metadata service: incorrect Metadata-Flavor header. Expected 'Google', got no header");
    let step = failing(Reply::new(500).header("metadata-flavor", "Google")).await;
    assert_eq!((step.kind, step.message.as_str(), step.status), ("transient", "Could not refresh access token: ", Some(500)));
}

#[tokio::test]
async fn credential_files_that_cannot_be_used() {
    let failing = |credentials: &str| {
        let home = Home::new(Some(credentials));
        let mock = upstream(token_reply("{}"), None);
        let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
        async move {
            let error = stream_text(&model).await.unwrap_err();
            assert!(mock.requests().is_empty());
            drop(home);
            classify(&error)
        }
    };
    assert_eq!(failing(r#"{"type":"service_account","private_key":"k"}"#).await.message, "The incoming JSON object does not contain a client_email field");
    assert_eq!(failing(r#"{"type":"authorized_user","client_id":"c","client_secret":"s"}"#).await.message, "The incoming JSON object does not contain a refresh_token field");
    assert_eq!(failing("{nope").await.kind, "permanent");
    assert_eq!(failing(r#"{"type":"external_account","audience":"a"}"#).await.message, "A credential source or subject token supplier must be specified.");

    let home = Home::new(None);
    let location = home.0.join(".config/gcloud/application_default_credentials.json");
    std::fs::create_dir_all(&location).unwrap();
    let mock = upstream(token_reply("{}"), None);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    let message = stream_text(&model).await.unwrap_err().message();
    assert!(message.starts_with("The file at ") && message.ends_with("does not exist, or it is not a file. "), "{message}");
}

/// Answers everything and writes down the URLs, for the endpoints that are not configurable. No socket at all.
struct Recorder(std::sync::Mutex<Vec<String>>);

impl Fetch for Recorder {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        self.0.lock().unwrap().push(request.url.clone());
        let (token, mut headers) = (request.url.ends_with("/token"), Headers::new());
        headers.set("content-type", if token { "application/json" } else { "text/event-stream" });
        let body = if token { r#"{"access_token":"t","expires_in":3600}"# } else { "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n" };
        Box::pin(async move { Ok(HttpResponse::with_text(200, "OK", headers, body)) })
    }
}

#[tokio::test]
async fn endpoints_derive_from_location_and_project() {
    let url_for = |api_key: &str, model: &str, location: &str| {
        let recorder = Arc::new(Recorder(std::sync::Mutex::new(Vec::new())));
        let settings: HashMap<String, String> = [("project", "my-project"), ("location", location)].iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        let target = ProviderSettings { base_url: None, api_key: api_key.to_owned(), headers: Headers::new(), settings, model_id: model.to_owned(), fetch: recorder.clone() };
        let home = Home::new(Some(&authorized_user()));
        let adc = home.adc();
        async move {
            let built = create_with_adc(target, adc).unwrap();
            stream_text(&built).await.unwrap();
            drop(home);
            let urls = recorder.0.lock().unwrap().clone();
            urls.last().unwrap().clone()
        }
    };
    let suffix = "streamGenerateContent?alt=sse";
    assert_eq!(url_for("", "gemini-2.5-flash", "us-central1").await, format!("https://us-central1-aiplatform.googleapis.com/v1beta1/projects/my-project/locations/us-central1/publishers/google/models/gemini-2.5-flash:{suffix}"));
    assert_eq!(url_for("", "gemini-2.5-flash", "global").await, format!("https://aiplatform.googleapis.com/v1beta1/projects/my-project/locations/global/publishers/google/models/gemini-2.5-flash:{suffix}"));
    assert_eq!(url_for("", "gemini-2.5-flash", "eu").await, format!("https://aiplatform.eu.rep.googleapis.com/v1beta1/projects/my-project/locations/eu/publishers/google/models/gemini-2.5-flash:{suffix}"));
    assert_eq!(url_for("", "gemini-2.5-flash", "us").await, format!("https://aiplatform.us.rep.googleapis.com/v1beta1/projects/my-project/locations/us/publishers/google/models/gemini-2.5-flash:{suffix}"));
    assert_eq!(url_for("", "endpoints/123", "us-central1").await, format!("https://us-central1-aiplatform.googleapis.com/v1beta1/projects/my-project/locations/us-central1/endpoints/123:{suffix}"));
    assert_eq!(url_for("key", "gemini-2.5-flash", "").await, format!("https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.5-flash:{suffix}"));
}

// ------------------------------------------------------------------ Vertex: the other credential files
//
// The bodies, headers and messages pinned below are what `GoogleAuth` under node sent and threw for the same files, over
// a fake `window.fetch`. Real Google endpoints are not part of any of this.

const IAM_PATH: &str = "/v1/projects/-/serviceAccounts/target@p.iam:generateAccessToken";
const WORKLOAD_AUDIENCE: &str = "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/pool/providers/prov";
const WORKFORCE_AUDIENCE: &str = "//iam.googleapis.com/locations/global/workforcePools/pool/providers/prov";
const STS_FORM_HEAD: &str = "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange&audience=%2F%2Fiam.googleapis.com%2Fprojects%2F1%2Flocations%2Fglobal%2FworkloadIdentityPools%2Fpool%2Fproviders%2Fprov&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform&requested_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aaccess_token&subject_token=";

/// Routes by the start of the path; anything else is the model's stream.
fn routed(routes: Vec<(&'static str, Reply)>) -> Mock {
    Mock::start(Arc::new(move |seen, _| routes.iter().find(|(path, _)| seen.target.starts_with(path)).map_or_else(simple_stream, |(_, reply)| reply.clone())))
}

fn at(mock: &Mock, path: &str) -> Vec<common::Seen> {
    mock.requests().into_iter().filter(|r| r.target.starts_with(path)).collect()
}

fn iam_reply() -> Reply {
    Reply::json(200, r#"{"accessToken":"iam-tok","expireTime":"2099-01-01T00:00:00.123456Z"}"#)
}

fn sts_reply() -> Reply {
    Reply::json(200, r#"{"access_token":"sts-tok","issued_token_type":"x","token_type":"Bearer","expires_in":3600}"#)
}

fn source_token_reply() -> Reply {
    Reply::json(200, r#"{"access_token":"src-tok","expires_in":3600}"#)
}

fn user_json() -> Json {
    Json::parse(&authorized_user()).unwrap()
}

fn impersonated(source: Json) -> String {
    jobj! {
        "type": "impersonated_service_account",
        "service_account_impersonation_url": format!("https://iamcredentials.googleapis.com{IAM_PATH}"),
        "source_credentials": source,
    }
    .to_text()
}

/// One step over `credentials`, for its outcome. The mock outlives it, for what it saw.
async fn run(credentials: &str, mock: &Mock) -> Result<String, AiError> {
    run_with(credentials, mock, |adc| adc).await
}

async fn run_with(credentials: &str, mock: &Mock, adc: impl FnOnce(Adc) -> Adc) -> Result<String, AiError> {
    let home = Home::new(Some(credentials));
    let model = create_with_adc(target(mock, false, Headers::new()), adc(home.adc())).unwrap();
    stream_text(&model).await
}

#[tokio::test]
async fn impersonation_spends_the_source_token_on_generate_access_token() {
    let home = Home::new(Some(
        &jobj! {
            "type": "impersonated_service_account",
            // Only the target is read out of this URL, as upstream reads it: nothing goes to that host.
            "service_account_impersonation_url": format!("https://elsewhere.example{IAM_PATH}"),
            "source_credentials": user_json(),
            "delegates": vec![Json::from("a@p"), Json::from("b@p")],
            "scopes": vec![Json::from("ignored")],
        }
        .to_text(),
    ));
    let mock = routed(vec![("/token", source_token_reply()), (IAM_PATH, iam_reply())]);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");

    let exchange = token_requests(&mock).remove(0);
    assert_eq!(exchange.body, "refresh_token=1%2F%2Fref+tok&client_id=cid.apps&client_secret=sec%2Fret%2B&grant_type=refresh_token");
    let generate = at(&mock, IAM_PATH).remove(0);
    assert_eq!(generate.method, "POST");
    assert_eq!(generate.body, format!(r#"{{"delegates":["a@p","b@p"],"scope":["{SCOPE}"],"lifetime":"3600s"}}"#));
    assert_eq!(generate.header("authorization"), Some("Bearer src-tok"));
    assert_eq!(generate.header("content-type"), Some("application/json"));
    assert_eq!(generate.header("user-agent"), Some("google-api-nodejs-client/10.9.1"));
    assert_eq!(model_requests(&mock)[0].header("authorization"), Some("Bearer iam-tok"));

    // Both tokens are alive: nothing is fetched again.
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");
    assert_eq!((token_requests(&mock).len(), at(&mock, IAM_PATH).len(), model_requests(&mock).len()), (1, 1, 2));
}

#[tokio::test]
async fn an_expired_impersonated_token_is_renewed_on_the_cached_source_token() {
    let home = Home::new(Some(&impersonated(user_json())));
    let mock = routed(vec![("/token", source_token_reply()), (IAM_PATH, Reply::json(200, r#"{"accessToken":"iam-tok","expireTime":"2000-01-01T00:00:00Z"}"#))]);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    stream_text(&model).await.unwrap();
    assert_eq!((token_requests(&mock).len(), at(&mock, IAM_PATH).len()), (1, 2));

    // A stamp `Date.parse` cannot read is `NaN`, which reads as no expiry at all.
    let home = Home::new(Some(&impersonated(user_json())));
    let mock = routed(vec![("/token", source_token_reply()), (IAM_PATH, Reply::json(200, r#"{"accessToken":"iam-tok","expireTime":"later"}"#))]);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    stream_text(&model).await.unwrap();
    assert_eq!(at(&mock, IAM_PATH).len(), 1);
}

#[tokio::test]
async fn impersonation_takes_a_service_account_source_and_the_files_settings() {
    let source = Json::parse(&service_account(&pkcs8_pem())).unwrap();
    let file = jobj! {
        "type": "impersonated_service_account",
        "service_account_impersonation_url": format!("https://iamcredentials.googleapis.com{IAM_PATH}"),
        "source_credentials": source,
        "lifetime": 600u64,
    };
    let mock = routed(vec![("/token", source_token_reply()), (IAM_PATH, iam_reply())]);
    assert_eq!(run(&file.to_text(), &mock).await.unwrap(), "hi there");
    check_assertion(token_requests(&mock).remove(0).body.rsplit('=').next().unwrap());
    assert_eq!(at(&mock, IAM_PATH).remove(0).body, format!(r#"{{"delegates":[],"scope":["{SCOPE}"],"lifetime":"600s"}}"#));

    // A source in another universe moves the endpoint with it.
    let mut source = user_json();
    source.as_obj_mut().unwrap().insert("universe_domain", Json::from("u.example"));
    let recorder = Arc::new(Recorder(std::sync::Mutex::new(Vec::new())));
    let home = Home::new(Some(&impersonated(source)));
    let settings: HashMap<String, String> = [("project", "p"), ("location", "global")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    let provider = ProviderSettings { base_url: None, api_key: String::new(), headers: Headers::new(), settings, model_id: "m".to_owned(), fetch: recorder.clone() };
    let model = create_with_adc(provider, home.adc()).unwrap();
    let _ = stream_text(&model).await;
    assert_eq!(recorder.0.lock().unwrap()[1], format!("https://iamcredentials.u.example{IAM_PATH}"));
}

#[tokio::test]
async fn impersonation_failures_read_as_upstream_words_them() {
    let failing = |source: Json, routes: Vec<(&'static str, Reply)>| async move {
        let mock = routed(routes);
        let error = run(&impersonated(source), &mock).await.unwrap_err();
        assert!(model_requests(&mock).is_empty());
        classify(&error)
    };
    let source_ok = || ("/token", source_token_reply());
    let step = failing(user_json(), vec![source_ok(), (IAM_PATH, Reply::json(403, r#"{"error":{"code":403,"message":"denied","status":"PERMISSION_DENIED"}}"#))]).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "PERMISSION_DENIED: unable to impersonate: denied"));
    let step = failing(user_json(), vec![source_ok(), (IAM_PATH, Reply::new(403).header("content-type", "text/plain").body("nope"))]).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "unable to impersonate: Error: nope"));
    let invalid = r#"{"error":"invalid_grant","error_description":"bad"}"#;
    let step = failing(user_json(), vec![("/token", Reply::json(400, invalid))]).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "unable to impersonate: Error: invalid_grant"));
    let service = Json::parse(&service_account(&pkcs8_pem())).unwrap();
    let step = failing(service, vec![("/token", Reply::json(400, invalid))]).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "unable to impersonate: Error: invalid_grant: bad"));
    let step = failing(user_json(), vec![source_ok(), (IAM_PATH, Reply::json(200, "{}"))]).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "Could not refresh access token."));
    // What gaxios would have retried is the host's to retry, wherever in the chain it happened.
    let step = failing(user_json(), vec![source_ok(), (IAM_PATH, Reply::json(503, r#"{"error":{"message":"busy","status":"UNAVAILABLE"}}"#))]).await;
    assert_eq!((step.kind, step.message.as_str(), step.status), ("transient", "UNAVAILABLE: unable to impersonate: busy", Some(503)));
    let step = failing(user_json(), vec![("/token", Reply::json(503, r#"{"error":"backend_error"}"#))]).await;
    assert_eq!((step.kind, step.message.as_str(), step.status), ("transient", "unable to impersonate: Error: backend_error", Some(503)));
}

fn external(rest: Json) -> String {
    let mut file = jobj! { "type": "external_account", "audience": WORKLOAD_AUDIENCE, "subject_token_type": "urn:x" };
    for (name, value) in rest.as_obj().unwrap().iter() {
        file.as_obj_mut().unwrap().insert(name, value.clone());
    }
    file.to_text()
}

/// A file beside the credentials, for a `credential_source` to read.
fn token_file(home: &Home, name: &str, contents: &str) -> String {
    let path = home.0.join(name);
    std::fs::write(&path, contents).unwrap();
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn a_file_subject_token_goes_through_sts_as_it_is() {
    let files = Home::new(None);
    let source = jobj! { "file": token_file(&files, "tok.txt", "file-tok\n") };
    let credentials = external(jobj! { "credential_source": source });
    let home = Home::new(Some(&credentials));
    let mock = routed(vec![("/v1/token", sts_reply())]);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");

    let exchange = at(&mock, "/v1/token").remove(0);
    assert_eq!(exchange.method, "POST");
    // The default `token_url`, and the newline stays, as upstream sends it.
    assert_eq!(exchange.body, format!("{STS_FORM_HEAD}file-tok%0A&subject_token_type=urn%3Ax"));
    assert_eq!(exchange.header("content-type"), Some("application/x-www-form-urlencoded;charset=UTF-8"));
    assert_eq!(exchange.header("accept"), Some("application/json"));
    assert_eq!(exchange.header("x-goog-api-client"), Some("gl-rust auth/10.9.1 google-byoid-sdk source/file sa-impersonation/false config-lifetime/false"));
    assert_eq!(exchange.header("authorization"), None);
    assert_eq!(exchange.header("user-agent"), None);
    assert_eq!(model_requests(&mock)[0].header("authorization"), Some("Bearer sts-tok"));

    // Cached by `expires_in` and read from the file again only on a refresh.
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");
    assert_eq!(at(&mock, "/v1/token").len(), 1);

    // Shorter than the eager threshold, or missing: as the other kinds read them.
    let mock = routed(vec![("/v1/token", Reply::json(200, r#"{"access_token":"t","expires_in":60}"#))]);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    stream_text(&model).await.unwrap();
    assert_eq!(at(&mock, "/v1/token").len(), 2);
    let mock = routed(vec![("/v1/token", Reply::json(200, r#"{"access_token":"t"}"#))]);
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    stream_text(&model).await.unwrap();
    assert_eq!(at(&mock, "/v1/token").len(), 1);
}

#[tokio::test]
async fn sts_then_impersonation_when_the_file_asks_for_it() {
    let files = Home::new(None);
    let mock = routed(vec![("/sts", sts_reply()), ("/sa", iam_reply())]);
    let credentials = external(jobj! {
        "token_url": format!("{}/sts", mock.base),
        "service_account_impersonation_url": format!("{}/sa/serviceAccounts/target@p.iam:generateAccessToken", mock.base),
        "service_account_impersonation": jobj! { "token_lifetime_seconds": 2800u64 },
        "credential_source": jobj! { "file": token_file(&files, "tok.json", r#"{"t":"file-json-tok"}"#), "format": jobj! { "type": "json", "subject_token_field_name": "t" } },
    });
    assert_eq!(run(&credentials, &mock).await.unwrap(), "hi there");
    let exchange = at(&mock, "/sts").remove(0);
    assert_eq!(exchange.body, format!("{STS_FORM_HEAD}file-json-tok&subject_token_type=urn%3Ax"));
    assert_eq!(exchange.header("x-goog-api-client"), Some("gl-rust auth/10.9.1 google-byoid-sdk source/file sa-impersonation/true config-lifetime/true"));
    let generate = at(&mock, "/sa").remove(0);
    assert_eq!(generate.body, format!(r#"{{"scope":["{SCOPE}"],"lifetime":"2800s"}}"#));
    assert_eq!(generate.header("authorization"), Some("Bearer sts-tok"));
    assert_eq!(generate.header("accept"), Some("application/json"));
    assert_eq!(model_requests(&mock)[0].header("authorization"), Some("Bearer iam-tok"));
}

#[tokio::test]
async fn a_url_subject_token_as_text_or_json() {
    let mock = routed(vec![("/subject", Reply::new(200).header("content-type", "text/plain").body("url-tok")), ("/v1/token", sts_reply())]);
    let source = jobj! { "url": format!("{}/subject", mock.base), "headers": jobj! { "Metadata": "True" } };
    assert_eq!(run(&external(jobj! { "credential_source": source }), &mock).await.unwrap(), "hi there");
    let subject = at(&mock, "/subject").remove(0);
    assert_eq!(subject.method, "GET");
    assert_eq!(subject.header("metadata"), Some("True"));
    assert_eq!(subject.header("accept"), Some("*/*"));
    assert_eq!(subject.header("x-goog-api-client"), Some("gl-rust"));
    let exchange = at(&mock, "/v1/token").remove(0);
    assert_eq!(exchange.body, format!("{STS_FORM_HEAD}url-tok&subject_token_type=urn%3Ax"));
    assert_eq!(exchange.header("x-goog-api-client"), Some("gl-rust auth/10.9.1 google-byoid-sdk source/url sa-impersonation/false config-lifetime/false"));

    let mock = routed(vec![("/subject", Reply::json(200, r#"{"access_token":"url-json-tok"}"#)), ("/v1/token", sts_reply())]);
    let source = jobj! { "url": format!("{}/subject", mock.base), "format": jobj! { "type": "json", "subject_token_field_name": "access_token" } };
    assert_eq!(run(&external(jobj! { "credential_source": source }), &mock).await.unwrap(), "hi there");
    assert_eq!(at(&mock, "/subject").remove(0).header("accept"), Some("application/json"));
    assert_eq!(at(&mock, "/v1/token").remove(0).body, format!("{STS_FORM_HEAD}url-json-tok&subject_token_type=urn%3Ax"));
}

#[tokio::test]
async fn client_authentication_wins_over_the_workforce_project() {
    let files = Home::new(None);
    let source = || jobj! { "file": token_file(&files, "tok.txt", "tok") };
    let mock = routed(vec![("/v1/token", sts_reply())]);
    let credentials = external(jobj! { "audience": WORKFORCE_AUDIENCE, "workforce_pool_user_project": "proj", "credential_source": source() });
    run(&credentials, &mock).await.unwrap();
    let exchange = at(&mock, "/v1/token").remove(0);
    assert!(exchange.body.ends_with("&subject_token=tok&subject_token_type=urn%3Ax&options=%7B%22userProject%22%3A%22proj%22%7D"), "{}", exchange.body);
    assert_eq!(exchange.header("authorization"), None);

    let mock = routed(vec![("/v1/token", sts_reply())]);
    let credentials = external(jobj! { "audience": WORKFORCE_AUDIENCE, "workforce_pool_user_project": "proj", "client_id": "cid", "client_secret": "sec", "credential_source": source() });
    run(&credentials, &mock).await.unwrap();
    let exchange = at(&mock, "/v1/token").remove(0);
    assert!(exchange.body.ends_with("&subject_token=tok&subject_token_type=urn%3Ax"), "{}", exchange.body);
    assert_eq!(exchange.header("authorization"), Some("Basic Y2lkOnNlYw=="));

    // A client id alone authenticates with an empty secret.
    let mock = routed(vec![("/v1/token", sts_reply())]);
    run(&external(jobj! { "client_id": "cid", "credential_source": source() }), &mock).await.unwrap();
    assert_eq!(at(&mock, "/v1/token").remove(0).header("authorization"), Some("Basic Y2lkOg=="));
}

#[tokio::test]
async fn external_account_failures_read_as_upstream_words_them() {
    let files = Home::new(None);
    let file = jobj! { "file": token_file(&files, "tok.txt", "tok") };
    let failing = |credentials: String, routes: Vec<(&'static str, Reply)>| async move {
        let mock = routed(routes);
        let error = run(&credentials, &mock).await.unwrap_err();
        assert!(model_requests(&mock).is_empty());
        classify(&error)
    };
    let with_file = || external(jobj! { "credential_source": file.clone() });
    let step = failing(with_file(), vec![("/v1/token", Reply::json(400, r#"{"error":"invalid_grant","error_description":"bad","error_uri":"http://u"}"#))]).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "Error code invalid_grant: bad - http://u"));
    let step = failing(with_file(), vec![("/v1/token", Reply::new(401).header("content-type", "text/plain").body("nope"))]).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "Error code undefined"));
    let step = failing(with_file(), vec![("/v1/token", Reply::json(503, r#"{"error":"backend"}"#))]).await;
    assert_eq!((step.kind, step.message.as_str(), step.status), ("transient", "Error code backend", Some(503)));
    let step = failing(with_file(), vec![("/v1/token", Reply::json(200, r#"{"expires_in":3600}"#))]).await;
    assert_eq!(step.message, "Could not refresh access token.");

    let impersonating = external(jobj! { "service_account_impersonation_url": format!("https://iamcredentials.googleapis.com{IAM_PATH}"), "credential_source": file.clone() });
    let step = failing(impersonating, vec![("/v1/token", sts_reply()), (IAM_PATH, Reply::json(403, r#"{"error":{"code":403,"message":"denied","status":"PERMISSION_DENIED"}}"#))]).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "denied"));

    let missing = external(jobj! { "credential_source": jobj! { "file": "/nowhere/at/all" } });
    let step = failing(missing, vec![]).await;
    assert!(step.message.starts_with("The file at /nowhere/at/all does not exist, or it is not a file. "), "{}", step.message);
    let no_field = external(jobj! { "credential_source": jobj! { "file": token_file(&files, "tok.json", r#"{"t":"x"}"#), "format": jobj! { "type": "json", "subject_token_field_name": "nope" } } });
    let step = failing(no_field, vec![]).await;
    assert_eq!(step.message, "Unable to parse the subject_token from the credential_source file");

    let url_source = |base: &str, format: Option<Json>| jobj! { "url": format!("{base}/subject"), "format"?: format };
    let mock = routed(vec![("/subject", Reply::new(404).header("content-type", "text/plain").body("gone"))]);
    let error = run(&external(jobj! { "credential_source": url_source(&mock.base, None) }), &mock).await.unwrap_err();
    assert_eq!((classify(&error).kind, error.message().as_str()), ("permanent", "gone"));
    let mock = routed(vec![("/subject", Reply::json(200, "{}"))]);
    let format = jobj! { "type": "json", "subject_token_field_name": "x" };
    let error = run(&external(jobj! { "credential_source": url_source(&mock.base, Some(format)) }), &mock).await.unwrap_err();
    assert_eq!(error.message(), "Unable to parse the subject_token from the credential_source URL");
    let mock = routed(vec![("/subject", Reply::json(500, r#"{"error":{"message":"down"}}"#))]);
    let error = run(&external(jobj! { "credential_source": url_source(&mock.base, None) }), &mock).await.unwrap_err();
    assert_eq!((classify(&error).kind, error.message().as_str()), ("transient", "down"));
}

fn aws_routes() -> Vec<(&'static str, Reply)> {
    let text = |body: &str| Reply::new(200).header("content-type", "text/plain").body(body.to_owned());
    vec![
        ("/imds", text("imds-tok")),
        ("/az", text("us-east-2b")),
        ("/creds/role", Reply::json(200, r#"{"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY","Token":"SESSION/tok=="}"#)),
        ("/creds", text("role")),
        ("/v1/token", sts_reply()),
    ]
}

fn aws_file(base: &str) -> String {
    external(jobj! {
        "subject_token_type": "urn:ietf:params:aws:token-type:aws4_request",
        "credential_source": jobj! {
            "environment_id": "aws1",
            "region_url": format!("{base}/az"),
            "url": format!("{base}/creds"),
            "imdsv2_session_token_url": format!("{base}/imds"),
            "regional_cred_verification_url": "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15",
        },
    })
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            out.push(u8::from_str_radix(&text[at + 1..at + 3], 16).unwrap());
            at += 3;
        } else {
            out.push(if bytes[at] == b'+' { b' ' } else { bytes[at] });
            at += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn an_aws_source_signs_get_caller_identity_with_the_vms_keys() {
    let mock = routed(aws_routes());
    assert_eq!(run(&aws_file(&mock.base), &mock).await.unwrap(), "hi there");

    let requests = mock.requests();
    let order: Vec<(&str, &str)> = requests.iter().map(|r| (r.method.as_str(), r.target.as_str())).take(6).collect();
    assert_eq!(order, [("PUT", "/imds"), ("GET", "/az"), ("PUT", "/imds"), ("GET", "/creds"), ("GET", "/creds/role"), ("POST", "/v1/token")]);
    assert_eq!(requests[0].header("x-aws-ec2-metadata-token-ttl-seconds"), Some("300"));
    for at in [1, 3, 4] {
        assert_eq!(requests[at].header("x-aws-ec2-metadata-token"), Some("imds-tok"));
    }
    assert_eq!(requests[4].header("accept"), Some("application/json"));

    let exchange = &requests[5];
    assert_eq!(exchange.header("x-goog-api-client"), Some("gl-rust auth/10.9.1 google-byoid-sdk source/aws sa-impersonation/false config-lifetime/false"));
    let subject = exchange.body.split('&').find_map(|pair| pair.strip_prefix("subject_token=")).unwrap();
    // Encoded once by `encodeURIComponent` and again by the form.
    let signed = Json::parse(&percent_decoded(&percent_decoded(subject))).unwrap();
    let url = "https://sts.us-east-2.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15";
    assert_eq!(signed.str_at("url"), Some(url));
    assert_eq!(signed.str_at("method"), Some("POST"));
    let headers: Vec<(&str, &str)> = signed.get("headers").unwrap().as_arr().unwrap().iter().map(|h| (h.str_at("key").unwrap(), h.str_at("value").unwrap())).collect();
    let names: Vec<&str> = headers.iter().map(|(name, _)| *name).collect();
    assert_eq!(names, ["authorization", "host", "x-amz-date", "x-amz-security-token", "x-goog-cloud-target-resource"]);
    assert_eq!(headers[1].1, "sts.us-east-2.amazonaws.com");
    assert_eq!(headers[3].1, "SESSION/tok==");
    assert_eq!(headers[4].1, WORKLOAD_AUDIENCE);
    let credentials = Credentials {
        region: "us-east-2".to_owned(),
        access_key_id: "AKIDEXAMPLE".to_owned(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
        session_token: Some("SESSION/tok==".to_owned()),
    };
    let expected = sign(&SignInput { method: "POST", url, headers: &Headers::new(), body: "", credentials: &credentials, service: "sts", datetime: headers[2].1 }).unwrap();
    assert_eq!(Some(headers[0].1), expected.headers.get("authorization"));
    assert_eq!(model_requests(&mock)[0].header("authorization"), Some("Bearer sts-tok"));
}

#[tokio::test]
async fn aws_failures_read_as_upstream_words_them() {
    let verification = "https://sts.{region}.amazonaws.com";
    let mock = routed(vec![("/az", Reply::new(200).header("content-type", "text/plain").body("us-east-2b"))]);
    let no_region = external(jobj! { "credential_source": jobj! { "environment_id": "aws1", "regional_cred_verification_url": verification } });
    assert_eq!(run(&no_region, &mock).await.unwrap_err().message(), "Unable to determine AWS region due to missing \"options.credential_source.region_url\"");
    assert!(mock.requests().is_empty());
    let no_url = external(jobj! { "credential_source": jobj! { "environment_id": "aws1", "region_url": format!("{}/az", mock.base), "regional_cred_verification_url": verification } });
    assert_eq!(run(&no_url, &mock).await.unwrap_err().message(), "Unable to determine AWS role name due to missing \"options.credential_source.url\"");

    let mut routes = aws_routes();
    routes[0] = ("/imds", Reply::json(500, r#"{"error":{"message":"imds down"}}"#));
    let mock = routed(routes);
    let error = run(&aws_file(&mock.base), &mock).await.unwrap_err();
    assert_eq!((classify(&error).kind, error.message().as_str()), ("transient", "imds down"));

    let mut routes = aws_routes();
    routes[2] = ("/creds/role", Reply::json(200, r#"{"SecretAccessKey":"never-show-this"}"#));
    let mock = routed(routes);
    let error = run(&aws_file(&mock.base), &mock).await.unwrap_err();
    assert_eq!(error.message(), "The AWS security credentials response does not contain AccessKeyId");
}

#[tokio::test]
async fn an_external_authorized_user_refreshes_and_keeps_the_rotated_token() {
    let mock = routed(vec![("/rotate", Reply::json(200, r#"{"access_token":"eau-tok","expires_in":60,"refresh_token":"rt/2"}"#))]);
    let credentials = jobj! {
        "type": "external_account_authorized_user",
        "audience": WORKFORCE_AUDIENCE,
        "client_id": "cid",
        "client_secret": "sec",
        "refresh_token": "rt/1",
        "token_url": format!("{}/rotate", mock.base),
    };
    let home = Home::new(Some(&credentials.to_text()));
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");
    assert_eq!(model_requests(&mock)[0].header("authorization"), Some("Bearer eau-tok"));
    // Under the eager threshold, so the next call refreshes, with what the last reply handed back.
    stream_text(&model).await.unwrap();
    let refreshes = at(&mock, "/rotate");
    assert_eq!(refreshes[0].method, "POST");
    assert_eq!(refreshes[0].body, "grant_type=refresh_token&refresh_token=rt%2F1");
    assert_eq!(refreshes[1].body, "grant_type=refresh_token&refresh_token=rt%2F2");
    assert_eq!(refreshes[0].header("authorization"), Some("Basic Y2lkOnNlYw=="));
    assert_eq!(refreshes[0].header("accept"), Some("application/json"));
    assert_eq!(refreshes[0].header("user-agent"), Some("google-api-nodejs-client/10.9.1"));

    // A live token is reused, and the default endpoint is the universe's.
    let mock = routed(vec![("/v1/oauthtoken", Reply::json(200, r#"{"access_token":"t","expires_in":3600}"#))]);
    let credentials = jobj! { "type": "external_account_authorized_user", "client_id": "cid", "client_secret": "sec", "refresh_token": "rt" };
    let home = Home::new(Some(&credentials.to_text()));
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    stream_text(&model).await.unwrap();
    assert_eq!(at(&mock, "/v1/oauthtoken").len(), 1);

    let failing = |reply: Reply| async move {
        let mock = routed(vec![("/v1/oauthtoken", reply)]);
        let credentials = jobj! { "type": "external_account_authorized_user", "client_id": "cid", "client_secret": "sec", "refresh_token": "rt" };
        classify(&run(&credentials.to_text(), &mock).await.unwrap_err())
    };
    let step = failing(Reply::json(400, r#"{"error":"invalid_grant"}"#)).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "Error code invalid_grant"));
    let step = failing(Reply::json(502, r#"{"error":"bad_gateway","error_description":"later"}"#)).await;
    assert_eq!((step.kind, step.message.as_str(), step.status), ("transient", "Error code bad_gateway: later", Some(502)));

    let no_token = jobj! { "type": "external_account_authorized_user", "client_id": "cid" }.to_text();
    assert_eq!(run(&no_token, &Mock::script(vec![simple_stream()])).await.unwrap_err().message(), "The incoming JSON object does not contain a refresh_token field");
}

// ---- GDCH

fn ec_key() -> EcKey<P256> {
    EcKey::<P256>::generate().unwrap()
}

fn ec_pkcs8_pem(key: &EcKey<P256>) -> String {
    let mut buffer = [0u8; 512];
    der_to_pem("PRIVATE KEY", key.to_pkcs8_der(&mut buffer).unwrap())
}

fn ec_sec1_pem(key: &EcKey<P256>) -> String {
    let mut buffer = [0u8; 512];
    der_to_pem("EC PRIVATE KEY", key.to_sec1_der(&mut buffer).unwrap())
}

fn gdch_file(pem: &str, token_uri: &str, ca_cert_path: Option<&str>) -> String {
    jobj! {
        "type": "gdch_service_account",
        "format_version": "1",
        "project": "proj",
        "private_key_id": "kid-1",
        "private_key": pem,
        "name": "svc",
        "token_uri": token_uri,
        "ca_cert_path"?: ca_cert_path,
    }
    .to_text()
}

/// The assertion as `createAssertion` writes it, with its signature checked against `key`.
fn check_gdch_assertion(assertion: &str, key: &EcKey<P256>, token_uri: &str) {
    let parts: Vec<&str> = assertion.split('.').collect();
    assert_eq!(parts.len(), 3, "{assertion}");
    assert_eq!(std::str::from_utf8(&base64url(parts[0])).unwrap(), r#"{"alg":"ES256","typ":"JWT","kid":"kid-1"}"#);
    let payload = Json::parse(std::str::from_utf8(&base64url(parts[1])).unwrap()).unwrap();
    let keys: Vec<&str> = payload.as_obj().unwrap().keys().collect();
    assert_eq!(keys, ["iss", "sub", "iat", "exp", "aud"]);
    assert_eq!(payload.str_at("iss"), Some("system:serviceaccount:proj:svc"));
    assert_eq!(payload.str_at("sub"), Some("system:serviceaccount:proj:svc"));
    assert_eq!(payload.str_at("aud"), Some(token_uri));
    let iat = payload.get("iat").unwrap().as_u64().unwrap();
    assert_eq!(payload.get("exp").unwrap().as_u64().unwrap(), iat + 3600);
    let signature = base64url(parts[2]);
    assert_eq!(signature.len(), 64, "ieee-p1363, not DER");
    let mut spki = [0u8; 256];
    let spki = key.to_spki_der(&mut spki).unwrap();
    let public = EcVerifyingKey::<P256>::from_x962_uncompressed(&spki[spki.len() - 65..]).unwrap();
    let message = format!("{}.{}", parts[0], parts[1]);
    public.verify::<Sha256>(&[message.as_bytes()], &signature).expect("the signature verifies");
    assert!(public.verify::<Sha256>(&[b"something else"], &signature).is_err());
}

#[tokio::test]
async fn gdch_exchanges_an_es256_assertion_at_its_token_uri() {
    let key = ec_key();
    let mock = routed(vec![("/gdch/token", Reply::json(200, r#"{"access_token":"gdch-tok","expires_in":3600}"#))]);
    let token_uri = format!("{}/gdch/token", mock.base);
    let home = Home::new(Some(&gdch_file(&ec_pkcs8_pem(&key), &token_uri, None)));
    let model = create_with_adc(target(&mock, false, Headers::new()), home.adc()).unwrap();
    assert_eq!(stream_text(&model).await.unwrap(), "hi there");

    let exchange = at(&mock, "/gdch/token").remove(0);
    assert_eq!(exchange.method, "POST");
    assert_eq!(exchange.header("content-type"), Some("application/json"));
    assert_eq!(exchange.header("accept"), Some("application/json"));
    let body = exchange.json();
    let keys: Vec<&str> = body.as_obj().unwrap().keys().collect();
    assert_eq!(keys, ["audience", "grant_type", "requested_token_type", "subject_token", "subject_token_type"]);
    // The origin of the endpoint the model is called on.
    assert_eq!(body.str_at("audience"), Some(mock.base.as_str()));
    assert_eq!(body.str_at("grant_type"), Some("urn:ietf:params:oauth:token-type:token-exchange"));
    assert_eq!(body.str_at("requested_token_type"), Some("urn:ietf:params:oauth:token-type:access_token"));
    assert_eq!(body.str_at("subject_token_type"), Some("urn:k8s:params:oauth:token-type:serviceaccount"));
    check_gdch_assertion(body.str_at("subject_token").unwrap(), &key, &token_uri);
    assert_eq!(model_requests(&mock)[0].header("authorization"), Some("Bearer gdch-tok"));

    stream_text(&model).await.unwrap();
    assert_eq!(at(&mock, "/gdch/token").len(), 1, "the token is reused");
}

#[tokio::test]
async fn gdch_reads_every_key_form_node_reads() {
    let key = ec_key();
    let parameters = "-----BEGIN EC PARAMETERS-----\nBggqhkjOPQMBBw==\n-----END EC PARAMETERS-----\n";
    for pem in [ec_sec1_pem(&key), format!("{parameters}{}", ec_sec1_pem(&key))] {
        let mock = routed(vec![("/gdch/token", Reply::json(200, r#"{"access_token":"t","expires_in":3600}"#))]);
        let token_uri = format!("{}/gdch/token", mock.base);
        run(&gdch_file(&pem, &token_uri, None), &mock).await.unwrap();
        check_gdch_assertion(at(&mock, "/gdch/token").remove(0).json().str_at("subject_token").unwrap(), &key, &token_uri);
    }
    // An RSA key, or nothing that is a key: refused before a request, in Node's words.
    for pem in [pkcs8_pem(), "not a pem".to_owned()] {
        let mock = routed(vec![]);
        let error = run(&gdch_file(&pem, &format!("{}/gdch/token", mock.base), None), &mock).await.unwrap_err();
        assert_eq!(error.message(), "error:1E08010C:DECODER routines::unsupported");
        assert!(mock.requests().is_empty());
    }
}

#[tokio::test]
async fn gdch_failures_read_as_upstream_words_them() {
    let pem = ec_pkcs8_pem(&ec_key());
    let failing = |reply: Reply| {
        let pem = pem.clone();
        async move {
            let mock = routed(vec![("/gdch/token", reply)]);
            let error = run(&gdch_file(&pem, &format!("{}/gdch/token", mock.base), None), &mock).await.unwrap_err();
            assert!(model_requests(&mock).is_empty());
            classify(&error)
        }
    };
    let step = failing(Reply::json(400, r#"{"error":{"message":"nope"}}"#)).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "Error getting access token for GDCH service account: nope, iss: svc"));
    let step = failing(Reply::json(200, r#"{"expires_in":3600}"#)).await;
    assert_eq!((step.kind, step.message.as_str()), ("permanent", "Error getting access token for GDCH service account: Token response did not contain an access_token., iss: svc"));
    let step = failing(Reply::json(200, r#"{"access_token":"x"}"#)).await;
    assert_eq!(step.message, "Error getting access token for GDCH service account: Token response did not contain an expires_in field., iss: svc");
    let step = failing(Reply::json(500, r#"{"error":{"message":"down"}}"#)).await;
    assert_eq!((step.kind, step.message.as_str(), step.status), ("transient", "Error getting access token for GDCH service account: down, iss: svc", Some(500)));
}

/// Forwards everything and writes down which requests carried roots of their own.
struct RootsRecorder {
    seen: std::sync::Mutex<Vec<(String, Option<usize>)>>,
    inner: DynFetch,
}

impl Fetch for RootsRecorder {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        self.seen.lock().unwrap().push((request.url.clone(), request.ca_roots.as_ref().map(|roots| roots.len())));
        self.inner.fetch(request)
    }
}

#[tokio::test]
async fn gdch_trusts_its_ca_file_for_the_token_request_only() {
    let certificate = "-----BEGIN CERTIFICATE-----
MIIBPTCB5aADAgECAgkAsAplLSSCSCEwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJ
bG9jYWxob3N0MCAXDTI2MTEwNDA0NTEwMloYDzIxMjYxMDExMDQ1MTAyWjAUMRIw
EAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAATu8r7V
MkaCij/obv/RY+S11TzERBS2nucBLst2p/TuEKDsPUifcUI7f7jRp6FgeKanEMJQ
ws1jtHzZUBy5dUPAox4wHDAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYI
KoZIzj0EAwIDRwAwRAIgJC0xLXzf7133ejtgqvIj3TU9kQKNnEocxKC3aWxKMkkC
IHXKPq1qJhsWbgn0fdiUI+qh0W1sqBIBSRg+ihtjVZ4i
-----END CERTIFICATE-----
";
    let files = Home::new(None);
    let pem = ec_pkcs8_pem(&ec_key());
    let mock = routed(vec![("/gdch/token", Reply::json(200, r#"{"access_token":"t","expires_in":3600}"#))]);
    let token_uri = format!("{}/gdch/token", mock.base);

    let ca = token_file(&files, "ca.pem", &format!("{certificate}{certificate}"));
    let home = Home::new(Some(&gdch_file(&pem, &token_uri, Some(&ca))));
    let mut settings = target(&mock, false, Headers::new());
    let recorder = Arc::new(RootsRecorder { seen: std::sync::Mutex::new(Vec::new()), inner: settings.fetch.clone() });
    settings.fetch = recorder.clone();
    let model = create_with_adc(settings, home.adc()).unwrap();
    stream_text(&model).await.unwrap();
    let seen = recorder.seen.lock().unwrap().clone();
    assert_eq!(seen[0], (token_uri.clone(), Some(2)));
    assert_eq!(seen[1].1, None, "the model's own request keeps the bundled roots, as upstream's fetch does");

    // A file that cannot be used fails before a request, and not as something to retry.
    let unusable = |path: &str| {
        let credentials = gdch_file(&pem, &token_uri, Some(path));
        let path = path.to_owned();
        async move {
            let mock = routed(vec![]);
            let error = run(&credentials, &mock).await.unwrap_err();
            assert!(mock.requests().is_empty());
            assert_eq!(classify(&error).kind, "permanent");
            let message = error.message();
            assert!(message.starts_with(&format!("Error reading certificate file from CA cert path, value '{path}': ")), "{message}");
        }
    };
    unusable("/nowhere/ca.pem").await;
    unusable(&token_file(&files, "empty.pem", "not a certificate")).await;
    unusable(&token_file(&files, "garbage.pem", "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")).await;
}

// ---- Executables

/// A shell script, as the `credential_source` that runs it. What it prints is `{output}`.
fn executable_file(files: &Home, script: &str, output_file: Option<&str>) -> String {
    let path = token_file(files, "exe.sh", script);
    external(jobj! {
        "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
        "credential_source": jobj! { "executable": jobj! { "command": format!("/bin/sh \"{path}\""), "timeout_millis": 5000u64, "output_file"?: output_file } },
    })
}

const GOOD_RESPONSE: &str = r#"{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:jwt","id_token":"exe-tok","expiration_time":4000000000}"#;

fn printing(json: &str) -> String {
    format!("cat <<'EOF'\n{json}\nEOF\n")
}

#[cfg(unix)]
#[tokio::test]
async fn an_executable_runs_only_when_the_variable_allows_it() {
    let files = Home::new(None);
    let ran = files.0.join("ran");
    let script = format!("touch \"{}\"\n{}", ran.display(), printing(GOOD_RESPONSE));
    let credentials = executable_file(&files, &script, None);
    let mock = routed(vec![("/v1/token", sts_reply())]);
    let error = run(&credentials, &mock).await.unwrap_err();
    assert_eq!(error.message(), "Pluggable Auth executables need to be explicitly allowed to run by setting the GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES environment Variable to 1.");
    assert!(!ran.exists());
    assert!(mock.requests().is_empty());

    assert_eq!(run_with(&credentials, &mock, Adc::allowing_executables).await.unwrap(), "hi there");
    assert!(ran.exists());
    let exchange = at(&mock, "/v1/token").remove(0);
    assert!(exchange.body.contains("&subject_token=exe-tok&subject_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Ajwt"), "{}", exchange.body);
    assert_eq!(exchange.header("x-goog-api-client"), Some("gl-rust auth/10.9.1 google-byoid-sdk source/executable sa-impersonation/false config-lifetime/false"));
    assert_eq!(model_requests(&mock)[0].header("authorization"), Some("Bearer sts-tok"));
}

#[cfg(unix)]
#[tokio::test]
async fn an_executable_sees_the_clients_variables_and_nothing_of_the_hosts() {
    let files = Home::new(None);
    let printed = files.0.join("env.txt");
    let output = files.0.join("out.json");
    let script = format!("env > \"{}\"\n{}", printed.display(), printing(GOOD_RESPONSE));
    let path = token_file(&files, "exe.sh", &script);
    let credentials = external(jobj! {
        "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
        "service_account_impersonation_url": format!("https://iamcredentials.googleapis.com{IAM_PATH}"),
        "credential_source": jobj! { "executable": jobj! { "command": format!("/bin/sh {path}"), "output_file": output.to_string_lossy().as_ref() } },
    });
    let mock = routed(vec![("/v1/token", sts_reply()), (IAM_PATH, iam_reply())]);
    assert_eq!(run_with(&credentials, &mock, Adc::allowing_executables).await.unwrap(), "hi there");
    let env = std::fs::read_to_string(&printed).unwrap();
    let lines: Vec<&str> = env.lines().collect();
    assert!(lines.contains(&format!("GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE={WORKLOAD_AUDIENCE}").as_str()), "{env}");
    assert!(lines.contains(&"GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE=urn:ietf:params:oauth:token-type:jwt"), "{env}");
    assert!(lines.contains(&"GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE=0"), "{env}");
    assert!(lines.contains(&"GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL=target@p.iam"), "{env}");
    assert!(lines.contains(&format!("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE={}", output.display()).as_str()), "{env}");
    // Cargo set this for the test process: the cleared environment is what keeps it from the child.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    assert!(!env.contains("CARGO_MANIFEST_DIR"), "{env}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_good_answer_in_the_output_file_saves_running_the_executable() {
    let files = Home::new(None);
    let ran = files.0.join("ran");
    let output = token_file(&files, "out.json", GOOD_RESPONSE);
    let script = format!("touch \"{}\"\n{}", ran.display(), printing(GOOD_RESPONSE));
    let credentials = executable_file(&files, &script, Some(&output));
    let mock = routed(vec![("/v1/token", sts_reply())]);
    run_with(&credentials, &mock, Adc::allowing_executables).await.unwrap();
    assert!(!ran.exists());

    // Expired or empty, it is ignored.
    for stale in [GOOD_RESPONSE.replace("4000000000", "1000000000"), String::new()] {
        std::fs::write(&output, stale).unwrap();
        let _ = std::fs::remove_file(&ran);
        run_with(&credentials, &mock, Adc::allowing_executables).await.unwrap();
        assert!(ran.exists());
    }

    // An answer that cannot be read fails, without repeating what might be the token.
    std::fs::write(&output, "secret-token-text").unwrap();
    let error = run_with(&credentials, &mock, Adc::allowing_executables).await.unwrap_err();
    assert_eq!(error.message(), "The output file contained an invalid response.");
}

#[cfg(unix)]
#[tokio::test]
async fn executable_failures_read_as_upstream_words_them_without_its_output() {
    let failing = |script: String, output_file: bool| async move {
        let files = Home::new(None);
        let output = files.0.join("out.json");
        let credentials = executable_file(&files, &script, output_file.then(|| output.to_str().unwrap()));
        let mock = routed(vec![]);
        let error = run_with(&credentials, &mock, Adc::allowing_executables).await.unwrap_err();
        assert!(mock.requests().is_empty());
        error.message()
    };
    assert_eq!(failing(printing(r#"{"version":1,"success":false,"code":"401","message":"not today"}"#), false).await, "The executable failed with exit code: 401 and error message: not today.");
    assert_eq!(failing(printing(&GOOD_RESPONSE.replace(r#""version":1"#, r#""version":2"#)), false).await, "Version of executable is not currently supported, maximum supported version is 1.");
    assert_eq!(failing(printing(&GOOD_RESPONSE.replace("4000000000", "1000000000")), false).await, "Executable response is expired.");
    assert_eq!(
        failing(printing(&GOOD_RESPONSE.replace(r#","expiration_time":4000000000"#, "")), true).await,
        "The executable response must contain the `expiration_time` field for successful responses when an output_file has been specified in the configuration."
    );
    // What it printed may be the token: upstream repeats it here, and this does not.
    assert_eq!(failing("echo secret-token-text\nexit 3\n".to_owned(), false).await, "The executable failed with exit code: 3.");
    assert_eq!(failing("echo secret-token-text\n".to_owned(), false).await, "The executable returned an invalid response.");
    let files = Home::new(None);
    let missing = external(jobj! { "credential_source": jobj! { "executable": jobj! { "command": files.0.join("nothing-here").to_string_lossy().as_ref() } } });
    let mock = routed(vec![]);
    let message = run_with(&missing, &mock, Adc::allowing_executables).await.unwrap_err().message();
    assert!(message.starts_with("The executable could not be started: "), "{message}");
}

// ---- What holds for every flow

/// Nothing in any of these failures names a secret, in the message or anywhere else the error carries.
#[tokio::test]
async fn failures_never_carry_a_secret() {
    const SECRETS: [&str; 6] = ["SUBJECT-SECRET", "CLIENT-SECRET", "REFRESH-SECRET", "ACCESS-SECRET", "AWS-SECRET", "SESSION-SECRET"];
    let check = |error: AiError| {
        let shown = format!("{} {error:?}", error.message());
        for secret in SECRETS {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        assert!(!shown.contains(&base64_simd::STANDARD.encode_to_string(b"cid:CLIENT-SECRET")), "{shown}");
    };
    let files = Home::new(None);
    let source = || jobj! { "file": token_file(&files, "tok.txt", "SUBJECT-SECRET") };
    let replies = || [
        Reply::json(400, r#"{"error":"invalid_grant","error_description":"bad"}"#),
        Reply::json(403, r#"{"error":{"message":"denied","status":"PERMISSION_DENIED"}}"#),
        Reply::json(503, r#"{"error":"busy"}"#),
        Reply::new(500).header("content-type", "text/plain").body("oops"),
    ];
    for reply in replies() {
        let mock = routed(vec![("/v1/token", reply)]);
        check(run(&external(jobj! { "client_id": "cid", "client_secret": "CLIENT-SECRET", "credential_source": source() }), &mock).await.unwrap_err());
    }
    for reply in replies() {
        let mock = routed(vec![("/v1/token", Reply::json(200, r#"{"access_token":"ACCESS-SECRET","expires_in":3600}"#)), (IAM_PATH, reply)]);
        let credentials = external(jobj! { "service_account_impersonation_url": format!("https://iamcredentials.googleapis.com{IAM_PATH}"), "credential_source": source() });
        check(run(&credentials, &mock).await.unwrap_err());
    }
    for reply in replies() {
        let mock = routed(vec![("/v1/oauthtoken", reply)]);
        let credentials = jobj! { "type": "external_account_authorized_user", "client_id": "cid", "client_secret": "CLIENT-SECRET", "refresh_token": "REFRESH-SECRET" };
        check(run(&credentials.to_text(), &mock).await.unwrap_err());
    }
    for reply in replies() {
        let mock = routed(vec![("/token", Reply::json(200, r#"{"access_token":"ACCESS-SECRET","expires_in":3600}"#)), (IAM_PATH, reply)]);
        let user = jobj! { "type": "authorized_user", "client_id": "cid", "client_secret": "CLIENT-SECRET", "refresh_token": "REFRESH-SECRET" };
        check(run(&impersonated(user), &mock).await.unwrap_err());
    }
    for reply in replies() {
        let mock = routed(vec![("/token", reply)]);
        let user = jobj! { "type": "authorized_user", "client_id": "cid", "client_secret": "CLIENT-SECRET", "refresh_token": "REFRESH-SECRET" };
        check(run(&impersonated(user), &mock).await.unwrap_err());
    }
    // AWS keys that signed a request the token endpoint then refused.
    let mut routes = aws_routes();
    routes[2] = ("/creds/role", Reply::json(200, r#"{"AccessKeyId":"AKID","SecretAccessKey":"AWS-SECRET","Token":"SESSION-SECRET"}"#));
    routes[4] = ("/v1/token", Reply::json(400, r#"{"error":"invalid_grant"}"#));
    let mock = routed(routes);
    check(run(&aws_file(&mock.base), &mock).await.unwrap_err());
    // A subject token file that is meant to be JSON and is not: the parser names a position, not the contents.
    let broken = jobj! { "file": token_file(&files, "bad.json", r#"{"t":"SUBJECT-SECRET"#), "format": jobj! { "type": "json", "subject_token_field_name": "t" } };
    let mock = routed(vec![]);
    check(run(&external(jobj! { "credential_source": broken }), &mock).await.unwrap_err());
}

#[tokio::test]
async fn a_cancelled_step_stops_waiting_in_every_flow() {
    let hanging = || Reply::new(200).header("content-type", "application/json").body("{").hanging();
    let cancelled = |credentials: String, mock: Mock| async move {
        let home = Home::new(Some(&credentials));
        let model = create_with_adc(target(&mock, false, Headers::new()), home.adc().allowing_executables()).unwrap();
        let cancel = Cancel::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });
        let outcome = tokio::time::timeout(Duration::from_secs(10), model.do_stream(call_options(cancel))).await.expect("the cancel is honoured");
        assert!(matches!(outcome, Err(AiError::Abort)));
    };
    let files = Home::new(None);
    let file = jobj! { "file": token_file(&files, "tok.txt", "tok") };

    cancelled(impersonated(user_json()), routed(vec![("/token", hanging())])).await;
    cancelled(impersonated(user_json()), routed(vec![("/token", source_token_reply()), (IAM_PATH, hanging())])).await;
    cancelled(external(jobj! { "credential_source": file.clone() }), routed(vec![("/v1/token", hanging())])).await;
    let impersonating = external(jobj! { "service_account_impersonation_url": format!("https://iamcredentials.googleapis.com{IAM_PATH}"), "credential_source": file.clone() });
    cancelled(impersonating, routed(vec![("/v1/token", sts_reply()), (IAM_PATH, hanging())])).await;
    let mock = routed(vec![("/subject", hanging())]);
    let by_url = external(jobj! { "credential_source": jobj! { "url": format!("{}/subject", mock.base) } });
    cancelled(by_url, mock).await;
    let mut routes = aws_routes();
    routes[1] = ("/az", hanging());
    let mock = routed(routes);
    let aws = aws_file(&mock.base);
    cancelled(aws, mock).await;
    let eau = jobj! { "type": "external_account_authorized_user", "client_id": "cid", "refresh_token": "rt" };
    cancelled(eau.to_text(), routed(vec![("/v1/oauthtoken", hanging())])).await;
    let mock = routed(vec![("/gdch/token", hanging())]);
    let gdch = gdch_file(&ec_pkcs8_pem(&ec_key()), &format!("{}/gdch/token", mock.base), None);
    cancelled(gdch, mock).await;
    // An executable is killed, not just left behind.
    #[cfg(unix)]
    {
        let pid_file = files.0.join("pid");
        let sleeping = executable_file(&files, &format!("echo $$ > \"{}\"\nexec sleep 30\n", pid_file.display()), None);
        cancelled(sleeping, routed(vec![])).await;
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = || std::process::Command::new("kill").args(["-0", pid.trim()]).stderr(std::process::Stdio::null()).status().unwrap().success();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive() {
            assert!(std::time::Instant::now() < deadline, "the executable outlived the cancel");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
