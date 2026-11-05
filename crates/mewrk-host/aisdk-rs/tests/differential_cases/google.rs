//! Differential cases for `google` and `vertex` (see `tests/differential.rs`). Vertex runs in Express Mode, the only
//! mode with a local upstream: standard mode's token endpoint is fixed, so it is covered by `tests/google.rs`.

use std::sync::OnceLock;

use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;
use regex::Regex;

use super::{differential, differential_normalized};
use crate::common::Reply;

fn base(family: &str, model: &str, extra: Json) -> Json {
    let mut payload = jobj! {
        "family": family,
        "apiKey": "k",
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

fn google(model: &str, extra: Json) -> Json {
    base("google", model, extra)
}

fn vertex(model: &str, extra: Json) -> Json {
    base("vertex", model, extra)
}

fn read_tool() -> Json {
    jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "path": jobj! { "type": "string" } } } }
}

/// `generateId`'s 16 characters. Signatures and call ids in these cases are never that long.
fn ids(text: &str) -> String {
    static ID: OnceLock<Regex> = OnceLock::new();
    ID.get_or_init(|| Regex::new(r#""[0-9A-Za-z]{16}""#).unwrap()).replace_all(text, r#""<id>""#).into_owned()
}

fn text_stream() -> Reply {
    Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"}}],"responseId":"r1","modelVersion":"gemini-x"}"#,
        r#"{"candidates":[{"content":{"parts":[{"text":" there"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"totalTokenCount":13,"cachedContentTokenCount":4},"responseId":"r1"}"#,
    ])
}

fn thought_tool_stream() -> Reply {
    Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"thinking ","thought":true,"thoughtSignature":"SIG-R"}]}}],"responseId":"r1"}"#,
        r#"{"candidates":[{"content":{"parts":[{"text":"more","thought":true}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"text":"visible","thoughtSignature":"SIG-T"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"SIG-E"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_1","name":"read","args":{"path":"a"}},"thoughtSignature":"SIG-C"},{"functionCall":{"id":"call_2","name":"read","args":{"path":"b"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"thoughtsTokenCount":7,"totalTokenCount":20,"serviceTier":"standard","promptTokensDetails":[{"modality":"TEXT","tokenCount":10}]}}"#,
    ])
}

