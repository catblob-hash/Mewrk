use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use mewrk_aisdk::http::{Cancel, Headers, NetFetch};
use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;
use mewrk_aisdk::model::CallOptions;
use mewrk_aisdk::providers::ProviderSettings;
use mewrk_aisdk::providers::bedrock::create_signed;
use mewrk_aisdk::providers::bedrock::eventstream::{HeaderValue, encode_frame};
use mewrk_aisdk::providers::bedrock::sigv4::Credentials;

use super::{differential, differential_normalized};
use crate::common::{Mock, Reply, comparable_request, node_sidecar_path, step_frame};

const OPUS: &str = "us.anthropic.claude-opus-5";
const SONNET: &str = "anthropic.claude-sonnet-4-5-20250929-v1:0";

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

fn base(model: &str, extra: Json) -> Json {
    let mut payload = jobj! {
        "family": "bedrock",
        "apiKey": "bedrock-test-key-1",
        "settings": jobj! { "region": "us-east-1" },
        "modelId": model,
        "system": "S",
        "messages": vec![jobj! { "role": "user", "content": "hello" }],
        "tools": vec![jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "path": jobj! { "type": "string" } } } }],
        "maxSteps": 1u64,
    };
    if let (Some(object), Some(extra)) = (payload.as_obj_mut(), extra.as_obj()) {
        object.extend_from(extra);
    }
    payload
}

