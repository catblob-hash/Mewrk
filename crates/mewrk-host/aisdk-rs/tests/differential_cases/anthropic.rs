use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;

use super::differential;
use crate::common::Reply;

fn base(model: &str, extra: Json) -> Json {
    let mut payload = jobj! {
        "family": "anthropic",
        "apiKey": "k",
        "modelId": model,
        "system": "S",
        "systemDynamic": "tail",
        "messages": vec![jobj! { "role": "user", "content": "hello" }],
        "tools": vec![jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "path": jobj! { "type": "string" } } } }],
        "maxSteps": 1u64,
    };
    if let (Some(object), Some(extra)) = (payload.as_obj_mut(), extra.as_obj()) {
        object.extend_from(extra);
    }
    payload
}

fn thinking_tool_stream() -> Reply {
    Reply::sse(&[
        r#"{"type":"message_start","message":{"id":"m1","model":"claude-opus-5-20261101","usage":{"input_tokens":3,"cache_read_input_tokens":2,"cache_creation_input_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me "}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"RED"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"hi"}}"#,
        r#"{"type":"content_block_stop","index":2}"#,
        r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"c1","name":"read","input":{}}}"#,
        r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}"#,
        r#"{"type":"content_block_stop","index":3}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9,"output_tokens_details":{"thinking_tokens":5}}}"#,
        r#"{"type":"message_stop"}"#,
    ])
}

fn replay_history() -> Vec<Json> {
    vec![
        jobj! { "role": "user", "content": vec![
            jobj! { "type": "text", "text": "look at this" },
            jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png" },
        ] },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "t", "providerOptions": jobj! { "anthropic": jobj! { "signature": "S" }, "mewrk": jobj! { "model": "claude-opus-5[1m]" } } },
            jobj! { "type": "reasoning", "text": "other", "providerOptions": jobj! { "anthropic": jobj! { "signature": "S2" }, "mewrk": jobj! { "model": "other" } } },
            jobj! { "type": "text", "text": "a  " },
            jobj! { "type": "tool-call", "toolCallId": "c0", "toolName": "read", "input": jobj! { "path": "x" } },
        ] },
        jobj! { "role": "tool", "content": vec![jobj! {
            "type": "tool-result", "toolCallId": "c0", "toolName": "read",
            "output": jobj! { "type": "content", "value": vec![
                jobj! { "type": "text", "text": "body" },
                jobj! { "type": "file", "data": jobj! { "type": "data", "data": "iVBORw0KGgo=" }, "mediaType": "image/png" },
            ] },
        }] },
        jobj! { "role": "user", "content": "again" },
    ]
}

#[test]
fn stream_reasoning_tools_usage() {
    differential("anthropic stream", Some("/v1"), base("claude-opus-5", jobj! { "reasoning": "high", "reasoningContent": "plaintext", "promptCache": true }), vec![thinking_tool_stream()]);
}

#[test]
fn encrypted_form_is_eager() {
    differential("anthropic encrypted", Some("/v1"), base("claude-opus-5", jobj! { "reasoning": "max", "reasoningContent": "encrypted", "promptCache": false }), vec![thinking_tool_stream()]);
}

#[test]
fn replay_with_files_and_foreign_signatures() {
    differential(
        "anthropic replay",
        Some("/v1/"),
        base("claude-opus-5", jobj! { "messages": replay_history(), "promptCache": true, "reasoning": "low" }),
        vec![thinking_tool_stream()],
    );
}

#[test]
fn budget_model_and_defaults() {
    differential("anthropic budget", Some("/v1"), base("claude-sonnet-4-5", jobj! { "reasoning": "medium", "maxOutputTokens": 8000u64 }), vec![thinking_tool_stream()]);
    differential("anthropic haiku 3", Some("/v1"), base("claude-3-haiku-20240307", jobj! {}), vec![thinking_tool_stream()]);
    differential("anthropic non-claude", Some("/v1"), base("glm-5", jobj! { "reasoning": "high", "maxOutputTokens": 1000u64, "headers": jobj! { "x-extra": "1", "anthropic-beta": "Custom-Beta" } }), vec![thinking_tool_stream()]);
}

#[test]
fn native_search_with_pause_and_continuation() {
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
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_1","content":[{"type":"web_search_result","url":"https://a.example/x","title":"A","encrypted_content":"E","page_age":"2d"}]}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"found"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    differential("anthropic search one step", Some("/v1"), base("claude-opus-5", jobj! { "tools": Vec::<Json>::new(), "nativeSearch": jobj! { "maxUses": 3u64, "toolType": "web_search_20260209" } }), vec![paused.clone()]);
    differential("anthropic search loop", Some("/v1"), base("claude-opus-5", jobj! { "tools": Vec::<Json>::new(), "maxSteps": 4u64, "nativeSearch": jobj! { "maxUses": 3u64 } }), vec![paused, finished]);
}

#[test]
fn errors_and_self_heal() {
    differential("anthropic 429", Some("/v1"), base("m", jobj! {}), vec![Reply::json(429, r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#).header("retry-after", "7")]);
    differential("anthropic empty 503", Some("/v1"), base("m", jobj! {}), vec![Reply::new(503)]);
    differential("anthropic first chunk error", Some("/v1"), base("m", jobj! {}), vec![Reply::sse(&[r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#])]);
    differential("anthropic midstream error", Some("/v1"), base("m", jobj! {}), vec![Reply::sse(&[
        r#"{"type":"message_start","message":{"id":"m1","model":"m","usage":{"input_tokens":3}}}"#,
        r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#,
    ])]);
    differential(
        "anthropic effort heal",
        Some("/v1"),
        base("diff-heal-effort", jobj! { "reasoning": "high", "maxOutputTokens": 100000u64 }),
        vec![
            Reply::json(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 100000 > 8192, which is the maximum allowed"}}"#),
            thinking_tool_stream(),
        ],
    );
}

#[test]
fn appended_tools_and_system_messages() {
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "system", "content": "", "providerOptions": jobj! { "mewrk": jobj! { "toolAddition": vec![Json::from("read"), Json::from("gone")] } } },
        jobj! { "role": "system", "content": "plan mode", "providerOptions": jobj! { "mewrk": jobj! { "systemAppend": true } } },
    ];
    differential("anthropic append on", Some("/v1"), base("claude-opus-5", jobj! { "messages": history.clone(), "toolAppend": true, "systemAppend": true, "promptCache": true }), vec![thinking_tool_stream()]);
    differential("anthropic append off", Some("/v1"), base("claude-opus-5", jobj! { "messages": history, "promptCache": true }), vec![thinking_tool_stream()]);
}

#[test]
fn compressed_bodies_and_a_body_with_no_content_length() {
    use std::io::Write;
    let plain = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"m\",\"usage\":{\"input_tokens\":1}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"zipped\"}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
    let mut gzipped = Vec::new();
    let mut encoder = flate2::write::GzEncoder::new(&mut gzipped, flate2::Compression::default());
    encoder.write_all(plain.as_bytes()).unwrap();
    encoder.finish().unwrap();
    differential("anthropic gzip", Some("/v1"), base("m", jobj! {}), vec![Reply::new(200).header("content-type", "text/event-stream").header("content-encoding", "gzip").body(gzipped)]);
    let mut raw = Reply::new(200).header("content-type", "text/event-stream").body(plain);
    raw.raw = true;
    differential("anthropic close-delimited", Some("/v1"), base("m", jobj! {}), vec![raw]);
}
