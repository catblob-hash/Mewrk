use std::time::Duration;

use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;

use super::{differential, differential_normalized};
use crate::common::Reply;

const PNG: &str = "iVBORw0KGgo=";

fn tools() -> Vec<Json> {
    vec![
        jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "path": jobj! { "type": "string" } }, "additionalProperties": false } },
        jobj! { "name": "ls", "description": "e", "inputSchema": jobj! { "type": "object" } },
    ]
}

fn base(family: &str, model: &str, extra: Json) -> Json {
    let mut payload = jobj! {
        "family": family,
        "apiKey": "k",
        "modelId": model,
        "system": "S",
        "messages": vec![jobj! { "role": "user", "content": "hello" }],
        "tools": tools(),
        "maxSteps": 1u64,
    };
    if let (Some(object), Some(extra)) = (payload.as_obj_mut(), extra.as_obj()) {
        object.extend_from(extra);
    }
    payload
}

/// Both names for the one model.
fn each_family(name: &str, model: &str, extra: Json, replies: Vec<Reply>) {
    for family in ["openai-chat", "openai-compatible"] {
        differential(&format!("{name} {family}"), Some("/v1"), base(family, model, extra.clone()), replies.clone());
    }
}

/// Zod's report of a failed chunk is its own text, and the only part that is not reproduced.
fn without_zod_report(text: &str) -> String {
    text.split_once("Error message:").map_or_else(|| text.to_owned(), |(head, _)| head.to_owned())
}

/// V8's and serde's wording of a JSON syntax error: the one thing in a failed tool input that differs.
fn without_syntax_error_text(text: &str) -> String {
    let Some((head, tail)) = text.split_once("SyntaxError: ") else { return text.to_owned() };
    let rest = tail.split_once('"').map_or("", |(_, rest)| rest);
    format!("{head}SyntaxError\"{rest}")
}

fn chunk(delta: &str, finish: Option<&str>, usage: Option<&str>) -> String {
    let finish = finish.map_or("null".to_owned(), |f| format!("\"{f}\""));
    let usage = usage.map_or(String::new(), |u| format!(",\"usage\":{u}"));
    format!(r#"{{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"served-model","choices":[{{"index":0,"delta":{delta},"finish_reason":{finish}}}]{usage}}}"#)
}

fn sse(events: &[String]) -> Reply {
    Reply::sse(&events.iter().map(String::as_str).collect::<Vec<_>>())
}

const USAGE: &str = r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2,"accepted_prediction_tokens":1,"rejected_prediction_tokens":0}}"#;