fn stop_and_usage(reason: &str) -> Vec<Vec<u8>> {
    vec![
        event("messageStop", &format!(r#"{{"stopReason":"{reason}","p":"padding"}}"#)),
        event("metadata", r#"{"usage":{"inputTokens":3,"outputTokens":9,"totalTokens":12},"metrics":{"latencyMs":5}}"#),
    ]
}

fn text_stream() -> Reply {
    let mut chunks = vec![
        event("messageStart", r#"{"role":"assistant","p":"abc"}"#),
        event("contentBlockStart", r#"{"contentBlockIndex":0,"start":{}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"hi"},"p":"xyz"}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":" there"}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
    ];
    chunks.extend(stop_and_usage("end_turn"));
    frames(chunks)
}

fn thinking_tool_stream() -> Reply {
    let mut chunks = vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"Let me "}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"think"}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"SIG"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"data":"RED"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":1}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":2,"delta":{"text":"hi"}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":2}"#),
        event("contentBlockStart", r#"{"contentBlockIndex":3,"start":{"toolUse":{"toolUseId":"tooluse_c-1","name":"read"}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":3,"delta":{"toolUse":{"input":"{\"path\":"}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":3,"delta":{"toolUse":{"input":"\"a\"}"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":3}"#),
        event("contentBlockStart", r#"{"contentBlockIndex":4,"start":{"toolUse":{"toolUseId":"tooluse_c-2","name":"read"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":4}"#),
    ];
    chunks.extend(stop_and_usage("tool_use"));
    frames(chunks)
}

#[test]
fn plain_text_through_one_chunk_and_many() {
    differential("bedrock text", Some(""), base(SONNET, jobj! {}), vec![text_stream()]);
    // Everything in one socket write, then split inside frames.
    let stream = text_stream();
    let joined: Vec<u8> = stream.chunks.concat();
    differential("bedrock one write", Some("/"), base(SONNET, jobj! { "tools": Vec::<Json>::new() }), vec![frames(vec![joined.clone()])]);
    let split: Vec<Vec<u8>> = joined.chunks(7).map(<[u8]>::to_vec).collect();
    differential("bedrock small writes", Some("/bedrock/"), base("amazon.titan-text-express-v1", jobj! {}), vec![frames(split)]);
}

#[test]
fn the_request_id_header_is_optional_and_the_model_id_is_encoded() {
    let mut no_id = Reply::new(200).header("content-type", "application/vnd.amazon.eventstream");
    no_id.chunks = text_stream().chunks;
    differential("bedrock no request id", Some(""), base("arn:aws:bedrock:us-east-1:123:inference-profile/us.anthropic.claude-3-7-sonnet", jobj! {}), vec![no_id]);
}

#[test]
fn signed_reasoning_and_redacted_thinking_on_claude() {
    for form in ["plaintext", "encrypted"] {
        differential(
            &format!("bedrock reasoning {form}"),
            Some(""),
            base(OPUS, jobj! { "reasoning": "high", "reasoningContent": form, "promptCache": true, "systemDynamic": "tail" }),
            vec![thinking_tool_stream()],
        );
    }
    differential("bedrock no consumer", Some(""), base(SONNET, jobj! {}), vec![thinking_tool_stream()]);
    // Signature-only and redacted-only items: no text delta at all.
    let opaque = frames(vec![
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"ONLY"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"data":"RED1"}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"data":"RED2"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":1}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":2,"delta":{"reasoningContent":{"redactedContent":"AB"}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":2,"delta":{"reasoningContent":{"redactedContent":"CD"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":2}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":3,"delta":{"text":"done"}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":3}"#),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
    ]);
    for form in ["plaintext", "encrypted"] {
        differential(&format!("bedrock opaque {form}"), Some(""), base(OPUS, jobj! { "reasoningContent": form }), vec![opaque.clone()]);
    }
    // Empty deltas of every kind open nothing; a reasoning delta on a text block shares its id.
    differential(
        "bedrock empty deltas",
        Some(""),
        base(OPUS, jobj! {}),
        vec![frames(vec![
            event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":""}}"#),
            event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":""}}}"#),
            event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":""}}}"#),
            event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":null}"#),
            event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            event("contentBlockDelta", r#"{"contentBlockIndex":1,"delta":{"text":"x"}}"#),
            event("contentBlockDelta", r#"{"contentBlockIndex":1,"delta":{"reasoningContent":{"text":"y"}}}"#),
            event("contentBlockStop", r#"{"contentBlockIndex":1}"#),
            event("messageStop", r#"{"stopReason":"end_turn"}"#),
        ])],
    );
}

fn replay_history() -> Vec<Json> {
    vec![
        jobj! { "role": "user", "content": vec![
            jobj! { "type": "text", "text": "look at this", "providerOptions": jobj! { "bedrock": jobj! { "cachePoint": jobj! { "type": "default" } } } },
            jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png" },
            jobj! { "type": "file", "data": "JVBERi0xLjQ=", "mediaType": "application/pdf", "filename": "report.final.pdf", "providerOptions": jobj! { "amazonBedrock": jobj! { "citations": jobj! { "enabled": true } } } },
            jobj! { "type": "file", "data": "aGVsbG8=", "mediaType": "text/plain" },
            jobj! { "type": "file", "data": "AAAAIGZ0eXBpc29t", "mediaType": "video/mp4" },
        ], "providerOptions": jobj! { "bedrock": jobj! { "cachePoint": jobj! { "type": "default", "ttl": "1h" } } } },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": " keep my spaces ", "providerOptions": jobj! { "amazonBedrock": jobj! { "signature": "S" }, "bedrock": jobj! { "signature": "S" }, "mewrk": jobj! { "model": OPUS } } },
            jobj! { "type": "reasoning", "text": "", "providerOptions": jobj! { "amazonBedrock": jobj! { "redactedData": "RD" }, "bedrock": jobj! { "redactedData": "RD" }, "mewrk": jobj! { "model": OPUS } } },
            jobj! { "type": "reasoning", "text": "", "providerOptions": jobj! { "bedrock": jobj! { "redactedContent": "RC" } } },
            jobj! { "type": "reasoning", "text": "other", "providerOptions": jobj! { "bedrock": jobj! { "signature": "S2" }, "mewrk": jobj! { "model": "other" } } },
            jobj! { "type": "reasoning", "text": "unsigned" },
            jobj! { "type": "text", "text": "  " },
            jobj! { "type": "text", "text": "a  " },
            jobj! { "type": "tool-call", "toolCallId": "tooluse_c-0", "toolName": "read", "input": jobj! { "path": "x" } },
            jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "re.ad!", "input": jobj! { "path": "y" }, "providerOptions": jobj! { "bedrock": jobj! { "cachePoint": jobj! { "type": "default" } } } },
        ] },
        jobj! { "role": "tool", "content": vec![
            jobj! {
                "type": "tool-result", "toolCallId": "tooluse_c-0", "toolName": "read",
                "output": jobj! { "type": "content", "value": vec![
                    jobj! { "type": "text", "text": "body" },
                    jobj! { "type": "file", "data": jobj! { "type": "data", "data": "iVBORw0KGgo=" }, "mediaType": "image/png" },
                    jobj! { "type": "file", "data": jobj! { "type": "data", "data": "JVBERi0xLjQ=" }, "mediaType": "application/pdf" },
                    jobj! { "type": "file", "data": jobj! { "type": "data", "data": "AAAAIGZ0eXBpc29t" }, "mediaType": "video/mp4" },
                ] },
            },
            jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "re.ad!", "output": jobj! { "type": "json", "value": jobj! { "z": 1, "a": jarr_of(&["x"]) } }, "providerOptions": jobj! { "bedrock": jobj! { "cachePoint": jobj! { "type": "default" } } } },
        ] },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "tool-call", "toolCallId": "c2", "toolName": "read", "input": jobj! {} },
            jobj! { "type": "tool-call", "toolCallId": "c3", "toolName": "read", "input": jobj! {} },
            jobj! { "type": "tool-call", "toolCallId": "c4", "toolName": "read", "input": jobj! {} },
            jobj! { "type": "tool-call", "toolCallId": "c5", "toolName": "read", "input": jobj! {} },
        ] },
        jobj! { "role": "tool", "content": vec![
            jobj! { "type": "tool-result", "toolCallId": "c2", "toolName": "read", "output": jobj! { "type": "text", "value": "t" } },
            jobj! { "type": "tool-result", "toolCallId": "c3", "toolName": "read", "output": jobj! { "type": "error-text", "value": "bad" } },
            jobj! { "type": "tool-result", "toolCallId": "c4", "toolName": "read", "output": jobj! { "type": "error-json", "value": jobj! { "e": 1 } } },
            jobj! { "type": "tool-result", "toolCallId": "c5", "toolName": "read", "output": jobj! { "type": "execution-denied" } },
        ] },
        jobj! { "role": "user", "content": "again" },
        jobj! { "role": "assistant", "content": "prefill  " },
    ]
}

fn jarr_of(items: &[&str]) -> Vec<Json> {
    items.iter().map(|item| Json::from(*item)).collect()
}

#[test]
fn replay_with_files_tools_signatures_and_cache_points() {
    differential("bedrock replay", Some(""), base(OPUS, jobj! { "messages": replay_history(), "reasoning": "low", "promptCache": true }), vec![text_stream()]);
    // Another model's signature goes; so does the prefill trimming stay on the last part only.
    differential("bedrock replay foreign", Some(""), base(SONNET, jobj! { "messages": replay_history() }), vec![text_stream()]);
    // A non-Claude model replays the same parts by the same rules.
    differential("bedrock replay nova", Some(""), base("us.amazon.nova-pro-v1:0", jobj! { "messages": replay_history() }), vec![text_stream()]);
}