fn replay_history(namespace: &str) -> Vec<Json> {
    let signed = |signature: &str| {
        let mut options = mewrk_aisdk::json::Obj::new();
        options.insert(namespace, jobj! { "thoughtSignature": signature });
        Json::Obj(options)
    };
    vec![
        jobj! { "role": "user", "content": vec![
            jobj! { "type": "text", "text": "look at this" },
            jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png" },
            jobj! { "type": "file", "data": "JVBERi0xLjQ=", "mediaType": "application/pdf", "filename": "a.pdf" },
            jobj! { "type": "file", "data": "https://www.youtube.com/watch?v=abc_D-1", "mediaType": "video/mp4" },
            jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/*" },
        ] },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "t", "providerOptions": signed("SIG-R") },
            jobj! { "type": "reasoning", "text": "" },
            jobj! { "type": "text", "text": "a", "providerOptions": signed("SIG-T") },
            jobj! { "type": "text", "text": "" },
            jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png", "providerOptions": signed("SIG-F") },
            jobj! { "type": "reasoning-file", "data": "iVBORw0KGgo=", "mediaType": "image/png", "providerOptions": signed("SIG-RF") },
            jobj! { "type": "tool-call", "toolCallId": "c0", "toolName": "read", "input": jobj! { "path": "x" }, "providerOptions": signed("SIG-C") },
            jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "read", "input": jobj! { "path": "y" } },
            jobj! { "type": "tool-call", "toolCallId": "c2", "toolName": "read", "input": jobj! { "path": "z" } },
            jobj! { "type": "tool-call", "toolCallId": "c3", "toolName": "read", "input": jobj! { "path": "w" } },
            jobj! { "type": "tool-call", "toolCallId": "c4", "toolName": "read", "input": jobj! { "path": "v" } },
        ] },
        jobj! { "role": "tool", "content": vec![
            jobj! {
                "type": "tool-result", "toolCallId": "c0", "toolName": "read",
                "output": jobj! { "type": "content", "value": vec![
                    jobj! { "type": "text", "text": "body" },
                    jobj! { "type": "text", "text": "more" },
                    jobj! { "type": "file-data", "data": "iVBORw0KGgo=", "mediaType": "image/png" },
                    jobj! { "type": "file-data", "data": "JVBERi0xLjQ=", "mediaType": "application/pdf" },
                    jobj! { "type": "file-url", "url": "https://example.test/a.png", "mediaType": "image/png" },
                    jobj! { "type": "file-url", "url": "data:image/png;base64,iVBORw0KGgo=", "mediaType": "image/png" },
                ] },
            },
            jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "read", "output": jobj! { "type": "text", "value": "plain" } },
            jobj! { "type": "tool-result", "toolCallId": "c2", "toolName": "read", "output": jobj! { "type": "json", "value": jobj! { "z": 1u64, "a": vec![Json::from(1u64)] } } },
            jobj! { "type": "tool-result", "toolCallId": "c3", "toolName": "read", "output": jobj! { "type": "error-text", "value": "bad" } },
            jobj! { "type": "tool-result", "toolCallId": "c4", "toolName": "read", "output": jobj! { "type": "execution-denied", "reason": "no" } },
        ] },
        jobj! { "role": "user", "content": "again" },
    ]
}

#[test]
fn plain_text_and_the_request_shape() {
    differential("google text", Some("/v1beta"), google("gemini-2.5-flash", jobj! {}), vec![text_stream()]);
    differential("google trailing slash", Some("/v1beta/"), google("gemini-2.5-flash", jobj! { "maxOutputTokens": 100u64, "headers": jobj! { "x-extra": "1", "User-Agent": "mine", "X-Goog-Api-Key": "over" } }), vec![text_stream()]);
    differential("google empty key", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "apiKey": "" }), vec![text_stream()]);
    differential("google full model path", Some("/v1beta"), google("tunedModels/x", jobj! {}), vec![text_stream()]);
    let mut no_system = google("gemini-2.5-flash", jobj! {});
    no_system.as_obj_mut().unwrap().remove("system");
    differential("google no system", Some("/v1beta"), no_system, vec![text_stream()]);
}

#[test]
fn thought_signatures_tools_and_usage() {
    for model in ["gemini-2.5-flash", "gemini-3-pro-preview"] {
        differential(&format!("{model} encrypted"), Some("/v1beta"), google(model, jobj! { "tools": vec![read_tool()], "reasoning": "high", "reasoningContent": "encrypted" }), vec![thought_tool_stream()]);
        differential(&format!("{model} plaintext"), Some("/v1beta"), google(model, jobj! { "tools": vec![read_tool()], "reasoning": "low", "reasoningContent": "plaintext" }), vec![thought_tool_stream()]);
    }
    differential("vertex signatures", Some("/v1"), vertex("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()], "reasoning": "medium", "reasoningContent": "plaintext" }), vec![thought_tool_stream()]);
}

#[test]
fn thinking_by_model_generation() {
    for (model, level) in [
        ("gemini-2.5-pro", "high"),
        ("gemini-2.5-flash", "max"),
        ("gemini-2.5-flash", "medium"),
        ("gemini-2.0-flash", "low"),
        ("gemini-3-pro-preview", "medium"),
        ("gemini-3-pro-preview", "max"),
        ("gemini-3-pro-image-preview", "high"),
        ("gemini-3.5-flash", "low"),
        ("gemini-3.7-flash", "low"),
        ("gemini-flash-latest", "high"),
        ("gemini-pro-latest", "high"),
        ("publishers/google/models/gemini-3.6-flash", "high"),
        ("gemini-2.5-flash", "xhigh"),
        ("gemini-3.5-flash", "xhigh"),
        ("GEMINI-3-PRO-PREVIEW", "high"),
        ("gemma-3-27b-it", "high"),
        ("nano-banana-pro-preview", "high"),
        ("aqa", "low"),
    ] {
        differential(&format!("{model} {level}"), Some("/v1beta"), google(model, jobj! { "reasoning": level }), vec![text_stream()]);
    }
}

#[test]
fn replay_with_files_and_tool_results() {
    for model in ["gemini-3-pro-preview", "gemini-2.5-flash"] {
        differential(&format!("{model} replay"), Some("/v1beta"), google(model, jobj! { "messages": replay_history("google"), "tools": vec![read_tool()] }), vec![text_stream()]);
    }
    for (name, namespace) in [("own", "googleVertex"), ("legacy", "vertex"), ("cross", "google")] {
        differential(&format!("vertex replay {name}"), Some("/v1"), vertex("gemini-3-pro-preview", jobj! { "messages": replay_history(namespace), "tools": vec![read_tool()] }), vec![text_stream()]);
    }
    differential("google replay reads vertex signatures", Some("/v1beta"), google("gemini-3-pro-preview", jobj! { "messages": replay_history("vertex") }), vec![text_stream()]);
}

#[test]
fn replay_of_server_side_tools_and_assistant_files() {
    let server = |id: &str, kind: &str, signature: Option<&str>| jobj! { "google": jobj! { "serverToolCallId": id, "serverToolType": kind, "thoughtSignature"?: signature } };
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "tool-call", "toolCallId": "s1", "toolName": "server:GOOGLE_SEARCH_WEB", "input": jobj! { "q": 1u64 }, "providerExecuted": true, "providerOptions": server("s1", "GOOGLE_SEARCH_WEB", Some("SIG-S")) },
            jobj! { "type": "tool-result", "toolCallId": "s1", "toolName": "server:GOOGLE_SEARCH_WEB", "output": jobj! { "type": "json", "value": jobj! { "ok": true } }, "providerOptions": server("s1", "GOOGLE_SEARCH_WEB", Some("SIG-U")) },
            jobj! { "type": "tool-call", "toolCallId": "s2", "toolName": "server:URL_CONTEXT", "input": "{\"u\":2}", "providerExecuted": true, "providerOptions": server("s2", "URL_CONTEXT", None) },
            jobj! { "type": "tool-result", "toolCallId": "s2", "toolName": "server:URL_CONTEXT", "output": jobj! { "type": "text", "value": "dropped" }, "providerOptions": server("s2", "URL_CONTEXT", None) },
            jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "read", "input": jobj! {} },
            jobj! { "type": "tool-result", "toolCallId": "other", "toolName": "read", "output": jobj! { "type": "text", "value": "not a server tool" } },
        ] },
        jobj! { "role": "tool", "content": vec![
            jobj! { "type": "tool-result", "toolCallId": "s3", "toolName": "server:LATE", "output": jobj! { "type": "json", "value": jobj! { "late": true } }, "providerOptions": server("s3", "LATE", Some("SIG-L")) },
            jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "read", "output": jobj! { "type": "json", "value": vec![Json::from(1u64)] } },
        ] },
    ];
    differential("server tool replay gemini 3", Some("/v1beta"), google("gemini-3-pro-preview", jobj! { "messages": history.clone(), "tools": vec![read_tool()] }), vec![text_stream()]);
    differential("server tool replay gemini 2", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "messages": history.clone(), "tools": vec![read_tool()] }), vec![text_stream()]);
    differential("server tool replay vertex", Some("/v1"), vertex("gemini-3-pro-preview", jobj! { "messages": history, "tools": vec![read_tool()] }), vec![text_stream()]);

    let thought = jobj! { "google": jobj! { "thought": true, "thoughtSignature": "SIG" } };
    let files = jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "file", "data": jobj! { "type": "text", "text": "é" }, "mediaType": "text/plain", "providerOptions": thought.clone() },
            jobj! { "type": "file", "data": jobj! { "type": "text", "text": "x" }, "mediaType": "text" },
            jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "google": "files/abc" } }, "mediaType": "application/pdf", "providerOptions": thought.clone() },
            jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png", "providerOptions": thought },
            jobj! { "type": "reasoning", "text": "numeric signature", "providerOptions": jobj! { "google": jobj! { "thoughtSignature": 5u64 } } },
        ] },
    ] };
    differential("assistant files", Some("/v1beta"), google("gemini-3-pro-preview", files), vec![text_stream()]);
    let reference_on_vertex = jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "google": "files/abc" } }, "mediaType": "application/pdf" }] },
    ] };
    differential("assistant reference on vertex", Some("/v1"), vertex("gemini-3-pro-preview", reference_on_vertex), vec![text_stream()]);
    let url_reasoning_file = jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "reasoning-file", "data": "https://www.youtube.com/watch?v=abc", "mediaType": "video/mp4" }] },
    ] };
    differential("reasoning file url refused", Some("/v1beta"), google("gemini-3-pro-preview", url_reasoning_file), vec![text_stream()]);
    let outputs = jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c", "toolName": "read", "input": jobj! {} }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c", "toolName": "read", "output": jobj! { "type": "content", "value": vec![
            jobj! { "type": "file-id", "fileId": "f1" },
            jobj! { "type": "file-reference", "providerReference": jobj! { "google": "files/x" } },
            jobj! { "type": "image-data", "data": "iVBORw0KGgo=", "mediaType": "image/png" },
            jobj! { "type": "image-url", "url": "https://example.test/i.png" },
            jobj! { "type": "custom", "providerOptions": jobj! { "x": jobj! { "y": 1u64 } } },
        ] } }] },
    ] };
    differential("tool output kinds gemini 3", Some("/v1beta"), google("gemini-3-pro-preview", outputs.clone()), vec![text_stream()]);
    differential("tool output kinds vertex", Some("/v1"), vertex("gemini-3-pro-preview", outputs.clone()), vec![text_stream()]);
    differential("tool output kinds gemini 2", Some("/v1beta"), google("gemini-2.5-flash", outputs), vec![text_stream()]);
    let empty_output = jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c", "toolName": "read", "input": jobj! {} }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c", "toolName": "read", "output": jobj! { "type": "content", "value": Vec::<Json>::new() } }] },
    ] };
    differential("empty tool output gemini 3", Some("/v1beta"), google("gemini-3-pro-preview", empty_output.clone()), vec![text_stream()]);
    differential("empty tool output gemini 2", Some("/v1beta"), google("gemini-2.5-flash", empty_output), vec![text_stream()]);
}

#[test]
fn file_variety_and_references() {
    let parts = |parts: Vec<Json>| jobj! { "messages": vec![jobj! { "role": "user", "content": parts }] };
    let text_file = jobj! { "type": "file", "data": jobj! { "type": "text", "text": "héllo" }, "mediaType": "text/markdown" };
    let text_partial = jobj! { "type": "file", "data": jobj! { "type": "text", "text": "x" }, "mediaType": "text" };
    let reference = jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "google": "files/abc" } }, "mediaType": "application/pdf" };
    let wrong_reference = jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "openai": "file-1" } }, "mediaType": "application/pdf" };
    let external = jobj! { "type": "file", "data": "https://example.test/doc.pdf", "mediaType": "application/pdf" };
    differential("google text files", Some("/v1beta"), google("gemini-3-pro-preview", parts(vec![text_file.clone(), text_partial])), vec![text_stream()]);
    differential("google reference", Some("/v1beta"), google("gemini-3-pro-preview", parts(vec![reference.clone()])), vec![text_stream()]);
    differential("google wrong reference", Some("/v1beta"), google("gemini-3-pro-preview", parts(vec![wrong_reference])), vec![text_stream()]);
    differential("google external url", Some("/v1beta"), google("gemini-3-pro-preview", parts(vec![external.clone()])), vec![text_stream()]);
    differential("google short youtube url", Some("/v1beta"), google("gemini-2.0-flash", parts(vec![jobj! { "type": "file", "data": "https://youtu.be/abc_D-1?t=1", "mediaType": "video/mp4" }])), vec![text_stream()]);
    // Neither model takes this URL as it is, and the download it would need is refused before anything connects.
    let refused = jobj! { "type": "file", "data": "http://127.0.0.1:9/x.pdf", "mediaType": "application/pdf" };
    differential("google unsupported url", Some("/v1beta"), google("gemini-2.0-flash", parts(vec![refused.clone()])), vec![text_stream()]);
    differential("google http url", Some("/v1beta"), google("gemini-3-pro-preview", parts(vec![refused])), vec![text_stream()]);
    differential("vertex reference refused", Some("/v1"), vertex("gemini-3-pro-preview", parts(vec![reference])), vec![text_stream()]);
    differential("vertex gs url", Some("/v1"), vertex("gemini-3-pro-preview", parts(vec![jobj! { "type": "file", "data": "gs://bucket/a.pdf", "mediaType": "application/pdf" }, external])), vec![text_stream()]);
    let assistant_url = jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "file", "data": "https://www.youtube.com/watch?v=abc", "mediaType": "video/mp4" }] },
    ] };
    differential("assistant url refused", Some("/v1beta"), google("gemini-3-pro-preview", assistant_url), vec![text_stream()]);
}

#[test]
fn provider_options() {
    let full = jobj! {
        "labels": jobj! { "z": "1", "a": "2" },
        "unknownKey": 1u64,
        "serviceTier": "flex",
        "requestType": "shared",
        "sharedRequestType": "priority",
        "retrievalConfig": jobj! { "latLng": jobj! { "longitude": 2.5, "latitude": 1.5, "x": 1u64 }, "y": 1u64 },
        "imageConfig": jobj! { "imageSize": "2K", "personGeneration": "ALLOW_ALL", "aspectRatio": "16:9", "imageOutputOptions": jobj! { "compressionQuality": 80u64, "mimeType": "image/png" } },
        "mediaResolution": "MEDIA_RESOLUTION_LOW",
        "audioTimestamp": true,
        "threshold": "BLOCK_NONE",
        "safetySettings": vec![jobj! { "threshold": "OFF", "category": "HARM_CATEGORY_HARASSMENT", "x": 1u64 }],
        "structuredOutputs": false,
        "cachedContent": "cachedContents/abc",
        "thinkingConfig": jobj! { "thinkingLevel": "low", "includeThoughts": true, "thinkingBudget": 100u64, "other": 1u64 },
        "responseModalities": vec![Json::from("TEXT"), Json::from("IMAGE")],
        "streamFunctionCallArguments": true,
    };
    differential("google options", Some("/v1beta"), google("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()], "reasoning": "high", "providerOptions": jobj! { "google": full.clone() } }), vec![text_stream()]);
    differential("google foreign options", Some("/v1beta"), google("gemini-3-pro-preview", jobj! { "providerOptions": jobj! { "googleVertex": full.clone() } }), vec![text_stream()]);
    for key in ["googleVertex", "vertex", "google"] {
        let mut options = mewrk_aisdk::json::Obj::new();
        options.insert(key, full.clone());
        differential(&format!("vertex options {key}"), Some("/v1"), vertex("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()], "reasoning": "high", "providerOptions": Json::Obj(options) }), vec![text_stream()]);
    }
    differential("vertex name precedence", Some("/v1"), vertex("gemini-3-pro-preview", jobj! { "providerOptions": jobj! {
        "vertex": jobj! { "cachedContent": "legacy" },
        "googleVertex": jobj! { "cachedContent": "new" },
        "google": jobj! { "cachedContent": "cross" },
    } }), vec![text_stream()]);
    differential("thinking only through options", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "providerOptions": jobj! { "google": jobj! { "thinkingConfig": jobj! { "includeThoughts": true } } } }), vec![text_stream()]);
    differential("empty thinking config", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "providerOptions": jobj! { "google": jobj! { "thinkingConfig": jobj! {} } } }), vec![text_stream()]);
    differential("threshold only", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "providerOptions": jobj! { "google": jobj! { "threshold": "OFF" } } }), vec![text_stream()]);
    differential("retrieval only", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "providerOptions": jobj! { "google": jobj! { "retrievalConfig": jobj! {} } } }), vec![text_stream()]);
    differential("stream args only", Some("/v1"), vertex("gemini-3-pro-preview", jobj! { "providerOptions": jobj! { "googleVertex": jobj! { "streamFunctionCallArguments": true } } }), vec![text_stream()]);
    differential("invalid enum", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "providerOptions": jobj! { "google": jobj! { "threshold": "NOPE" } } }), vec![text_stream()]);
    differential("invalid null", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "providerOptions": jobj! { "google": jobj! { "cachedContent": Json::Null } } }), vec![text_stream()]);
    differential("invalid object", Some("/v1"), vertex("gemini-2.5-flash", jobj! { "providerOptions": jobj! { "vertex": "nope" } }), vec![text_stream()]);
}

#[test]
fn native_search_and_grounding() {
    let grounded = Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"found"}]},"groundingMetadata":{"webSearchQueries":["q"],"searchEntryPoint":{"renderedContent":"<div/>"},"groundingChunks":[{"web":{"uri":"https://a.example/x","title":"A"}},{"web":{"uri":"https://a.example/x","title":"dup"}},{"web":{"uri":"https://b.example/"}},{"image":{"sourceUri":"https://c.example/","imageUri":"https://c.example/i.png","title":"C"}},{"retrievedContext":{"uri":"https://d.example/","title":"D"}},{"retrievedContext":{"uri":"gs://bucket/e.pdf","title":"E"}},{"retrievedContext":{"fileSearchStore":"stores/f","title":"F"}},{"maps":{"uri":"https://g.example/","title":"G"}},{"maps":{"title":"no uri"}}],"groundingSupports":[{"segment":{"startIndex":0,"endIndex":5,"text":"found"},"groundingChunkIndices":[0],"confidenceScores":[0.5]}],"retrievalMetadata":{}}}]}"#,
        r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP","groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://h.example/"}}]},"urlContextMetadata":{"urlMetadata":[{"retrievedUrl":"https://u.example/","urlRetrievalStatus":"OK"}]}}],"usageMetadata":{"promptTokenCount":1}}"#,
    ]);
    let search = jobj! { "nativeSearch": jobj! { "maxUses": 3u64 } };
    differential("search only", Some("/v1beta"), google("gemini-2.5-flash", search.clone()), vec![grounded.clone()]);
    differential("search old model", Some("/v1beta"), google("gemini-1.5-pro", search.clone()), vec![grounded.clone()]);
    let with_tools = jobj! { "nativeSearch": jobj! { "maxUses": 3u64 }, "tools": vec![read_tool()] };
    differential("search with tools gemini 2", Some("/v1beta"), google("gemini-2.5-flash", with_tools.clone()), vec![grounded.clone()]);
    differential("search with tools gemini 3", Some("/v1beta"), google("gemini-3-pro-preview", with_tools.clone()), vec![grounded.clone()]);
    differential("vertex search with tools", Some("/v1"), vertex("gemini-3-pro-preview", with_tools), vec![grounded]);
}

#[test]
fn server_side_tools_and_generated_ids() {
    let server = Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"toolCall":{"toolType":"GOOGLE_SEARCH_WEB","args":{"queries":["q"]},"id":"srv_1"},"thoughtSignature":"SIG-S"},{"toolResponse":{"toolType":"GOOGLE_SEARCH_WEB","response":{"ok":true},"id":"srv_1"},"thoughtSignature":"SIG-U"},{"toolCall":{"toolType":"URL_CONTEXT","id":"srv_2"}},{"toolResponse":{"toolType":"URL_CONTEXT","id":"srv_x"}},{"toolResponse":{"toolType":"LATE","id":"srv_3"}},{"text":"done"}]},"finishReason":"STOP"}]}"#,
    ]);
    differential("server tools", Some("/v1beta"), google("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()] }), vec![server]);
    let no_ids = Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"a"}},"thoughtSignature":"S1"},{"functionCall":{"name":"read"}},{"functionCall":{"name":"read","args":"{\"path\":\"raw\"}"}}]},"finishReason":"STOP"}]}"#,
    ]);
    differential_normalized("generated call ids", Some("/v1beta"), google("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()] }), vec![no_ids], &ids);
    let code = Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"codeExecutionResult":{"outcome":"OUTCOME_OK"}},{"executableCode":{"code":"print(1)","language":"PYTHON","x":1}},{"codeExecutionResult":{"outcome":"OUTCOME_OK","output":"1"}},{"text":"done"}]},"finishReason":"STOP"}]}"#,
    ]);
    differential_normalized("code execution", Some("/v1beta"), google("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()] }), vec![code], &ids);
    // A call to an unknown tool fails locally and goes back to the model in the next step, signature and all.
    let unknown = Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"before","thoughtSignature":"SIG-T"},{"functionCall":{"name":"missing","args":{"a":1}},"thoughtSignature":"SIG-C"}]},"finishReason":"STOP"}]}"#,
    ]);
    differential_normalized("loop with a generated id", Some("/v1beta"), google("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()], "maxSteps": 2u64 }), vec![unknown.clone(), text_stream()], &ids);
    differential_normalized("vertex loop drops ids", Some("/v1"), vertex("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()], "maxSteps": 2u64 }), vec![unknown, text_stream()], &ids);
}

#[test]
fn file_output_parts() {
    let files = Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"look"},{"inlineData":{"mimeType":"image/png","data":"iVBORw0KGgo="},"thoughtSignature":"SIG-F"},{"text":"thought","thought":true},{"inlineData":{"mimeType":"image/webp","data":"AAAA"},"thought":true,"thoughtSignature":"SIG-RF"},{"text":"end"}]},"finishReason":"STOP"}]}"#,
    ]);
    differential("generated files", Some("/v1beta"), google("gemini-3-pro-image-preview", jobj! {}), vec![files.clone()]);
    differential("vertex generated files", Some("/v1"), vertex("gemini-3-pro-image-preview", jobj! {}), vec![files]);
}

#[test]
fn streamed_function_arguments() {
    let streamed = Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_1","name":"read","willContinue":true},"thoughtSignature":"SIG-C"}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.path","stringValue":"a\"b","willContinue":true}],"willContinue":true}}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.path","stringValue":"cé"}],"willContinue":true}}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.n.list[0].x","numberValue":1.5},{"jsonPath":"$.n.list[1].y","boolValue":false},{"jsonPath":"$.z","nullValue":"NULL_VALUE"}],"willContinue":true}}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{}}]}}]}"#,
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_2","name":"read","partialArgs":[{"jsonPath":"$.path","stringValue":"one"}]}}]},"finishReason":"STOP"}]}"#,
    ]);
    differential("streamed args", Some("/v1"), vertex("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()], "providerOptions": jobj! { "googleVertex": jobj! { "streamFunctionCallArguments": true } } }), vec![streamed.clone()]);
    differential("streamed args on google", Some("/v1beta"), google("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()] }), vec![streamed]);
    let unfinished = Reply::sse(&[
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_1","name":"read","partialArgs":[{"jsonPath":"$.path","stringValue":"a","willContinue":true}],"willContinue":true}}]},"finishReason":"STOP"}]}"#,
    ]);
    differential("unfinished streamed call", Some("/v1"), vertex("gemini-3-pro-preview", jobj! { "tools": vec![read_tool()] }), vec![unfinished]);
}

#[test]
fn tool_schemas_and_choice() {
    let tool = |name: &str, schema: Json| jobj! { "name": name, "description": "d", "inputSchema": schema };
    let schema = Json::parse(r##"{
        "type": "object",
        "required": ["a"],
        "description": "root",
        "additionalProperties": false,
        "$defs": { "leaf": { "type": "string", "minLength": 2 }, "a/b~c": { "type": "number" } },
        "definitions": { "old": { "type": "boolean", "description": "kept" } },
        "properties": {
            "a": { "type": ["string", "null"], "description": "nullable" },
            "b": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
            "c": { "anyOf": [{ "type": "string" }, { "type": "number" }, { "type": "null" }] },
            "d": { "oneOf": [{ "type": "string", "format": "date-time" }, { "type": "object" }] },
            "e": { "allOf": [{ "type": "object", "properties": { "x": { "type": "integer", "enum": [1, 2] } } }] },
            "f": { "enum": ["x", "y", null] },
            "g": { "const": "only" },
            "h": { "const": null },
            "i": { "type": "array", "items": { "type": "object", "description": "empty nested" } },
            "j": { "type": "array", "items": [{ "type": "string" }, true] },
            "k": { "$ref": "#/$defs/leaf", "description": "over" },
            "l": { "$ref": "#/$defs/a~1b~0c" },
            "m": { "$ref": "#/$defs/a%2Fb" },
            "n": { "$ref": "#/definitions/old" },
            "o": { "type": "boolean", "enum": [true] },
            "p": { "type": ["number", "null"], "enum": [1.5, null] },
            "q": true,
            "r": { "type": "null" },
            "s": { "type": ["null"] },
            "t": { "description": "", "format": "", "minLength": 0 }
        }
    }"##);
    let schema_without_bad_reference = {
        let mut value = schema.clone().unwrap();
        value.get_mut("properties").and_then(Json::as_obj_mut).unwrap().remove("m");
        value
    };
    let tools = vec![
        tool("full", schema_without_bad_reference),
        tool("empty", jobj! { "type": "object", "properties": jobj! {} }),
        tool("open", jobj! { "type": "object", "additionalProperties": true }),
        jobj! { "name": "nodesc", "inputSchema": jobj! { "type": "object", "properties": jobj! { "a": jobj! { "type": "string" } } } },
        tool("recursive", Json::parse(r##"{"type":"object","$defs":{"n":{"type":"object","properties":{"next":{"$ref":"#/$defs/n"}}}},"properties":{"root":{"$ref":"#/$defs/n"}}}"##).unwrap()),
    ];
    differential("schemas", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "tools": tools }), vec![text_stream()]);
    differential("bad reference", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "tools": vec![tool("bad", Json::parse(r##"{"type":"object","properties":{"a":{"$ref":"#/$defs/none"}}}"##).unwrap())] }), vec![text_stream()]);
    differential("mixed enum", Some("/v1beta"), google("gemini-2.5-flash", jobj! { "tools": vec![tool("bad", Json::parse(r#"{"type":"object","properties":{"a":{"enum":["x",1]}}}"#).unwrap())] }), vec![text_stream()]);
}

#[test]
fn gemma_system_and_message_placement() {
    differential("gemma", Some("/v1beta"), google("gemma-3-27b-it", jobj! { "system": "first" }), vec![text_stream()]);
    differential("gemma assistant first", Some("/v1beta"), google("gemma-3-27b-it", jobj! { "messages": vec![jobj! { "role": "assistant", "content": "a" }, jobj! { "role": "user", "content": "q" }] }), vec![text_stream()]);
    let lifted = jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "system", "content": "plan mode", "providerOptions": jobj! { "mewrk": jobj! { "systemAppend": true } } },
    ], "systemAppend": true, "systemDynamic": "tail" };
    differential("system append", Some("/v1beta"), google("gemini-2.5-flash", lifted), vec![text_stream()]);
}

#[test]
fn finish_reasons_and_odd_chunks() {
    for reason in ["MAX_TOKENS", "SAFETY", "RECITATION", "BLOCKLIST", "PROHIBITED_CONTENT", "SPII", "IMAGE_SAFETY", "MALFORMED_FUNCTION_CALL", "OTHER", "FINISH_REASON_UNSPECIFIED", "SOMETHING_NEW"] {
        let chunk = format!(r#"{{"candidates":[{{"content":{{"parts":[{{"text":"x"}}]}},"finishReason":"{reason}","finishMessage":"m","safetyRatings":[{{"category":"HARM","probability":"LOW","blocked":false}}]}}]}}"#);
        differential(&format!("finish {reason}"), Some("/v1beta"), google("gemini-2.5-flash", jobj! {}), vec![Reply::sse(&[&chunk])]);
    }
    let odd = |name: &str, events: &[&str]| differential(name, Some("/v1beta"), google("gemini-2.5-flash", jobj! {}), vec![Reply::sse(events)]);
    odd("no finish reason", &[r#"{"candidates":[{"content":{"parts":[{"text":"x"}]}}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1}}"#]);
    odd("empty candidates", &[r#"{"candidates":[],"promptFeedback":{"blockReason":"SAFETY"},"usageMetadata":{"promptTokenCount":2}}"#]);
    odd("no candidates", &[r#"{"usageMetadata":{"promptTokenCount":2}}"#, r#"{"candidates":[{"finishReason":"STOP"}]}"#]);
    odd("null content", &[r#"{"candidates":[{"content":null,"finishReason":"STOP"}],"responseId":null}"#]);
    odd("empty content", &[r#"{"candidates":[{"content":{},"finishReason":"STOP"}]}"#]);
    odd("no output", &[]);
    odd("error chunk is ignored", &[r#"{"candidates":[{"content":{"parts":[{"text":"x"}]}}]}"#, r#"{"error":{"code":503,"message":"overloaded","status":"UNAVAILABLE"}}"#]);
    odd("reasoning then text", &[r#"{"candidates":[{"content":{"parts":[{"text":"t","thought":true},{"text":"a"},{"text":"t2","thought":true},{"text":"b"}]},"finishReason":"STOP"}]}"#]);
    odd("empty text without a block", &[r#"{"candidates":[{"content":{"parts":[{"text":"","thoughtSignature":"S"},{"text":""}]},"finishReason":"STOP"}]}"#]);
    odd("signature on the first text", &[r#"{"candidates":[{"content":{"parts":[{"text":"a","thoughtSignature":"S1"},{"text":"b","thoughtSignature":"S2"},{"text":"c"}]},"finishReason":"STOP"}]}"#]);
    odd("malformed function call falls through", &[r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":1},"text":"x"}]},"finishReason":"STOP"}]}"#]);
    odd("unknown part keys", &[r#"{"candidates":[{"content":{"parts":[{"videoMetadata":{},"text":"x"},{"somethingNew":1}]},"finishReason":"STOP"}]}"#]);
    let invalid = Reply::sse(&[r#"{"candidates":[{"content":{"parts":[{"text":5}]}}]}"#]);
    differential_normalized("schema failure", Some("/v1beta"), google("gemini-2.5-flash", jobj! {}), vec![invalid], &schema_failure);
}

/// Zod's wording of a failure is not reproduced, only the failure and what it carries up to the message's first line.
fn schema_failure(text: &str) -> String {
    match text.find("Error message: ") {
        Some(at) => format!("{}<zod>", &text[..at]),
        None => text.to_owned(),
    }
}

#[test]
fn errors_and_their_classification() {
    let run = |name: &str, reply: Reply| differential(name, Some("/v1beta"), google("gemini-2.5-flash", jobj! {}), vec![reply]);
    run("429", Reply::json(429, r#"{"error":{"code":429,"message":"slow down","status":"RESOURCE_EXHAUSTED"}}"#).header("retry-after", "7"));
    run("400", Reply::json(400, r#"{"error":{"code":400,"message":"bad request","status":"INVALID_ARGUMENT","details":[{"x":1}]}}"#));
    run("401", Reply::json(401, r#"{"error":{"code":null,"message":"no key","status":"UNAUTHENTICATED"}}"#));
    run("503", Reply::json(503, r#"{"error":{"code":503,"message":"overloaded","status":"UNAVAILABLE"}}"#));
    run("schema mismatch falls back to the status", Reply::json(500, r#"{"error":{"message":"no status"}}"#));
    run("relay error shape", Reply::json(500, r#"{"message":"bad gateway"}"#));
    run("array error", Reply::json(400, r#"[{"error":{"code":400,"message":"wrapped","status":"INVALID_ARGUMENT"}}]"#));
    run("html error", Reply::new(404).header("content-type", "text/html").body("<html>nope</html>"));
    run("empty 503", Reply::new(503));
    differential("vertex 403", Some("/v1"), vertex("gemini-2.5-flash", jobj! {}), vec![Reply::json(403, r#"{"error":{"code":403,"message":"denied","status":"PERMISSION_DENIED"}}"#)]);
}

#[test]
fn vertex_express_mode_endpoints() {
    let model = |id: &str, extra: Json| vertex(id, extra);
    differential("vertex trailing slash", Some("/v1/"), model("gemini-2.5-flash", jobj! { "headers": jobj! { "x-goog-api-key": "ignored", "x-extra": "1" } }), vec![text_stream()]);
    differential("vertex publisher path", Some("/v1"), model("publishers/google/models/gemini-2.5-flash", jobj! {}), vec![text_stream()]);
    differential("vertex settings do not matter", Some("/v1"), model("gemini-2.5-flash", jobj! { "settings": jobj! { "project": "p", "location": "us-central1" } }), vec![text_stream()]);
    differential("vertex tuned model refused", Some("/v1"), model("endpoints/123", jobj! {}), vec![text_stream()]);
}

/// Standard mode needs a token before anything reaches the mock. These fail first, so nothing is contacted.
#[test]
fn vertex_standard_mode_settings_errors() {
    let standard = |settings: Json| vertex("gemini-2.5-flash", jobj! { "apiKey": "", "settings": settings });
    differential("no project or location", Some("/v1"), standard(jobj! {}), vec![text_stream()]);
    differential("no project", Some("/v1"), standard(jobj! { "location": "us-central1" }), vec![text_stream()]);
    differential("no location", Some("/v1"), standard(jobj! { "project": "p" }), vec![text_stream()]);
    differential("empty settings read as missing", Some("/v1"), standard(jobj! { "project": "", "location": "" }), vec![text_stream()]);
    differential("tuned model no project", Some("/v1"), vertex("endpoints/123", jobj! { "apiKey": "", "settings": jobj! { "location": "us" } }), vec![text_stream()]);
}

/// With no credentials anywhere, both sides end at the same refusal. This probes the link-local metadata address
/// and `metadata.google.internal` as upstream does, so it is skipped where a real credential could be picked up:
/// the Node side runs without `HOME`, the in-process one with the test runner's.
#[test]
fn vertex_standard_mode_without_credentials() {
    let home = std::env::var("HOME").unwrap_or_default();
    if std::path::Path::new(&home).join(".config/gcloud/application_default_credentials.json").exists() {
        eprintln!("vertex without credentials: skipped, this machine has an application default credentials file");
        return;
    }
    let payload = vertex("gemini-2.5-flash", jobj! { "apiKey": "", "settings": jobj! { "project": "p", "location": "us-central1" } });
    differential("no credentials", Some("/v1"), payload, vec![text_stream()]);
}