fn text_stream() -> Reply {
    sse(&[
        chunk(r#"{"role":"assistant","content":""}"#, None, None),
        chunk(r#"{"content":"hi "}"#, None, None),
        chunk(r#"{"content":"there"}"#, Some("stop"), Some(USAGE)),
    ])
}

fn reasoning_tool_stream() -> Reply {
    sse(&[
        chunk(r#"{"role":"assistant","reasoning_content":"Let me "}"#, None, None),
        chunk(r#"{"reasoning_content":"think"}"#, None, None),
        chunk(r#"{"content":"ok"}"#, None, None),
        chunk(r#"{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":""},"extra_content":{"google":{"thought_signature":"SIG"}}}]}"#, None, None),
        chunk(r#"{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}"#, None, None),
        chunk(r#"{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}},{"index":1,"id":"call_2","function":{"name":"ls","arguments":"{}"}}]}"#, None, None),
        chunk(r#"{}"#, Some("tool_calls"), Some(USAGE)),
    ])
}

#[test]
fn plain_text_and_usage() {
    each_family("chat text", "plain-model", jobj! {}, vec![text_stream()]);
    each_family("chat max output", "plain-model", jobj! { "maxOutputTokens": 77u64 }, vec![text_stream()]);
    for family in ["openai-chat", "openai-compatible"] {
        differential("chat trailing slash", Some("/v1/"), base(family, "m", jobj! {}), vec![text_stream()]);
        differential("chat empty key", Some("/v1"), base(family, "m", jobj! { "apiKey": "" }), vec![text_stream()]);
        differential("chat key spaces", Some("/v1"), base(family, "m", jobj! { "apiKey": " k " }), vec![text_stream()]);
        differential("chat headers", Some("/v1"), base(family, "m", jobj! { "headers": jobj! { "X-Extra": "1", "Authorization": "Bearer other", "User-Agent": "host/1" } }), vec![text_stream()]);
    }
}

#[test]
fn reasoning_tools_and_signatures() {
    each_family("chat reasoning tools", "plain-model", jobj! { "reasoning": "high", "reasoningContent": "plaintext" }, vec![reasoning_tool_stream()]);
    each_family("chat reasoning field", "gpt-oss", jobj! {}, vec![sse(&[
        chunk(r#"{"reasoning":"from reasoning"}"#, None, None),
        chunk(r#"{"content":"a"}"#, None, None),
        chunk(r#"{"reasoning":"again"}"#, None, None),
        chunk(r#"{"content":"b"}"#, Some("stop"), None),
    ])]);
    each_family("chat empty reasoning content wins", "m", jobj! {}, vec![sse(&[
        chunk(r#"{"reasoning_content":"","reasoning":"ignored","content":"c"}"#, Some("length"), None),
    ])]);
    each_family("chat reasoning only", "m", jobj! {}, vec![sse(&[
        chunk(r#"{"reasoning_content":"just thought"}"#, Some("stop"), None),
    ])]);
    each_family("chat content filter", "m", jobj! {}, vec![sse(&[chunk(r#"{"content":"x"}"#, Some("content_filter"), None)])]);
    each_family("chat unknown finish", "m", jobj! {}, vec![sse(&[chunk(r#"{"content":"x"}"#, Some("eos"), None)])]);
}

#[test]
fn chunk_shapes_relays_vary() {
    each_family("chat second choice ignored", "m", jobj! {}, vec![Reply::sse(&[
        r#"{"id":"x","choices":[{"delta":{"content":"a"},"finish_reason":null},{"delta":{"content":"b"},"finish_reason":"length"}]}"#,
        r#"{"id":"x","choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ])]);
    each_family("chat empty choices first", "m", jobj! {}, vec![Reply::sse(&[
        r#"{"id":"first","model":"first-model","choices":[]}"#,
        r#"{"id":"second","model":"second-model","choices":[{"delta":{"content":"a"},"finish_reason":"stop"}]}"#,
    ])]);
    each_family("chat null delta", "m", jobj! {}, vec![Reply::sse(&[
        r#"{"choices":[{"delta":{"content":"a"}}]}"#,
        r#"{"choices":[{"delta":null,"finish_reason":"stop"}]}"#,
    ])]);
    each_family("chat no ids at all", "m", jobj! {}, vec![Reply::sse(&[
        r#"{"choices":[{"delta":{"role":"","tool_calls":[{"function":{"name":"ls","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
    ])]);
    each_family("chat nulls everywhere", "m", jobj! {}, vec![Reply::sse(&[
        r#"{"id":null,"created":null,"model":null,"usage":null,"choices":[{"delta":{"role":null,"content":null,"reasoning_content":null,"reasoning":null,"tool_calls":null},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":null,"id":"a","function":{"name":"ls","arguments":null},"extra_content":null}]},"finish_reason":"tool_calls"}]}"#,
    ])]);
    each_family("chat tool description missing", "m", jobj! { "tools": vec![jobj! { "name": "ls", "inputSchema": jobj! { "type": "object" } }] }, vec![text_stream()]);
    // Reasoning still open when a call that never got a name fails the stream.
    each_family("chat flush throws with reasoning open", "m", jobj! {}, vec![sse(&[
        chunk(r#"{"reasoning_content":"r"}"#, None, None),
        chunk(r#"{"content":"t"}"#, None, None),
        chunk(r#"{"reasoning_content":"r2"}"#, None, None),
        chunk(r#"{"tool_calls":[{"index":0,"id":"a","function":{"arguments":"{}"}}]}"#, None, None),
        chunk(r#"{"reasoning_content":"r3"}"#, Some("tool_calls"), None),
    ]).pause(Duration::from_millis(30))]);
}

#[test]
fn reasoning_levels_per_model() {
    for (model, level) in [("gpt-4o", "high"), ("gpt-5.6", "max"), ("o3", "max"), ("openai/gpt-5.1", "xhigh"), ("deepseek-r1", "max"), ("deepseek-r1", "low")] {
        each_family(&format!("chat level {model} {level}"), model, jobj! { "reasoning": level }, vec![text_stream()]);
    }
}

fn replay_history() -> Vec<Json> {
    vec![
        jobj! { "role": "user", "content": vec![
            jobj! { "type": "text", "text": "look" },
            jobj! { "type": "file", "data": PNG, "mediaType": "image/png" },
            jobj! { "type": "file", "data": PNG, "mediaType": "image/*" },
            jobj! { "type": "file", "data": "AAAA", "mediaType": "video/mp4" },
            jobj! { "type": "file", "data": "AAAA", "mediaType": "audio/wav" },
            jobj! { "type": "file", "data": "AAAA", "mediaType": "audio/mpeg" },
            jobj! { "type": "file", "data": "JVBERg==", "mediaType": "application/pdf", "filename": "a.pdf" },
            jobj! { "type": "file", "data": "JVBERg==", "mediaType": "application/pdf" },
            jobj! { "type": "file", "data": "77u/aGVsbG8=", "mediaType": "text/plain" },
        ] },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "first " },
            jobj! { "type": "text", "text": "a " },
            jobj! { "type": "reasoning", "text": "second" },
            jobj! { "type": "text", "text": "b" },
            jobj! { "type": "tool-call", "toolCallId": "c0", "toolName": "read", "input": jobj! { "path": "x" }, "providerOptions": jobj! { "openai-chat": jobj! { "thoughtSignature": "S0" }, "openai-compatible": jobj! { "thoughtSignature": "S0" } } },
            jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "ls", "input": jobj! {}, "providerOptions": jobj! { "google": jobj! { "thoughtSignature": "G1" } } },
            jobj! { "type": "tool-call", "toolCallId": "c2", "toolName": "ls", "input": jobj! {}, "providerOptions": jobj! { "openaiCompatible": jobj! { "custom": 1u64 } } },
        ] },
        jobj! { "role": "tool", "content": vec![
            jobj! { "type": "tool-result", "toolCallId": "c0", "toolName": "read", "output": jobj! { "type": "text", "value": "plain" } },
            jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "ls", "output": jobj! { "type": "json", "value": jobj! { "a": 1u64 } } },
            jobj! { "type": "tool-result", "toolCallId": "c2", "toolName": "ls", "output": jobj! { "type": "content", "value": vec![jobj! { "type": "text", "text": "body" }] } },
        ] },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "tool-call", "toolCallId": "c3", "toolName": "read", "input": jobj! { "path": "y" } },
        ] },
        jobj! { "role": "tool", "content": vec![
            jobj! { "type": "tool-result", "toolCallId": "c3", "toolName": "read", "output": jobj! { "type": "execution-denied" } },
        ] },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c4", "toolName": "read", "input": jobj! { "path": "z" } }] },
        jobj! { "role": "tool", "content": vec![
            jobj! { "type": "tool-result", "toolCallId": "c4", "toolName": "read", "output": jobj! { "type": "error-text", "value": "bad" } },
        ] },
        jobj! { "role": "user", "content": vec![jobj! { "type": "text", "text": "again", "providerOptions": jobj! { "openaiCompatible": jobj! { "name": "pat" } } }] },
    ]
}

#[test]
fn replayed_history_with_files_and_tool_results() {
    each_family("chat replay", "m", jobj! { "messages": replay_history() }, vec![text_stream()]);
    // Metadata spreads over what the message already has, keeping each key's position.
    each_family("chat metadata spreads", "m", jobj! { "messages": vec![
        jobj! { "role": "user", "content": vec![
            jobj! { "type": "text", "text": "t", "providerOptions": jobj! { "openaiCompatible": jobj! { "cache": 1u64 } } },
            jobj! { "type": "file", "data": PNG, "mediaType": "image/png", "providerOptions": jobj! { "openaiCompatible": jobj! { "detail": "low" } } },
        ], "providerOptions": jobj! { "openaiCompatible": jobj! { "role_note": "x" } } },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "text", "text": "a" }], "providerOptions": jobj! { "openaiCompatible": jobj! { "tool_calls": "over", "content": "replaced", "note": 1u64 } } },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c", "toolName": "ls", "input": jobj! {}, "providerOptions": jobj! { "openaiCompatible": jobj! { "id": "renamed" } } }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "renamed", "toolName": "ls", "output": jobj! { "type": "text", "value": "v" }, "providerOptions": jobj! { "openaiCompatible": jobj! { "extra": true } } }] },
    ] }, vec![text_stream()]);
}

#[test]
fn unsupported_file_parts() {
    let cases: Vec<(&str, Json)> = vec![
        ("flac", jobj! { "type": "file", "data": "AAAA", "mediaType": "audio/flac" }),
        ("zip", jobj! { "type": "file", "data": "AAAA", "mediaType": "application/zip" }),
        ("reference", jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "openai-chat": "f" } }, "mediaType": "application/pdf" }),
        ("text data", jobj! { "type": "file", "data": jobj! { "type": "text", "text": "body" }, "mediaType": "text/plain" }),
        ("font", jobj! { "type": "file", "data": "AAAA", "mediaType": "font/woff" }),
    ];
    for (name, part) in cases {
        each_family(&format!("chat file {name}"), "m", jobj! { "messages": vec![jobj! { "role": "user", "content": vec![part] }] }, vec![text_stream()]);
    }
}

#[test]
fn provider_options() {
    each_family("chat options raw", "m", jobj! { "providerOptions": jobj! {
        "openai-chat": jobj! { "user": "u-chat", "custom": 1u64, "temperature": 0.5f64 },
        "openai-compatible": jobj! { "textVerbosity": "low", "seed": 3u64, "messages": "lost", "stream": false, "max_tokens": 1u64 },
    } }, vec![text_stream()]);
    each_family("chat options camel", "m", jobj! { "providerOptions": jobj! {
        "openaiChat": jobj! { "reasoningEffort": "low", "fromCamel": true },
        "openaiCompatible": jobj! { "user": "u", "reasoningEffort": "high", "alsoCamel": jobj! { "a": Json::Null } },
    } }, vec![text_stream()]);
    each_family("chat options strict", "m", jobj! { "providerOptions": jobj! { "openaiCompatible": jobj! { "strictJsonSchema": false } } }, vec![text_stream()]);
    // The metadata key follows the name the caller used.
    each_family("chat signature under camel", "m", jobj! { "reasoning": "low" }, vec![reasoning_tool_stream()]);
    each_family("chat signature replay camel", "m", jobj! {
        "reasoning": "low",
        "messages": vec![
            jobj! { "role": "user", "content": "q" },
            jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c", "toolName": "ls", "input": jobj! {}, "providerOptions": jobj! { "openaiCompatible": jobj! { "thoughtSignature": "camel" }, "openai-chat": jobj! { "thoughtSignature": "raw" } } }] },
            jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c", "toolName": "ls", "output": jobj! { "type": "text", "value": "v" } }] },
        ],
    }, vec![text_stream()]);
    for (name, options) in [
        ("user", jobj! { "openaiCompatible": jobj! { "user": 1u64 } }),
        ("null user", jobj! { "openai-chat": jobj! { "user": Json::Null } }),
        ("effort", jobj! { "openai-compatible": jobj! { "reasoningEffort": true } }),
        ("strict", jobj! { "openaiChat": jobj! { "strictJsonSchema": "yes" } }),
        ("not an object", jobj! { "openaiCompatible": "x" }),
    ] {
        each_family(&format!("chat bad options {name}"), "m", jobj! { "providerOptions": options }, vec![text_stream()]);
    }
}

#[test]
fn tool_choice_and_declared_tools() {
    each_family("chat no tools", "m", jobj! { "tools": Vec::<Json>::new() }, vec![text_stream()]);
    each_family("chat native search has no factory", "m", jobj! { "tools": Vec::<Json>::new(), "nativeSearch": jobj! { "maxUses": 2u64 } }, vec![text_stream()]);
    each_family("chat appended tools", "m", jobj! {
        "toolAppend": true,
        "messages": vec![
            jobj! { "role": "user", "content": "q" },
            jobj! { "role": "system", "content": "", "providerOptions": jobj! { "mewrk": jobj! { "toolAddition": vec![Json::from("ls")] } } },
        ],
    }, vec![text_stream()]);
    each_family("chat system append", "m", jobj! {
        "systemAppend": true,
        "messages": vec![
            jobj! { "role": "user", "content": "q" },
            jobj! { "role": "system", "content": "plan", "providerOptions": jobj! { "mewrk": jobj! { "systemAppend": true } } },
        ],
    }, vec![text_stream()]);
}

/// The relay dialects `selfcheck.mjs` keeps in check, from the socket up.
#[test]
fn tolerant_relay_shapes() {
    // Legacy `function_call`, as a whole and split with object arguments.
    each_family("chat legacy function_call", "m", jobj! {}, vec![sse(&[
        chunk(r#"{"role":"assistant","content":null,"function_call":{"name":"read","arguments":""}}"#, None, None),
        chunk(r#"{"function_call":{"arguments":"{\"path\":\"a\"}"}}"#, None, None),
        chunk(r#"{}"#, Some("function_call"), Some(USAGE)),
    ])]);
    each_family("chat legacy object arguments", "m", jobj! {}, vec![sse(&[
        chunk(r#"{"function_call":{"name":"read","arguments":{"path":"a"}}}"#, Some("function_call"), None),
    ])]);
    // A usage-only final chunk.
    each_family("chat usage tail", "m", jobj! {}, vec![Reply::sse_raw(&format!(
        "data: {}\n\ndata: {}\n\ndata: {{\"id\":\"chatcmpl-1\",\"usage\":{USAGE}}}\n\ndata: [DONE]\n\n",
        chunk(r#"{"content":"tail"}"#, None, None),
        chunk(r#"{}"#, Some("stop"), None),
    ))]);
    // A whole body for `stream: true`, and one with no terminal reason.
    each_family("chat json body", "m", jobj! {}, vec![Reply::json(200, &format!(r#"{{"id":"j","object":"chat.completion","created":1,"model":"served","choices":[{{"index":0,"message":{{"role":"assistant","content":"json-ok","reasoning_content":"why","tool_calls":[{{"id":"c","type":"function","function":{{"name":"read","arguments":"{{}}"}}}}]}},"finish_reason":"stop"}}],"usage":{USAGE}}}"#))]);
    each_family("chat json body truncated", "m", jobj! {}, vec![Reply::json(200, r#"{"id":"j","choices":[{"index":0,"message":{"role":"assistant","content":"half"}}]}"#)]);
    // A rejected `stream_options`, retried once without it.
    each_family("chat stream_options", "m", jobj! {}, vec![Reply::json(400, r#"{"error":{"message":"json: unknown field \"stream_options\""}}"#), text_stream()]);
    each_family("chat stream_options 422", "m", jobj! {}, vec![Reply::json(422, r#"{"error":{"message":"stream_options is not allowed"}}"#), text_stream()]);
    each_family("chat other 400 kept", "m", jobj! {}, vec![Reply::json(400, r#"{"error":{"message":"nope"}}"#)]);
    // A rejected `reasoning_effort`: a named level, then no field at all.
    each_family("chat effort named", "level-refuses-max", jobj! { "reasoning": "max" }, vec![
        Reply::json(400, r#"{"error":{"type":"invalid_request_error","param":"reasoning_effort","message":"Unsupported value: 'reasoning_effort' does not support 'max' with this model. Supported values are: 'low', 'medium', 'high', and 'xhigh'."}}"#),
        text_stream(),
    ]);
    each_family("chat effort dropped", "level-refuses-field", jobj! { "reasoning": "low" }, vec![
        Reply::json(400, r#"{"error":{"message":"`reasoning_effort` is not supported with this model"}}"#),
        text_stream(),
    ]);
    // The `reasoning_content` sentinel: backfilled only with evidence from the same request.
    let sentinel = |present: bool| {
        jobj! { "messages": vec![
            jobj! { "role": "user", "content": "q" },
            jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "read", "input": jobj! {} }] },
            jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "read", "output": jobj! { "type": "text", "value": "ok" } }] },
            jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c2", "toolName": "read", "input": jobj! {} }], "providerOptions": jobj! { "openaiCompatible"?: present.then(|| jobj! { "reasoning_content": "" }) } },
            jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c2", "toolName": "read", "output": jobj! { "type": "text", "value": "ok" } }] },
        ] }
    };
    each_family("chat sentinel missing", "m", sentinel(false), vec![text_stream()]);
    each_family("chat sentinel backfilled", "m", sentinel(true), vec![text_stream()]);
    each_family("chat sentinel from reasoning", "m", jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "reasoning", "text": "r" }, jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "read", "input": jobj! {} }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "read", "output": jobj! { "type": "text", "value": "ok" } }] },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c2", "toolName": "read", "input": jobj! {} }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c2", "toolName": "read", "output": jobj! { "type": "text", "value": "ok" } }] },
    ] }, vec![text_stream()]);
    // `[DONE]` variants.
    for done in ["data:[DONE]", "data:  [DONE]", "data: [DONE] ", "data: [DONE]"] {
        each_family(&format!("chat done {done}"), "m", jobj! {}, vec![Reply::sse_raw(&format!(
            "data: {}\n\n{done}\n\n",
            chunk(r#"{"content":"wide"}"#, Some("stop"), Some(USAGE)),
        ))]);
    }
    each_family("chat crlf", "m", jobj! {}, vec![Reply::sse_raw(&format!(
        "data: {}\r\n\r\ndata: [DONE]\r\n\r\n",
        chunk(r#"{"content":"crlf"}"#, Some("stop"), None),
    ))]);
}

#[test]
fn tool_call_identities() {
    let identity = |name: &str, deltas: &[&str]| {
        let mut events: Vec<String> = deltas.iter().map(|d| chunk(&format!(r#"{{"tool_calls":[{d}]}}"#), None, None)).collect();
        events.push(chunk("{}", Some("tool_calls"), None));
        // A dialect failure discards what Node had queued but not read, so what arrived before it reaches the host
        // only when the chunks are apart: the same race on the real network, taken out of the comparison here.
        each_family(&format!("chat identity {name}"), "m", jobj! {}, vec![sse(&events).pause(Duration::from_millis(40))]);
    };
    identity("missing id", &[r#"{"index":0,"function":{"name":"read","arguments":"{\"a\":1}"}}"#]);
    identity("missing index", &[r#"{"id":"a","function":{"name":"read","arguments":"{\"a\":1}"}}"#]);
    identity("missing both", &[r#"{"function":{"name":"read","arguments":""}}"#, r#"{"function":{"arguments":"{\"a\":1}"}}"#]);
    identity("late id", &[r#"{"index":0,"function":{"name":"read","arguments":""}}"#, r#"{"index":0,"id":"late","function":{"arguments":"{\"a\":1}"}}"#]);
    identity("late name", &[r#"{"index":0,"id":"a","function":{"arguments":"{\"a\":"}}"#, r#"{"index":0,"function":{"name":"read","arguments":"1}"}}"#]);
    identity("late name then id", &[r#"{"index":0,"function":{"arguments":"{\"a\":"}}"#, r#"{"index":0,"function":{"name":"read"}}"#, r#"{"index":0,"function":{"arguments":"1}"}}"#]);
    identity("repeated name", &[r#"{"index":0,"id":"a","function":{"name":"read","arguments":""}}"#, r#"{"index":0,"id":"a","function":{"name":"read","arguments":"{}"}}"#]);
    identity("collision", &[r#"{"index":0,"id":"a","function":{"name":"read"}}"#, r#"{"index":0,"id":"b","function":{"name":"ls"}}"#]);
    identity("rebind", &[r#"{"index":0,"id":"a","function":{"name":"read"}}"#, r#"{"index":1,"id":"a","function":{"name":"read"}}"#]);
    identity("orphan continuation", &[r#"{"function":{"arguments":"{}"}}"#]);
    identity("two with no index", &[r#"{"id":"a","function":{"name":"read","arguments":"{}"}}"#, r#"{"id":"b","function":{"name":"ls","arguments":"{}"}}"#, r#"{"function":{"arguments":"x"}}"#]);
    identity("never named", &[r#"{"index":0,"id":"a","function":{"arguments":"{}"}}"#]);
    identity("empty id", &[r#"{"index":0,"id":"","function":{"name":"read","arguments":"{}"}}"#]);
    identity("empty arguments delta", &[r#"{"index":0,"id":"a","function":{"name":"read","arguments":"{}"}}"#, r#"{"index":0,"id":"a","function":{"arguments":""}}"#]);
    identity("first extra content wins", &[
        r#"{"index":0,"function":{"arguments":""},"extra_content":{"google":{}}}"#,
        r#"{"index":0,"id":"a","function":{"name":"read","arguments":"{}"},"extra_content":{"google":{"thought_signature":"late"}}}"#,
    ]);
    identity("name-only signature", &[r#"{"index":0,"id":"a","function":{"name":"read","arguments":"{}"},"extra_content":{"google":{"thought_signature":"S"}}}"#, r#"{"index":0,"id":"a","function":{"arguments":""},"extra_content":{"google":{"thought_signature":"ignored"}}}"#]);
    // A call that never gets a name, with reasoning and text open when the stream ends.
    each_family("chat flush throws", "m", jobj! {}, vec![sse(&[
        chunk(r#"{"content":"t"}"#, None, None),
        chunk(r#"{"tool_calls":[{"index":0,"id":"a","function":{"arguments":"{}"}}]}"#, Some("tool_calls"), None),
    ])]);
    // Malformed arguments: the host hears of it as an invalid call.
    for family in ["openai-chat", "openai-compatible"] {
        differential_normalized("chat bad arguments", Some("/v1"), base(family, "m", jobj! {}), vec![sse(&[
            chunk(r#"{"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"{not json"}}]}"#, Some("tool_calls"), None),
        ])], &without_syntax_error_text);
    }
    each_family("chat unknown tool", "m", jobj! {}, vec![sse(&[
        chunk(r#"{"tool_calls":[{"index":0,"id":"a","function":{"name":"nope","arguments":"{}"}}]}"#, Some("tool_calls"), None),
    ])]);
}

#[test]
fn usage_shapes() {
    for (name, usage) in [
        ("legacy hit", r#"{"prompt_tokens":100,"completion_tokens":10,"prompt_cache_hit_tokens":80,"completion_tokens_details":{"reasoning_tokens":3}}"#),
        ("legacy pair", r#"{"prompt_tokens":100,"completion_tokens":10,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20}"#),
        ("mismatch", r#"{"prompt_tokens":100,"completion_tokens":10,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":30}"#),
        ("standard wins", r#"{"prompt_tokens":100,"completion_tokens":10,"prompt_cache_hit_tokens":80,"prompt_tokens_details":{"cached_tokens":30}}"#),
        ("null details", r#"{"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":null,"completion_tokens_details":null}"#),
        ("empty", r#"{}"#),
        ("null counts", r#"{"prompt_tokens":null,"completion_tokens":null}"#),
    ] {
        each_family(&format!("chat usage {name}"), "m", jobj! {}, vec![sse(&[chunk(r#"{"content":"u"}"#, Some("stop"), Some(usage))])]);
    }
    each_family("chat usage never sent", "m", jobj! {}, vec![sse(&[chunk(r#"{"content":"u"}"#, Some("stop"), None)])]);
    each_family("chat usage latest wins", "m", jobj! {}, vec![sse(&[
        chunk(r#"{"content":"u"}"#, None, Some(r#"{"prompt_tokens":1,"completion_tokens":1}"#)),
        chunk(r#"{}"#, Some("stop"), Some(r#"{"prompt_tokens":2,"completion_tokens":2}"#)),
    ])]);
}

#[test]
fn errors_and_classification() {
    // Every status, with and without the error schema.
    each_family("chat 429", "m", jobj! {}, vec![Reply::json(429, r#"{"error":{"message":"slow down","type":"rate_limit","code":"rate_limit_exceeded"}}"#).header("retry-after", "7")]);
    each_family("chat 401", "m", jobj! {}, vec![Reply::json(401, r#"{"error":{"message":"bad key","code":401}}"#)]);
    each_family("chat 500 empty", "m", jobj! {}, vec![Reply::new(500)]);
    each_family("chat 503 html", "m", jobj! {}, vec![Reply::new(503).header("content-type", "text/html").body("<html>")]);
    each_family("chat 400 other shape", "m", jobj! {}, vec![Reply::json(400, r#"{"message":"flat"}"#)]);
    each_family("chat 400 bad error type", "m", jobj! {}, vec![Reply::json(400, r#"{"error":{"message":"m","type":1}}"#)]);
    each_family("chat 404 string error", "m", jobj! {}, vec![Reply::json(404, r#"{"error":"not here"}"#)]);
    // Empty `baseURL`: `new URL` throws before any request goes out.
    for family in ["openai-chat", "openai-compatible"] {
        differential("chat no base url", None, base(family, "m", jobj! {}), vec![text_stream()]);
        differential("chat empty base url", None, base(family, "m", jobj! { "baseURL": "" }), vec![text_stream()]);
        differential("chat relative base url", None, base(family, "m", jobj! { "baseURL": "relay/v1" }), vec![text_stream()]);
    }
    // Errors inside the stream.
    each_family("chat first chunk error", "m", jobj! {}, vec![Reply::sse(&[r#"{"error":{"message":"Overloaded","type":"overloaded_error"}}"#])]);
    each_family("chat midstream error", "m", jobj! {}, vec![sse(&[
        chunk(r#"{"content":"partial"}"#, None, None),
        r#"{"error":{"message":"boom","type":"invalid_request_error","code":"bad"}}"#.to_owned(),
    ])]);
    each_family("chat retryable code", "m", jobj! {}, vec![Reply::sse(&[r#"{"error":{"message":"m","code":"rate_limit_exceeded"}}"#])]);
    each_family("chat numeric code", "m", jobj! {}, vec![Reply::sse(&[r#"{"error":{"message":"m","code":503}}"#])]);
    each_family("chat status is stripped", "m", jobj! {}, vec![Reply::sse(&[r#"{"error":{"message":"m","status":503,"isRetryable":true}}"#])]);
    each_family("chat loose error keeps status", "m", jobj! {}, vec![Reply::sse(&[r#"{"choices":[],"error":{"message":"m","status":503}}"#])]);
    each_family("chat loose string error", "m", jobj! {}, vec![Reply::sse(&[r#"{"choices":[],"error":"plain"}"#])]);
    each_family("chat loose null error", "m", jobj! {}, vec![Reply::sse(&[r#"{"choices":[],"error":null}"#])]);
    each_family("chat loose object error", "m", jobj! {}, vec![Reply::sse(&[r#"{"choices":[],"error":{"code":"x"}}"#])]);
    each_family("chat wrapped error", "m", jobj! {}, vec![Reply::sse(&[r#"{"choices":[],"error":{"error":{"message":"inner","code":"overloaded"}}}"#])]);
    each_family("chat no finish reason", "m", jobj! {}, vec![sse(&[chunk(r#"{"content":"cut"}"#, None, None)])]);
    each_family("chat empty stream", "m", jobj! {}, vec![Reply::sse_raw("")]);
    each_family("chat truncated stream", "m", jobj! {}, vec![Reply::sse_raw(&format!("data: {}\n\n", chunk(r#"{"content":"cut"}"#, None, None))).truncated()]);
    // Chunks the schema refuses.
    for (name, event) in [
        ("role", chunk(r#"{"role":"tool"}"#, None, None)),
        ("content array", chunk(r#"{"content":[{"type":"text","text":"x"}]}"#, None, None)),
        ("no function", chunk(r#"{"tool_calls":[{"index":0,"id":"a"}]}"#, None, None)),
        ("usage string", chunk(r#"{}"#, Some("stop"), Some(r#"{"prompt_tokens":"1"}"#))),
        ("infinite count", chunk(r#"{}"#, Some("stop"), Some(r#"{"prompt_tokens":1e400}"#))),
        ("no choices", r#"{"id":"x"}"#.to_owned()),
        ("scalar", "5".to_owned()),
    ] {
        for family in ["openai-chat", "openai-compatible"] {
            differential_normalized(&format!("chat invalid {name}"), Some("/v1"), base(family, "m", jobj! {}), vec![sse(std::slice::from_ref(&event))], &without_zod_report);
        }
    }
    for family in ["openai-chat", "openai-compatible"] {
        differential_normalized("chat bad json", Some("/v1"), base(family, "m", jobj! {}), vec![Reply::sse_raw("data: {not json\n\n")], &without_zod_report);
    }
}

/// `selfcheck.mjs`'s table of usage evidence, through each of the three ways a usage block can arrive.
#[test]
fn selfcheck_usage_evidence() {
    let shapes: [(&str, &str); 14] = [
        ("hit", r#""prompt_cache_hit_tokens":80"#),
        ("pair", r#""prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20"#),
        ("standard", r#""prompt_tokens_details":{"cached_tokens":30},"prompt_cache_hit_tokens":80"#),
        ("zero", r#""prompt_tokens_details":{"cached_tokens":0},"prompt_cache_hit_tokens":80"#),
        ("null", r#""prompt_tokens_details":{"cached_tokens":null},"prompt_cache_hit_tokens":80"#),
        ("negative", r#""prompt_cache_hit_tokens":-1"#),
        ("fraction", r#""prompt_cache_hit_tokens":1.5"#),
        ("excess", r#""prompt_cache_hit_tokens":101"#),
        ("mismatch", r#""prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":30"#),
        ("badmiss", r#""prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":-1"#),
        ("string", r#""prompt_cache_hit_tokens":"80""#),
        ("infinite", r#""prompt_cache_hit_tokens":1e400"#),
        ("standardonly", r#""prompt_tokens_details":{"cached_tokens":30}"#),
        ("none", r#""x":0"#),
    ];
    for (name, extra) in shapes {
        let usage = format!(r#"{{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110,"completion_tokens_details":{{"reasoning_tokens":3}},{extra}}}"#);
        let sse_path = sse(&[chunk(r#"{"content":"cache-ok"}"#, None, None), chunk("{}", Some("stop"), Some(&usage))]);
        let tail_path = Reply::sse_raw(&format!(
            "data: {}\n\ndata: {}\n\ndata: {{\"usage\":{usage}}}\n\ndata: [DONE]\n\n",
            chunk(r#"{"content":"cache-ok"}"#, None, None),
            chunk("{}", Some("stop"), None),
        ));
        let json_path = Reply::json(200, &format!(r#"{{"id":"cache","object":"chat.completion","created":1,"model":"m","choices":[{{"index":0,"message":{{"role":"assistant","content":"cache-ok"}},"finish_reason":"stop"}}],"usage":{usage}}}"#));
        for (path, reply) in [("sse", sse_path), ("tail", tail_path), ("json", json_path)] {
            differential(&format!("chat cache {path} {name}"), Some("/v1"), base("openai-compatible", "m", jobj! {}), vec![reply]);
        }
    }
}

/// `selfcheck.mjs`'s table of tool-call identities.
#[test]
fn selfcheck_identities() {
    let a = r#"{"id":"A","function":{"name":"alpha","arguments":"{\"a\":"}}"#;
    let a1 = r#"{"id":"A","function":{"arguments":"1}"}}"#;
    let b2 = r#"{"id":"B","function":{"arguments":"2}"}}"#;
    let a_fn = r#""function":{"name":"alpha","arguments":"{\"a\":"}"#;
    let b_fn = r#""function":{"name":"beta","arguments":"{\"b\":"}"#;
    let cases: Vec<(&str, Vec<String>)> = vec![
        ("collision", vec![a.into(), format!(r#"{{"index":0,"id":"B",{b_fn}}}"#), a1.into(), b2.into()]),
        ("confirm", vec![a.into(), r#"{"index":0,"id":"A","function":{"arguments":"1}"}}"#.into()]),
        ("separate", vec![a.into(), format!(r#"{{"index":1,"id":"B",{b_fn}}}"#), a1.into(), b2.into()]),
        ("explicit-first", vec![format!(r#"{{"index":0,"id":"A",{a_fn}}}"#), format!(r#"{{"id":"B",{b_fn}}}"#), a1.into(), b2.into()]),
        ("late-id", vec![format!(r#"{{"index":0,{a_fn}}}"#), r#"{"index":0,"id":"A","function":{"arguments":"1}"}}"#.into()]),
        ("missing-id", vec![format!(r#"{{"index":0,{a_fn}}}"#), r#"{"index":0,"function":{"arguments":"1}"}}"#.into()]),
        ("late-two", vec![format!(r#"{{"index":0,{a_fn}}}"#), format!(r#"{{"index":1,{b_fn}}}"#), r#"{"index":0,"id":"A","function":{"arguments":"1}"}}"#.into(), r#"{"index":1,"id":"B","function":{"arguments":"2}"}}"#.into()]),
        ("repeated-name", vec![format!(r#"{{"index":0,{a_fn}}}"#), r#"{"index":0,"id":"A","function":{"name":"alpha","arguments":"1}"}}"#.into()]),
        ("arguments-first", vec![r#"{"index":0,"function":{"arguments":"{\"a\":"}}"#.into(), r#"{"index":0,"function":{"name":"alpha","arguments":""}}"#.into(), r#"{"index":0,"id":"A","function":{"arguments":"1}"}}"#.into()]),
        ("anonymous-single", vec![format!(r#"{{{a_fn}}}"#), r#"{"function":{"arguments":"1}"}}"#.into()]),
        ("anonymous-orphan", vec![r#"{"function":{"arguments":"{}"}}"#.into()]),
        ("anonymous-many", vec![r#"{"function":{"name":"alpha","arguments":"{\"a\":1}"}}"#.into(), r#"{"function":{"name":"beta","arguments":"{\"b\":2}"}}"#.into(), r#"{"function":{"arguments":" "}}"#.into()]),
        ("anonymous-interleaved", vec![format!(r#"{{{a_fn}}}"#), format!(r#"{{{b_fn}}}"#), r#"{"function":{"arguments":"1}"}}"#.into(), r#"{"function":{"arguments":"2}"}}"#.into()]),
        ("rebind", vec![a.into(), r#"{"index":1,"id":"A","function":{"arguments":"1}"}}"#.into()]),
        ("no-index", vec![r#"{"id":"call_a","type":"function","function":{"name":"alpha","arguments":"{\"a\":"}}"#.into(), r#"{"id":"call_b","type":"function","function":{"name":"beta","arguments":"{\"b\":"}}"#.into(), r#"{"id":"call_a","function":{"arguments":"1}"}}"#.into(), r#"{"id":"call_b","function":{"arguments":"2}"}}"#.into()]),
    ];
    let declared: Vec<Json> = ["alpha", "beta"].iter().map(|name| jobj! { "name": *name, "description": *name, "inputSchema": jobj! { "type": "object", "additionalProperties": true } }).collect();
    for (name, deltas) in cases {
        let mut events: Vec<String> = deltas.iter().map(|d| chunk(&format!(r#"{{"tool_calls":[{d}]}}"#), None, None)).collect();
        events.push(chunk("{}", Some("tool_calls"), None));
        differential(&format!("chat selfcheck identity {name}"), Some("/v1"), base("openai-chat", "m", jobj! { "tools": declared.clone() }), vec![sse(&events).pause(Duration::from_millis(150))]);
    }
}

#[test]
fn prompt_forms_the_core_hands_over() {
    each_family("chat string assistant", "m", jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": "plain" },
        jobj! { "role": "user", "content": vec![jobj! { "type": "image", "image": PNG, "mediaType": "image/png" }] },
    ] }, vec![text_stream()]);
    each_family("chat foreign reasoning marks", "m", jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "t", "providerOptions": jobj! { "mewrk": jobj! { "model": "other" }, "anthropic": jobj! { "signature": "S" } } },
            jobj! { "type": "text", "text": "a" },
        ] },
        jobj! { "role": "user", "content": "again" },
    ] }, vec![text_stream()]);
    // A URL the model does not take is downloaded first, and a loopback one is refused.
    each_family("chat url refused", "m", jobj! { "messages": vec![jobj! { "role": "user", "content": vec![
        jobj! { "type": "file", "data": jobj! { "type": "url", "url": "http://127.0.0.1:9/x.png" }, "mediaType": "image/png" },
    ] }] }, vec![text_stream()]);
    each_family("chat data url", "m", jobj! { "messages": vec![jobj! { "role": "user", "content": vec![
        jobj! { "type": "file", "data": format!("data:image/png;base64,{PNG}"), "mediaType": "image/png" },
    ] }] }, vec![text_stream()]);
}