#[test]
fn tool_results_of_every_kind() {
    let run = |name: &str, output: Json| {
        let history = vec![
            jobj! { "role": "user", "content": "q" },
            jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c", "toolName": "read", "input": jobj! {} }] },
            jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c", "toolName": "read", "output": output }] },
        ];
        differential(&format!("bedrock result {name}"), Some(""), base(SONNET, jobj! { "messages": history }), vec![text_stream()]);
    };
    let file = |data: Json, media: &str| jobj! { "type": "content", "value": vec![jobj! { "type": "file", "data": data, "mediaType": media }] };
    run("reference", file(jobj! { "type": "reference", "reference": jobj! { "bedrock": "id" } }, "image/png"));
    run("sniffed image", file(jobj! { "type": "data", "data": "iVBORw0KGgo=" }, "image/*"));
    run("unsniffable image", file(jobj! { "type": "data", "data": "aGVsbG8=" }, "image/*"));
    run("custom part", jobj! { "type": "content", "value": vec![jobj! { "type": "custom", "providerOptions": jobj! {} }] });
    run("named document", jobj! { "type": "content", "value": vec![
        jobj! { "type": "file", "data": jobj! { "type": "data", "data": "JVBERi0xLjQ=" }, "mediaType": "application/pdf", "filename": "r.pdf", "providerOptions": jobj! { "bedrock": jobj! { "citations": jobj! { "enabled": true } } } },
        jobj! { "type": "file", "data": jobj! { "type": "data", "data": "JVBERi0xLjQ=" }, "mediaType": "application/pdf" },
    ] });
    run("denied with a reason", jobj! { "type": "execution-denied", "reason": "no" });
    run("json null", jobj! { "type": "json", "value": Json::Null });
}

#[test]
fn assistant_parts_bedrock_has_no_block_for() {
    let history = |parts: Vec<Json>| vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": parts },
        jobj! { "role": "user", "content": "again" },
    ];
    let run = |name: &str, parts: Vec<Json>| differential(&format!("bedrock assistant {name}"), Some(""), base(SONNET, jobj! { "messages": history(parts) }), vec![text_stream()]);
    run("file", vec![jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png" }, jobj! { "type": "text", "text": "t" }]);
    run("only blank", vec![jobj! { "type": "text", "text": "   " }]);
    run("unicode spaces", vec![jobj! { "type": "text", "text": "\u{feff}\u{a0}\u{3000}" }, jobj! { "type": "text", "text": "\u{85}x\u{85}" }]);
    run("bad reasoning metadata", vec![jobj! { "type": "reasoning", "text": "r", "providerOptions": jobj! { "bedrock": jobj! { "signature": 5u64 } } }]);
    run("empty amazonBedrock shadows bedrock", vec![
        jobj! { "type": "reasoning", "text": "r", "providerOptions": jobj! { "amazonBedrock": jobj! {}, "bedrock": jobj! { "signature": "S" } } },
        jobj! { "type": "text", "text": "t" },
    ]);
    let answered = |id: &str| jobj! { "type": "tool-result", "toolCallId": id, "toolName": "read", "output": jobj! { "type": "text", "value": "r" } };
    let shapes = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "read", "input": "text" },
            jobj! { "type": "tool-call", "toolCallId": "c2", "toolName": "read", "input": Json::Null },
            jobj! { "type": "tool-call", "toolCallId": "c3", "toolName": "...", "input": vec![Json::from(1u64)] },
        ] },
        jobj! { "role": "tool", "content": vec![answered("c1"), answered("c2"), answered("c3")] },
    ];
    differential("bedrock assistant tool inputs", Some(""), base(SONNET, jobj! { "messages": shapes }), vec![text_stream()]);
    // A call with no result fails the prompt before the provider sees it, in the SDK's two wordings.
    let unanswered = |ids: &[&str]| {
        let calls: Vec<Json> = ids.iter().map(|id| jobj! { "type": "tool-call", "toolCallId": *id, "toolName": "read", "input": jobj! {} }).collect();
        vec![jobj! { "role": "user", "content": "q" }, jobj! { "role": "assistant", "content": calls }, jobj! { "role": "user", "content": "again" }]
    };
    differential("bedrock one missing result", Some(""), base(SONNET, jobj! { "messages": unanswered(&["c1"]) }), Vec::new());
    differential("bedrock two missing results", Some(""), base(SONNET, jobj! { "messages": unanswered(&["c1", "c2"]) }), Vec::new());
}

#[test]
fn appended_tools_and_system_messages() {
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "system", "content": "", "providerOptions": jobj! { "mewrk": jobj! { "toolAddition": jarr_of(&["read", "gone"]) } } },
        jobj! { "role": "system", "content": "plan mode", "providerOptions": jobj! { "mewrk": jobj! { "systemAppend": true }, "bedrock": jobj! { "cachePoint": jobj! { "type": "default" } } } },
    ];
    differential("bedrock append on", Some(""), base(OPUS, jobj! { "messages": history.clone(), "toolAppend": true, "systemAppend": true }), vec![text_stream()]);
    differential("bedrock append off", Some(""), base(OPUS, jobj! { "messages": history }), vec![text_stream()]);
    let dynamic = jobj! { "system": "stable", "systemDynamic": "tail", "promptCache": false };
    differential("bedrock system tail", Some(""), base(OPUS, dynamic), vec![text_stream()]);
}

#[test]
fn tool_declarations() {
    let run = |name: &str, tools: Vec<Json>| differential(&format!("bedrock tools {name}"), Some(""), base(SONNET, jobj! { "tools": tools }), vec![text_stream()]);
    run("no description", vec![jobj! { "name": "t", "inputSchema": jobj! { "type": "object" } }]);
    run("empty description", vec![jobj! { "name": "t", "description": "", "inputSchema": jobj! { "type": "object" } }]);
    run("two and a duplicate", vec![
        jobj! { "name": "a", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "z": jobj! {}, "y": jobj! {} } } },
        jobj! { "name": "b", "description": "d", "inputSchema": jobj! { "type": "object" } },
        jobj! { "name": "a", "description": "e", "inputSchema": jobj! { "type": "object" } },
    ]);
}

#[test]
fn tool_content_goes_when_no_tools_are_active() {
    differential(
        "bedrock no tools",
        Some(""),
        base(SONNET, jobj! { "tools": Vec::<Json>::new(), "messages": replay_history() }),
        vec![text_stream()],
    );
}

#[test]
fn mistral_tool_ids_are_normalized() {
    let stream = frames(vec![
        event("contentBlockStart", r#"{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"tooluse_bpe71yCfRu2b5i-nKGDr5g","name":"read"}}}"#),
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{}"}}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        event("messageStop", r#"{"stopReason":"tool_use"}"#),
    ]);
    differential("bedrock mistral", Some(""), base("us.mistral.pixtral-large-2502-v1:0", jobj! { "messages": replay_history() }), vec![stream]);
}

#[test]
fn every_reasoning_route() {
    let run = |name: &str, model: &str, extra: Json| differential(&format!("bedrock {name}"), Some(""), base(model, extra), vec![text_stream()]);
    for level in ["low", "medium", "high", "xhigh", "max"] {
        run(&format!("adaptive {level}"), OPUS, jobj! { "reasoning": level });
        run(&format!("4.6 {level}"), "anthropic.claude-sonnet-4-6-v1", jobj! { "reasoning": level });
        run(&format!("budget {level}"), SONNET, jobj! { "reasoning": level, "maxOutputTokens": 8000u64 });
        run(&format!("budget default ceiling {level}"), SONNET, jobj! { "reasoning": level });
        run(&format!("nova 2 {level}"), "us.amazon.nova-2-lite-v1:0", jobj! { "reasoning": level });
        run(&format!("gpt-oss {level}"), "openai.gpt-oss-120b-1:0", jobj! { "reasoning": level });
        run(&format!("gpt-5 {level}"), "us.openai.gpt-5.6-luna", jobj! { "reasoning": level });
        run(&format!("llama {level}"), "us.meta.llama4-scout-17b-instruct-v1:0", jobj! { "reasoning": level });
    }
    // A tiny ceiling: the budget's minimum wins, and the visible part keeps one token.
    run("tiny ceiling", SONNET, jobj! { "reasoning": "low", "maxOutputTokens": 100u64 });
    // Options named by the caller, for models and combinations no plan produces.
    let bedrock = |config: Json| jobj! { "bedrock": jobj! { "reasoningConfig": config } };
    run("nova options", "us.amazon.nova-pro-v1:0", jobj! { "providerOptions": bedrock(jobj! { "type": "enabled", "budgetTokens": 2048u64, "maxReasoningEffort": "low" }) });
    run("nova adaptive", "us.amazon.nova-pro-v1:0", jobj! { "providerOptions": bedrock(jobj! { "type": "adaptive", "maxReasoningEffort": "low" }) });
    run("disabled claude", SONNET, jobj! { "providerOptions": bedrock(jobj! { "type": "disabled" }) });
    run("enabled without budget", SONNET, jobj! { "providerOptions": bedrock(jobj! { "type": "enabled" }) });
    run("adaptive display", OPUS, jobj! { "providerOptions": bedrock(jobj! { "type": "adaptive", "display": "omitted", "maxReasoningEffort": "low" }) });
    run("caller over plan", SONNET, jobj! { "reasoning": "low", "providerOptions": bedrock(jobj! { "type": "disabled", "budgetTokens": 5000u64 }) });
    run("float budget", SONNET, jobj! { "maxOutputTokens": 10u64, "providerOptions": bedrock(jobj! { "type": "enabled", "budgetTokens": 1024.5 }) });
    // `output_config` and `reasoning` the caller already had merge with the effort.
    run(
        "merged effort",
        OPUS,
        jobj! { "providerOptions": jobj! { "bedrock": jobj! {
            "reasoningConfig": jobj! { "maxReasoningEffort": "high" },
            "additionalModelRequestFields": jobj! { "output_config": jobj! { "format": jobj! { "type": "x" } } },
        } } },
    );
    run(
        "merged gpt effort",
        "us.openai.gpt-5.6-luna",
        jobj! { "providerOptions": jobj! { "amazonBedrock": jobj! {
            "reasoningConfig": jobj! { "maxReasoningEffort": "xhigh" },
            "additionalModelRequestFields": jobj! { "reasoning": jobj! { "summary": "auto" } },
        } } },
    );
}

#[test]
fn provider_options_reach_the_command_in_order() {
    let run = |name: &str, model: &str, extra: Json| differential(&format!("bedrock {name}"), Some(""), base(model, extra), vec![text_stream()]);
    run(
        "pass through",
        SONNET,
        jobj! { "providerOptions": jobj! { "bedrock": jobj! {
            "guardrailConfig": jobj! { "guardrailIdentifier": "g", "guardrailVersion": "1", "trace": "enabled" },
            "additionalModelRequestFields": jobj! { "top_k": 5u64 },
            "serviceTier": "priority",
            "anthropicBeta": jarr_of(&["a-beta", "b-beta"]),
            "performanceConfig": jobj! { "latency": "optimized" },
            "inferenceConfig": jobj! { "topP": 1u64 },
        } } },
    );
    run("empty betas", SONNET, jobj! { "providerOptions": jobj! { "bedrock": jobj! { "anthropicBeta": Vec::<Json>::new() } } });
    run(
        "amazonBedrock shadows bedrock",
        SONNET,
        jobj! { "providerOptions": jobj! {
            "bedrock": jobj! { "additionalModelRequestFields": jobj! { "a": 1u64 }, "x": 1u64 },
            "amazonBedrock": jobj! { "y": 2u64 },
        } },
    );
    run("disable parallel", SONNET, jobj! { "providerOptions": jobj! { "anthropic": jobj! { "disableParallelToolUse": true } } });
    run("disable parallel off", SONNET, jobj! { "providerOptions": jobj! { "anthropic": jobj! { "disableParallelToolUse": false } } });
    run("disable parallel on nova", "us.amazon.nova-pro-v1:0", jobj! { "providerOptions": jobj! { "anthropic": jobj! { "disableParallelToolUse": true } } });
    run("invalid bedrock", SONNET, jobj! { "providerOptions": jobj! { "bedrock": jobj! { "serviceTier": "gold" } } });
    run("invalid amazonBedrock", SONNET, jobj! { "providerOptions": jobj! { "amazonBedrock": jobj! { "reasoningConfig": jobj! { "budgetTokens": "many" } } } });
    run("invalid anthropic", SONNET, jobj! { "providerOptions": jobj! { "anthropic": jobj! { "disableParallelToolUse": "yes" } } });
    // Strict is refused for the newest models and kept elsewhere; a blank description goes.
    let strict_tool = vec![jobj! { "name": "t", "description": "  ", "inputSchema": jobj! { "type": "object" } }];
    run("blank description", OPUS, jobj! { "tools": strict_tool.clone() });
    run("headers", SONNET, jobj! { "headers": jobj! { "x-extra": "1", "Authorization": "Custom", "User-Agent": "mine" } });
    run("api key is trimmed", SONNET, jobj! { "apiKey": "  spaced  " });
}

#[test]
fn guard_content_and_part_options() {
    let run = |name: &str, content: Vec<Json>| {
        differential(&format!("bedrock {name}"), Some(""), base(SONNET, jobj! { "messages": vec![jobj! { "role": "user", "content": content }] }), vec![text_stream()]);
    };
    let bedrock = |options: Json| jobj! { "bedrock": options };
    run(
        "guard",
        vec![
            jobj! { "type": "text", "text": "plain", "providerOptions": bedrock(jobj! { "guardContent": false }) },
            jobj! { "type": "text", "text": "guarded", "providerOptions": bedrock(jobj! { "guardContent": true }) },
            jobj! { "type": "text", "text": "qualified", "providerOptions": bedrock(jobj! { "guardContent": true, "guardContentQualifiers": jarr_of(&["query", "grounding_source"]) }) },
            jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png", "providerOptions": bedrock(jobj! { "guardContent": true }) },
        ],
    );
    run("bad qualifier", vec![jobj! { "type": "text", "text": "x", "providerOptions": bedrock(jobj! { "guardContentQualifiers": jarr_of(&["nope"]) }) }]);
    run("s3 image and video", vec![
        jobj! { "type": "text", "text": "x" },
        jobj! { "type": "file", "data": "s3://Bucket/a b.png", "mediaType": "image/png" },
        jobj! { "type": "file", "data": "S3://bucket/clip.mp4", "mediaType": "video/mp4" },
    ]);
    // What is not `image/*` or `video/*` on `s3:` goes to the downloader, which refuses without connecting.
    run("private url", vec![jobj! { "type": "file", "data": "http://127.0.0.1:9/a.png", "mediaType": "image/png" }]);
    run("s3 document", vec![jobj! { "type": "file", "data": "s3://bucket/a.pdf", "mediaType": "application/pdf" }]);
    run("s3 image without a subtype", vec![jobj! { "type": "file", "data": "s3://bucket/a.png", "mediaType": "image/*" }]);
    run("unsupported image", vec![jobj! { "type": "file", "data": "Qk0=", "mediaType": "image/bmp" }]);
    run("unsupported document", vec![jobj! { "type": "file", "data": "aGVsbG8=", "mediaType": "application/zip" }]);
    run("unsupported video", vec![jobj! { "type": "file", "data": "aGVsbG8=", "mediaType": "video/x-unknown" }]);
    run("document kinds", vec![
        jobj! { "type": "file", "data": "aGVsbG8=", "mediaType": "text/csv", "filename": "a.csv" },
        jobj! { "type": "file", "data": "aGVsbG8=", "mediaType": "text/html" },
        jobj! { "type": "file", "data": "aGVsbG8=", "mediaType": "text/markdown" },
        jobj! { "type": "file", "data": "aGVsbG8=", "mediaType": "application/msword", "filename": "noext" },
        jobj! { "type": "file", "data": "aGVsbG8=", "mediaType": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "filename": "" },
    ]);
    run("text data", vec![
        jobj! { "type": "file", "data": jobj! { "type": "text", "text": "h\u{e9}llo" }, "mediaType": "text/plain", "filename": "a.txt" },
        jobj! { "type": "file", "data": jobj! { "type": "text", "text": "x" }, "mediaType": "text/*" },
        jobj! { "type": "file", "data": jobj! { "type": "text", "text": "y" }, "mediaType": "text/csv", "providerOptions": bedrock(jobj! { "citations": jobj! { "enabled": true } }) },
    ]);
    run("text data of a wrong type", vec![jobj! { "type": "file", "data": jobj! { "type": "text", "text": "x" }, "mediaType": "application/json" }]);
    run("reference", vec![jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "bedrock": "id" } }, "mediaType": "image/png" }]);
    run("bad citations", vec![jobj! { "type": "file", "data": "JVBERi0xLjQ=", "mediaType": "application/pdf", "providerOptions": bedrock(jobj! { "citations": jobj! {} }) }]);
}

#[test]
fn native_search_on_converse() {
    let tool_use = |name: &str, input: &str| {
        frames(vec![
            event("contentBlockStart", &format!(r#"{{"contentBlockIndex":0,"start":{{"toolUse":{{"toolUseId":"srv_1","name":"{name}"}}}}}}"#)),
            event("contentBlockDelta", &format!(r#"{{"contentBlockIndex":0,"delta":{{"toolUse":{{"input":{input}}}}}}}"#)),
            event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            event("messageStop", r#"{"stopReason":"tool_use"}"#),
        ])
    };
    let search = |tool_type: Json| jobj! { "tools": Vec::<Json>::new(), "nativeSearch": jobj! { "maxUses": 3u64, "toolType": tool_type } };
    // The default tool version is not supported on Bedrock: it goes, and nothing is left to declare.
    differential("bedrock search default", Some(""), base(OPUS, search(Json::from("web_search_20250305"))), vec![text_stream()]);
    let newer = search(Json::from("web_search_20260209"));
    differential("bedrock search newer", Some(""), base(OPUS, newer.clone()), vec![tool_use("web_search", r#""{\"query\":\"q\"}""#)]);
    differential("bedrock search with history", Some(""), base(OPUS, {
        let mut extra = newer.clone();
        extra.as_obj_mut().unwrap().insert("messages", replay_history());
        extra
    }), vec![text_stream()]);
    differential("bedrock search on a model id without the dot", Some(""), base("us:anthropic-claude", newer.clone()), vec![text_stream()]);
    differential("bedrock search on nova", Some(""), base("us.amazon.nova-pro-v1:0", newer), vec![text_stream()]);
    // With function tools next to it, and the search out of budget.
    differential(
        "bedrock search and tools",
        Some(""),
        base(OPUS, jobj! { "nativeSearch": jobj! { "maxUses": 1u64, "toolType": "web_search_20260209", "previousCallIds": jarr_of(&["old"]) } }),
        vec![tool_use("read", r#""{}""#)],
    );
    // Bedrock reports no provider execution, so a search is an ordinary call and the loop does not continue.
    differential(
        "bedrock search loop",
        Some(""),
        base(OPUS, jobj! { "tools": Vec::<Json>::new(), "maxSteps": 4u64, "nativeSearch": jobj! { "maxUses": 1u64, "toolType": "web_search_20260209" } }),
        vec![tool_use("web_search", r#""{\"query\":\"q\"}""#), text_stream()],
    );
    differential(
        "bedrock fetch",
        Some(""),
        base(OPUS, jobj! { "tools": Vec::<Json>::new(), "nativeFetch": jobj! { "maxUses": 2u64, "maxContentTokens": 100u64 } }),
        vec![tool_use("web_fetch", r#""{\"url\":\"https://a.example\"}""#)],
    );
    differential(
        "bedrock fetch newer and search",
        Some(""),
        base(OPUS, jobj! {
            "nativeFetch": jobj! { "toolType": "web_fetch_20260209" },
            "nativeSearch": jobj! { "toolType": "web_search_20260209" },
            "providerOptions": jobj! { "anthropic": jobj! { "disableParallelToolUse": true }, "bedrock": jobj! { "anthropicBeta": jarr_of(&["mine"]) } },
        }),
        vec![text_stream()],
    );
}

#[test]
fn usage_finish_reasons_and_metadata() {
    for reason in ["end_turn", "stop_sequence", "max_tokens", "content_filtered", "guardrail_intervened", "tool_use", "stop", "something_new"] {
        differential(&format!("bedrock finish {reason}"), Some(""), base(SONNET, jobj! {}), vec![frames(vec![
            event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#),
            event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            event("messageStop", &format!(r#"{{"stopReason":"{reason}"}}"#)),
        ])]);
    }
    let cached = frames(vec![
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        event("messageStop", r#"{"stopReason":"stop_sequence","additionalModelResponseFields":{"delta":{"stop_sequence":"END"},"other":1}}"#),
        event(
            "metadata",
            r#"{"usage":{"inputTokens":3,"outputTokens":9,"totalTokens":99,"cacheReadInputTokens":20,"cacheWriteInputTokens":7,"cacheDetails":[{"inputTokens":7,"ttl":"5m"}],"future":{"x":1}},"trace":{"guardrail":{}},"performanceConfig":{"latency":"optimized"},"serviceTier":{"type":"priority"}}"#,
        ),
    ]);
    differential("bedrock cached usage", Some(""), base(SONNET, jobj! {}), vec![cached]);
    differential("bedrock null cache counts", Some(""), base(SONNET, jobj! {}), vec![frames(vec![
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
        event("metadata", r#"{"usage":{"inputTokens":3,"outputTokens":9,"cacheReadInputTokens":null,"cacheWriteInputTokens":null}}"#),
    ])]);
    // A stream that names no stop and no usage, and one with nothing in it at all.
    differential("bedrock no stop", Some(""), base(SONNET, jobj! {}), vec![frames(vec![
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#),
    ])]);
    differential("bedrock empty stream", Some(""), base(SONNET, jobj! {}), vec![frames(Vec::new())]);
}

#[test]
fn http_errors() {
    let run = |name: &str, reply: Reply| differential(&format!("bedrock {name}"), Some(""), base(SONNET, jobj! {}), vec![reply]);
    run("429", Reply::json(429, r#"{"message":"Too many tokens"}"#).header("retry-after", "7").header("x-amzn-errortype", "ThrottlingException"));
    run("typed 400", Reply::json(400, r#"{"message":"bad","type":"ValidationException"}"#));
    run("null type 400", Reply::json(400, r#"{"message":"bad","type":null}"#));
    run("number type 400", Reply::json(400, r#"{"message":"bad","type":5}"#));
    run("no message 403", Reply::json(403, r#"{"Message":"denied"}"#));
    run("array 500", Reply::json(500, r#"[]"#));
    run("html 502", Reply::new(502).header("content-type", "text/html").body("<html>"));
    run("empty 503", Reply::new(503));
    run("529", Reply::json(529, r#"{"message":"overloaded"}"#));
}

#[test]
fn stream_exceptions() {
    for kind in ["internalServerException", "modelStreamErrorException", "serviceUnavailableException", "throttlingException", "validationException"] {
        let open = event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"partial"}}"#);
        differential(&format!("bedrock {kind}"), Some(""), base(SONNET, jobj! {}), vec![frames(vec![open.clone(), exception(kind, r#"{"message":"boom","originalStatusCode":500}"#)])]);
        differential(&format!("bedrock {kind} first"), Some(""), base(SONNET, jobj! {}), vec![frames(vec![exception(kind, r#"{"message":"boom"}"#)])]);
        differential(&format!("bedrock {kind} wordless"), Some(""), base(SONNET, jobj! {}), vec![frames(vec![exception(kind, "{}")])]);
        differential(&format!("bedrock {kind} number message"), Some(""), base(SONNET, jobj! {}), vec![frames(vec![exception(kind, r#"{"message":5}"#)])]);
        differential(&format!("bedrock {kind} null"), Some(""), base(SONNET, jobj! {}), vec![frames(vec![event(kind, "[]"), event("messageStop", r#"{"stopReason":"end_turn"}"#)])]);
    }
    // Spellings and kinds outside the schema strip away: the stream carries on.
    for kind in ["ThrottlingException", "accessDeniedException"] {
        differential(&format!("bedrock unnamed {kind}"), Some(""), base(SONNET, jobj! {}), vec![frames(vec![
            event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#),
            exception(kind, r#"{"message":"boom"}"#),
            event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
            event("messageStop", r#"{"stopReason":"end_turn"}"#),
        ])]);
    }
    // The wire's third message type has no branch in the handler.
    differential("bedrock error message type", Some(""), base(SONNET, jobj! {}), vec![frames(vec![
        event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#),
        encode_frame(&[(":message-type", string("error")), (":error-code", string("X")), (":error-message", string("Y"))], b""),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
    ])]);
}

#[test]
fn chunks_that_fail_the_schema() {
    let cases = [
        ("no index", event("contentBlockStop", "{}")),
        ("string index", event("contentBlockDelta", r#"{"contentBlockIndex":"0","delta":{"text":"t"}}"#)),
        ("unknown delta", event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"citation":{"x":1}}}"#)),
        ("tool delta without input", event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"toolUse":{}}}"#)),
        ("no stop reason", event("messageStop", "{}")),
        ("number stop reason", event("messageStop", r#"{"stopReason":5}"#)),
        ("usage without output", event("metadata", r#"{"usage":{"inputTokens":1}}"#)),
        ("bad cache detail", event("metadata", r#"{"usage":{"inputTokens":1,"outputTokens":1,"cacheDetails":[{"inputTokens":"x"}]}}"#)),
        ("bad tier", event("metadata", r#"{"serviceTier":{}}"#)),
        ("start without tool id", event("contentBlockStart", r#"{"contentBlockIndex":0,"start":{"toolUse":{"name":"t"}}}"#)),
        ("exception as text", event("throttlingException", r#""slow""#)),
        ("number body", event("contentBlockStop", "5")),
    ];
    for (name, frame) in cases {
        let open = event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#);
        differential(&format!("bedrock schema {name}"), Some(""), base(SONNET, jobj! {}), vec![frames(vec![open, frame, event("messageStop", r#"{"stopReason":"end_turn"}"#)])]);
    }
}

#[test]
fn frames_that_fail_to_decode() {
    let good = event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#);
    let flip = |at: usize| {
        let mut frame = good.clone();
        let index = if at == usize::MAX { frame.len() - 1 } else { at };
        frame[index] ^= 0x40;
        frame
    };
    // Upstream's decoder throws inside a `TransformStream`, and an errored stream discards what is still queued
    // behind it. Whether a delta decoded just before the failure survives is a race there, so the pause lets each one
    // through first. See `decoding_failures_keep_earlier_frames` in `tests/bedrock.rs` for what this crate does.
    let run = |name: &str, chunks: Vec<Vec<u8>>| differential(&format!("bedrock decode {name}"), Some(""), base(SONNET, jobj! {}), vec![frames(chunks).pause(Duration::from_millis(100))]);
    run("prelude crc", vec![good.clone(), flip(8)]);
    run("message crc", vec![good.clone(), flip(usize::MAX)]);
    run("payload", vec![good.clone(), flip(good.len() - 8)]);
    // A declared length under the minimum, and one past what ever arrives.
    let mut short = vec![0, 0, 0, 12];
    short.extend_from_slice(&[0; 8]);
    run("too short", vec![good.clone(), short]);
    run("truncated", vec![good.clone(), good[..good.len() - 5].to_vec()]);
    run("three bytes left", vec![good.clone(), vec![0, 0, 0]]);
    run("null payload", vec![good.clone(), event("metadata", "null")]);
    run("bad tag", vec![good.clone(), raw_frame(&[1, b'a', 42], b"{}")]);
    run("header past its end", vec![good.clone(), raw_frame(&[1, b'a', 7, 0], b"")]);
    // Decoding errors end the stream where they are; what follows never reaches the model.
    run("then more", vec![flip(usize::MAX), good.clone()]);
    run("reasoning open", vec![event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"r"}}}"#), flip(8)]);
}

/// A frame around header bytes no encoder would write, with valid CRCs: the decoder must get as far as the headers.
fn raw_frame(headers: &[u8], body: &[u8]) -> Vec<u8> {
    fn crc(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { 0xedb8_8320 ^ (crc >> 1) } else { crc >> 1 };
            }
        }
        crc ^ 0xffff_ffff
    }
    let length = headers.len() + body.len() + 16;
    let mut out = Vec::with_capacity(length);
    out.extend_from_slice(&(length as u32).to_be_bytes());
    out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    let prelude = crc(&out);
    out.extend_from_slice(&prelude.to_be_bytes());
    out.extend_from_slice(headers);
    out.extend_from_slice(body);
    let message = crc(&out);
    out.extend_from_slice(&message.to_be_bytes());
    out
}

/// V8's `JSON.parse` messages are not serde_json's. Everything up to the detail is compared.
fn without_parse_detail(text: &str) -> String {
    match text.find("Error message: ") {
        Some(at) => text[..at].to_owned(),
        None => text.to_owned(),
    }
}

#[test]
fn payloads_that_are_not_json() {
    let open = event("contentBlockDelta", r#"{"contentBlockIndex":0,"delta":{"text":"t"}}"#);
    for (name, payload) in [("words", "not json"), ("empty", ""), ("cut", r#"{"a":"#)] {
        differential_normalized(&format!("bedrock not json {name}"), Some(""), base(SONNET, jobj! {}), vec![frames(vec![open.clone(), event("metadata", payload)])], &without_parse_detail);
    }
    differential("bedrock forbidden key", Some(""), base(SONNET, jobj! {}), vec![frames(vec![open.clone(), event("metadata", r#"{"__proto__":{"x":1}}"#)])]);
}

/// No `AWS_*` variable ever reached the sidecar: the sidecar answers an empty key with these errors, before anything
/// is sent. The `differential` calls below have no scripted reply because no request may arrive.
#[test]
fn missing_credentials() {
    let run = |name: &str, base_path: Option<&str>, extra: Json| differential(&format!("bedrock credentials {name}"), base_path, base(SONNET, extra), Vec::new());
    run("empty key", Some(""), jobj! { "apiKey": "" });
    run("blank key", Some(""), jobj! { "apiKey": " \t\n" });
    run("no key", Some(""), jobj! { "apiKey": Json::Null });
    run("empty key, no region", Some(""), jobj! { "apiKey": "", "settings": jobj! {} });
    run("empty key, empty region", Some(""), jobj! { "apiKey": "", "settings": jobj! { "region": "" } });
    run("empty key, no endpoint", None, jobj! { "apiKey": "" });
    run("empty key, nothing", None, jobj! { "apiKey": "", "settings": jobj! {} });
    run("key, no endpoint, no region", None, jobj! { "settings": jobj! {} });
    // The prompt is converted before the endpoint is resolved or the credentials are loaded.
    run("bad prompt first", None, jobj! { "apiKey": "", "settings": jobj! {}, "messages": vec![jobj! { "role": "user", "content": vec![jobj! { "type": "file", "data": "Qk0=", "mediaType": "image/bmp" }] }] });
}

/// The real sidecar with `AWS_*` variables it never has in production, under a frozen clock, against the signer
/// driven directly: the whole SigV4 path, compared byte for byte, signature included.
#[test]
fn sigv4_matches_the_sidecar_under_a_frozen_clock() {
    let Some(node) = node_sidecar_path() else {
        eprintln!("sigv4: skipped, MEWRK_AISDK_NODE_SIDECAR is not set");
        return;
    };
    let freeze = "data:text/javascript,const R=Date,F=R.UTC(2026,0,2,3,4,5,678);globalThis.Date=class extends R{constructor(...a){if(a.length)super(...a);else super(F)}static now(){return F}};";
    let mock = Mock::script(vec![text_stream()]);
    for token in [Some("SESSION/TOKEN=="), None] {
        let before = mock.requests().len();
        let payload = jobj! {
            "family": "bedrock",
            "apiKey": "",
            "baseURL": format!("{}/bedrock/", mock.base),
            "settings": jobj! { "region": "eu-west-1" },
            "headers": jobj! { "x-extra": "  a   b ", "Authorization": "ignored" },
            "modelId": SONNET,
            "system": "S",
            "messages": vec![jobj! { "role": "user", "content": "h\u{e9}llo" }],
            "maxSteps": 1u64,
        };
        let mut command = Command::new("node");
        command
            .arg("--import")
            .arg(freeze)
            .arg(&node)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(token) = token {
            command.env("AWS_SESSION_TOKEN", token);
        }
        let mut child = command.spawn().expect("node");
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, r#"{{"v":18,"type":"hello"}}"#).unwrap();
        writeln!(stdin, "{}", step_frame("s", &payload)).unwrap();
        stdin.flush().unwrap();
        for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
            if line.contains(r#""type":"done""#) || line.contains(r#""type":"error""#) {
                assert!(line.contains(r#""type":"done""#), "{line}");
                break;
            }
        }
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(mock.requests().len(), before + 1, "the sidecar made one request");

        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let mut headers = Headers::new();
            headers.set("x-extra", "  a   b ");
            headers.set("Authorization", "ignored");
            let model = create_signed(
                ProviderSettings {
                    base_url: Some(format!("{}/bedrock/", mock.base)),
                    api_key: String::new(),
                    headers,
                    settings: Default::default(),
                    model_id: SONNET.to_owned(),
                    fetch: NetFetch::shared(),
                },
                Credentials {
                    region: "eu-west-1".to_owned(),
                    access_key_id: "AKIDEXAMPLE".to_owned(),
                    secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
                    session_token: token.map(str::to_owned),
                },
                Box::new(|| "20260102T030405Z".to_owned()),
            )
            .unwrap();
            let options = CallOptions {
                prompt: vec![
                    jobj! { "role": "system", "content": "S" },
                    jobj! { "role": "user", "content": vec![jobj! { "type": "text", "text": "h\u{e9}llo" }] },
                ],
                tools: None,
                tool_choice: jobj! { "type": "auto" },
                max_output_tokens: Some(32_000),
                reasoning: None,
                provider_options: None,
                cancel: Cancel::new(),
            };
            let mut stream = model.do_stream(options).await.map_err(|e| e.to_string()).unwrap();
            while stream.next().await.is_some() {}
        });
        let seen = mock.requests();
        let (node_request, rust_request) = (&seen[before], &seen[before + 1]);
        let authorization = node_request.header("authorization").unwrap();
        assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260102/eu-west-1/bedrock/aws4_request, SignedHeaders=host;x-amz-date;"), "{authorization}");
        assert_eq!(node_request.header("x-amz-date"), Some("20260102T030405Z"));
        assert_eq!(comparable_request(node_request), comparable_request(rust_request));
    }
}
