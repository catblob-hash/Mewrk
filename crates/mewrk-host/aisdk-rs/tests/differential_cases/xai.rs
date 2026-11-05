use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;

use super::{differential, differential_normalized};
use crate::common::Reply;

const PNG: &str = "iVBORw0KGgo=";

fn tools() -> Vec<Json> {
    vec![
        jobj! { "name": "read", "description": "d", "inputSchema": jobj! {
            "type": "object",
            "properties": jobj! {
                "path": jobj! { "type": "string" },
                "nested": jobj! { "type": "object", "additionalProperties": false, "properties": jobj! {} },
                "open": jobj! { "type": "object", "additionalProperties": true },
                "list": jobj! { "type": "array", "items": jobj! { "type": "object", "additionalProperties": false } },
            },
            "additionalProperties": false,
        } },
        jobj! { "name": "ls", "description": "e", "inputSchema": jobj! { "type": "object" } },
    ]
}

fn base(model: &str, extra: Json) -> Json {
    let mut payload = jobj! {
        "family": "xai",
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

fn run(name: &str, model: &str, extra: Json, replies: Vec<Reply>) {
    differential(name, Some("/v1"), base(model, extra), replies);
}

fn without_zod_report(text: &str) -> String {
    text.split_once("Error message:").map_or_else(|| text.to_owned(), |(head, _)| head.to_owned())
}

/// V8's and serde's wording of a JSON syntax error: the one thing in a failed tool input that differs.
fn without_syntax_error_text(text: &str) -> String {
    let Some((head, tail)) = text.split_once("SyntaxError: ") else { return text.to_owned() };
    let rest = tail.split_once('"').map_or("", |(_, rest)| rest);
    format!("{head}SyntaxError\"{rest}")
}

const USAGE: &str = r#"{"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":2},"num_sources_used":1,"num_server_side_tools_used":1,"cost_in_usd_ticks":123}"#;

fn created() -> String {
    r#"{"type":"response.created","response":{"id":"resp_1","created_at":1,"model":"grok-served","object":"response","output":[],"status":"in_progress"}}"#.to_owned()
}

fn completed(status: &str, usage: &str) -> String {
    format!(r#"{{"type":"response.completed","response":{{"id":"resp_1","created_at":1,"model":"grok-served","object":"response","output":[],"status":"{status}","usage":{usage},"service_tier":"default"}}}}"#)
}

fn sse(events: &[String]) -> Reply {
    Reply::sse(&events.iter().map(String::as_str).collect::<Vec<_>>())
}

fn text_events() -> Vec<String> {
    vec![
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}}"#.to_owned(),
        r#"{"type":"response.content_part.added","item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}"#.to_owned(),
        r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi "}"#.to_owned(),
        r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"there"}"#.to_owned(),
        r#"{"type":"response.output_text.done","item_id":"msg_1","output_index":0,"content_index":0,"text":"hi there"}"#.to_owned(),
        r#"{"type":"response.content_part.done","item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"hi there"}}"#.to_owned(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hi there"}]}}"#.to_owned(),
    ]
}

fn text_stream() -> Reply {
    let mut events = vec![created()];
    events.extend(text_events());
    events.push(completed("completed", USAGE));
    sse(&events)
}

fn reasoning_tool_stream() -> Reply {
    sse(&[
        created(),
        r#"{"type":"response.in_progress","response":{"id":"resp_again","model":"ignored","object":"response","output":[]}}"#.to_owned(),
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"status":"in_progress"}}"#.to_owned(),
        r#"{"type":"response.reasoning_summary_part.added","item_id":"rs_1","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#.to_owned(),
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"Let me "}"#.to_owned(),
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"think"}"#.to_owned(),
        r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_1","output_index":0,"summary_index":0,"text":"Let me think"}"#.to_owned(),
        r#"{"type":"response.reasoning_summary_part.done","item_id":"rs_1","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":"Let me think"}}"#.to_owned(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Let me think"}],"status":"completed","encrypted_content":"ENC"}}"#.to_owned(),
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}"#.to_owned(),
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"{\"path\":"}"#.to_owned(),
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"\"a\"}"}"#.to_owned(),
        r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","output_index":1,"arguments":"{\"path\":\"a\"}"}"#.to_owned(),
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a\"}"}}"#.to_owned(),
        completed("completed", USAGE),
    ])
}

#[test]
fn plain_text_and_request_shape() {
    run("xai text", "grok-4.3", jobj! {}, vec![text_stream()]);
    run("xai max output", "grok-4.3", jobj! { "maxOutputTokens": 77u64, "tools": Vec::<Json>::new() }, vec![text_stream()]);
    differential("xai trailing slash", Some("/v1/"), base("grok-4.3", jobj! {}), vec![text_stream()]);
    run("xai empty key", "grok-4.3", jobj! { "apiKey": "" }, vec![text_stream()]);
    run("xai headers", "grok-4.3", jobj! { "headers": jobj! { "X-Extra": "1", "Authorization": "Bearer other", "User-Agent": "host/1" } }, vec![text_stream()]);
    run("xai message item only", "grok-4.3", jobj! {}, vec![sse(&[
        created(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_2","role":"assistant","status":"completed","content":[{"type":"output_text","text":"whole"},{"type":"output_text","text":"second ignored"}]}}"#.to_owned(),
        completed("completed", USAGE),
    ])]);
    run("xai two messages", "grok-4.3", jobj! {}, vec![sse(&[
        r#"{"type":"response.output_text.delta","item_id":"a","output_index":0,"content_index":0,"delta":"one"}"#.to_owned(),
        r#"{"type":"response.output_text.delta","item_id":"b","output_index":1,"content_index":0,"delta":"two"}"#.to_owned(),
        completed("completed", USAGE),
    ])]);
}

#[test]
fn reasoning_and_tools() {
    run("xai summary and function call", "grok-4.3", jobj! { "reasoning": "high", "reasoningContent": "plaintext" }, vec![reasoning_tool_stream()]);
    run("xai reasoning text", "grok-4.3", jobj! {}, vec![sse(&[
        created(),
        r#"{"type":"response.reasoning_text.delta","item_id":"rs_1","output_index":0,"content_index":0,"delta":"raw "}"#.to_owned(),
        r#"{"type":"response.reasoning_text.delta","item_id":"rs_1","output_index":0,"content_index":0,"delta":"thought"}"#.to_owned(),
        r#"{"type":"response.reasoning_text.done","item_id":"rs_1","output_index":0,"content_index":0,"text":"raw thought"}"#.to_owned(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"content":[{"type":"reasoning_text","text":"raw thought"}],"status":"completed"}}"#.to_owned(),
        completed("completed", USAGE),
    ])]);
    run("xai encrypted only", "grok-4.3", jobj! { "reasoningContent": "encrypted" }, vec![sse(&[
        created(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_e","summary":[],"status":"completed","encrypted_content":"OPAQUE"}}"#.to_owned(),
        completed("completed", USAGE),
    ])]);
    run("xai summary delta before part", "grok-4.3", jobj! {}, vec![sse(&[
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"summary_index":0,"delta":"orphan"}"#.to_owned(),
        completed("completed", USAGE),
    ])]);
    run("xai end without added", "grok-4.3", jobj! {}, vec![sse(&[
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"f","call_id":"c","name":"ls","arguments":"{}"}}"#.to_owned(),
        completed("completed", USAGE),
    ])]);
    differential_normalized("xai bad arguments", Some("/v1"), base("grok-4.3", jobj! {}), vec![sse(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"f","call_id":"c","name":"read","arguments":""}}"#.to_owned(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"f","call_id":"c","name":"read","arguments":"{nope"}}"#.to_owned(),
        completed("completed", USAGE),
    ])], &without_syntax_error_text);
}

#[test]
fn reasoning_levels_per_model() {
    for (model, level) in [
        ("grok-3-mini", "high"), ("grok-3-mini", "medium"), ("grok-3-mini", "max"),
        ("grok-4.3", "xhigh"), ("grok-4.3", "max"), ("grok-4.6", "max"), ("grok-5", "low"),
        ("grok-4.20-reasoning", "high"), ("grok-4.20-0309-non-reasoning", "high"), ("grok-2", "high"),
    ] {
        run(&format!("xai level {model} {level}"), model, jobj! { "reasoning": level }, vec![text_stream()]);
    }
}

fn replay_history() -> Vec<Json> {
    vec![
        jobj! { "role": "user", "content": vec![
            jobj! { "type": "text", "text": "look" },
            jobj! { "type": "file", "data": PNG, "mediaType": "image/png" },
            jobj! { "type": "file", "data": PNG, "mediaType": "image/*", "providerOptions": jobj! { "xai": jobj! { "imageDetail": "low" } } },
            jobj! { "type": "file", "data": jobj! { "type": "url", "url": "https://files.example/a.png" }, "mediaType": "image/png", "providerOptions": jobj! { "xai": jobj! { "imageDetail": "high" } } },
            jobj! { "type": "file", "data": jobj! { "type": "url", "url": "https://files.example/a.pdf" }, "mediaType": "application/pdf" },
            jobj! { "type": "file", "data": jobj! { "type": "url", "url": "https://files.example/a.txt" }, "mediaType": "text/plain" },
            jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "xai": "file-1" } }, "mediaType": "application/pdf" },
        ] },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "summary", "providerOptions": jobj! { "xai": jobj! { "itemId": "rs_1", "reasoningEncryptedContent": "ENC" } } },
            jobj! { "type": "reasoning", "text": "", "providerOptions": jobj! { "xai": jobj! { "reasoningEncryptedContent": "ENC2" } } },
            jobj! { "type": "reasoning", "text": "", "providerOptions": jobj! { "xai": jobj! { "itemId": "rs_3" } } },
            jobj! { "type": "reasoning", "text": "plain, dropped" },
            jobj! { "type": "text", "text": "answer", "providerOptions": jobj! { "xai": jobj! { "itemId": "msg_1" } } },
            jobj! { "type": "text", "text": "more" },
            jobj! { "type": "tool-call", "toolCallId": "c0", "toolName": "read", "input": jobj! { "path": "x" }, "providerOptions": jobj! { "xai": jobj! { "itemId": "fc_0" } } },
            jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "ls", "input": jobj! {} },
            jobj! { "type": "tool-call", "toolCallId": "ws_1", "toolName": "web_search", "input": jobj! {}, "providerExecuted": true },
            jobj! { "type": "tool-result", "toolCallId": "ws_1", "toolName": "web_search", "output": jobj! { "type": "json", "value": jobj! {} } },
        ] },
        jobj! { "role": "tool", "content": vec![
            jobj! { "type": "tool-result", "toolCallId": "c0", "toolName": "read", "output": jobj! { "type": "content", "value": vec![
                jobj! { "type": "text", "text": "body" },
                jobj! { "type": "file", "data": jobj! { "type": "data", "data": PNG }, "mediaType": "image/png" },
                jobj! { "type": "image-url", "url": "https://files.example/b.png" },
                jobj! { "type": "file-url", "url": "https://files.example/c.png", "mediaType": "image/png" },
                jobj! { "type": "image-data", "data": PNG, "mediaType": "image/png" },
                jobj! { "type": "file", "data": jobj! { "type": "data", "data": "AAAA" }, "mediaType": "application/pdf" },
                jobj! { "type": "custom" },
            ] } },
            jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "ls", "output": jobj! { "type": "json", "value": jobj! { "a": 1u64 } } },
        ] },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c2", "toolName": "ls", "input": jobj! {} }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c2", "toolName": "ls", "output": jobj! { "type": "execution-denied" } }] },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c3", "toolName": "ls", "input": jobj! {} }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c3", "toolName": "ls", "output": jobj! { "type": "error-text", "value": "bad" } }] },
        jobj! { "role": "user", "content": "again" },
    ]
}

#[test]
fn replayed_history() {
    run("xai replay", "grok-4.3", jobj! { "messages": replay_history() }, vec![text_stream()]);
    for (name, part) in [
        ("inline pdf", jobj! { "type": "file", "data": "JVBERg==", "mediaType": "application/pdf" }),
        ("text data", jobj! { "type": "file", "data": jobj! { "type": "text", "text": "body" }, "mediaType": "text/plain" }),
        ("foreign reference", jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "openai": "file-1" } }, "mediaType": "application/pdf" }),
        ("bad detail", jobj! { "type": "file", "data": PNG, "mediaType": "image/png", "providerOptions": jobj! { "xai": jobj! { "imageDetail": "huge" } } }),
    ] {
        run(&format!("xai file {name}"), "grok-4.3", jobj! { "messages": vec![jobj! { "role": "user", "content": vec![part] }] }, vec![text_stream()]);
    }
}

#[test]
fn provider_options() {
    run("xai options all", "grok-4.3", jobj! { "providerOptions": jobj! { "xai": jobj! {
        "reasoningEffort": "none",
        "reasoningSummary": "detailed",
        "logprobs": true,
        "topLogprobs": 2u64,
        "serviceTier": "priority",
        "store": false,
        "previousResponseId": "resp_0",
        "include": vec![Json::from("file_search_call.results")],
    } } }, vec![text_stream()]);
    run("xai store off", "grok-4.3", jobj! { "providerOptions": jobj! { "xai": jobj! { "store": false } } }, vec![text_stream()]);
    run("xai store on", "grok-4.3", jobj! { "providerOptions": jobj! { "xai": jobj! { "store": true, "include": Json::Null, "logprobs": false } } }, vec![text_stream()]);
    run("xai empty include", "grok-4.3", jobj! { "providerOptions": jobj! { "xai": jobj! { "include": Vec::<Json>::new() } } }, vec![text_stream()]);
    run("xai top logprobs alone", "grok-4.3", jobj! { "providerOptions": jobj! { "xai": jobj! { "topLogprobs": 0u64 } } }, vec![text_stream()]);
    run("xai summary alone", "grok-4.20-reasoning", jobj! { "providerOptions": jobj! { "xai": jobj! { "reasoningSummary": "auto" } } }, vec![text_stream()]);
    run("xai plan over options", "grok-4.3", jobj! { "reasoning": "low", "providerOptions": jobj! { "xai": jobj! { "reasoningEffort": "high", "reasoningSummary": "concise" } } }, vec![text_stream()]);
    run("xai foreign options", "grok-4.3", jobj! { "providerOptions": jobj! { "openai": jobj! { "x": 1u64 }, "xai": jobj! { "unknown": 1u64 } } }, vec![text_stream()]);
    for (name, options) in [
        ("effort", jobj! { "reasoningEffort": "max" }),
        ("summary", jobj! { "reasoningSummary": "short" }),
        ("logprobs", jobj! { "logprobs": "yes" }),
        ("top", jobj! { "topLogprobs": 9u64 }),
        ("top fraction", jobj! { "topLogprobs": 1.5f64 }),
        ("tier", jobj! { "serviceTier": "flex" }),
        ("store", jobj! { "store": Json::Null }),
        ("previous", jobj! { "previousResponseId": 1u64 }),
        ("include", jobj! { "include": vec![Json::from("reasoning.encrypted_content")] }),
    ] {
        run(&format!("xai bad options {name}"), "grok-4.3", jobj! { "providerOptions": jobj! { "xai": options } }, vec![text_stream()]);
    }
    run("xai options not an object", "grok-4.3", jobj! { "providerOptions": jobj! { "xai": "x" } }, vec![text_stream()]);
}

#[test]
fn native_search_and_server_tools() {
    let search = sse(&[
        created(),
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress","name":"web_search","arguments":"{\"query\":\"q\"}"}}"#.to_owned(),
        r#"{"type":"response.web_search_call.in_progress","item_id":"ws_1","output_index":0}"#.to_owned(),
        r#"{"type":"response.web_search_call.searching","item_id":"ws_1","output_index":0}"#.to_owned(),
        r#"{"type":"response.web_search_call.completed","item_id":"ws_1","output_index":0}"#.to_owned(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed","name":"web_search","arguments":"{\"query\":\"q\"}"}}"#.to_owned(),
        r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"found"}"#.to_owned(),
        r#"{"type":"response.output_text.annotation.added","item_id":"msg_1","output_index":1,"content_index":0,"annotation_index":0,"annotation":{"type":"url_citation","url":"https://a.example/x","title":"A"}}"#.to_owned(),
        r#"{"type":"response.output_text.annotation.added","item_id":"msg_1","output_index":1,"content_index":0,"annotation_index":1,"annotation":{"type":"url_citation","url":"https://b.example/y"}}"#.to_owned(),
        r#"{"type":"response.output_text.annotation.added","item_id":"msg_1","output_index":1,"content_index":0,"annotation_index":2,"annotation":{"type":"file_citation"}}"#.to_owned(),
        r#"{"type":"response.output_text.annotation.added","item_id":"msg_1","output_index":1,"content_index":0,"annotation_index":3,"annotation":{"type":"url_citation","url":"https://c.example/z","title":null}}"#.to_owned(),
        r#"{"type":"response.output_text.done","item_id":"msg_1","output_index":1,"content_index":0,"text":"found","annotations":[{"type":"url_citation","url":"https://d.example/w","title":"D"}]}"#.to_owned(),
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"found","annotations":[{"type":"url_citation","url":"https://e.example/v","title":"E"}]}]}}"#.to_owned(),
        completed("completed", USAGE),
    ]);
    run("xai native search", "grok-4.3", jobj! { "tools": Vec::<Json>::new(), "nativeSearch": jobj! { "maxUses": 3u64 } }, vec![search.clone()]);
    run("xai native search with tools", "grok-4.3", jobj! { "nativeSearch": jobj! { "maxUses": 0u64 } }, vec![search.clone()]);
    run("xai no search tool declared", "grok-4.3", jobj! {}, vec![search]);
    let item = |event: &str, body: &str| format!(r#"{{"type":"response.output_item.{event}","output_index":0,"item":{body}}}"#);
    for (name, body) in [
        ("sub tool", r#"{"type":"custom_tool_call","id":"t1","status":"completed","name":"browse_page","input":"{\"url\":\"u\"}"}"#),
        ("snippets", r#"{"type":"web_search_call","id":"t2","status":"completed","name":"web_search_with_snippets"}"#),
        ("x search", r#"{"type":"x_search_call","id":"t3","status":"completed","name":"x_keyword_search","arguments":"{}"}"#),
        ("x sub tool", r#"{"type":"custom_tool_call","id":"t4","status":"completed","name":"x_user_search","input":"{}"}"#),
        ("code", r#"{"type":"code_interpreter_call","id":"t5","status":"completed"}"#),
        ("code execution", r#"{"type":"code_execution_call","id":"t6","status":"completed","name":"code_execution"}"#),
        ("custom code", r#"{"type":"custom_tool_call","id":"t7","status":"completed","name":"code_execution","input":"print(1)"}"#),
        ("view image", r#"{"type":"view_image_call","id":"t8","status":"completed","name":"view_image"}"#),
        ("video", r#"{"type":"view_x_video_call","id":"t9","status":"completed"}"#),
        ("mcp", r#"{"type":"mcp_call","id":"t10","status":"completed","name":"remote","arguments":"{}"}"#),
        ("mcp bare", r#"{"type":"mcp_call","id":"t11","status":"completed"}"#),
        ("file search", r#"{"type":"file_search_call","id":"t12","status":"completed","queries":["q"],"results":[{"file_id":"f","filename":"n","score":0.5,"text":"t"}]}"#),
        ("file search bare", r#"{"type":"file_search_call","id":"t13","status":"completed"}"#),
        ("image", r#"{"type":"image_generation_call","id":"t14","status":"completed","prompt":"p","result":"QUJD"}"#),
        ("image failed", r#"{"type":"image_generation_call","id":"t15","status":"failed"}"#),
    ] {
        run(&format!("xai server tool {name}"), "grok-4.3", jobj! { "tools": Vec::<Json>::new(), "nativeSearch": jobj! { "maxUses": 1u64 } }, vec![sse(&[
            created(),
            item("added", body),
            item("done", body),
            completed("completed", USAGE),
        ])]);
    }
    run("xai image progress", "grok-4.3", jobj! {}, vec![sse(&[
        r#"{"type":"response.image_generation_call.in_progress","item_id":"i","output_index":0}"#.to_owned(),
        r#"{"type":"response.image_generation_call.generating","item_id":"i","output_index":0}"#.to_owned(),
        completed("completed", USAGE),
    ])]);
    run("xai progress events", "grok-4.3", jobj! {}, vec![sse(&[
        r#"{"type":"response.code_interpreter_call.interpreting","item_id":"c","output_index":0}"#.to_owned(),
        r#"{"type":"response.code_interpreter_call_code.delta","item_id":"c","output_index":0,"delta":"x"}"#.to_owned(),
        r#"{"type":"response.code_interpreter_call_code.done","item_id":"c","output_index":0,"code":"x"}"#.to_owned(),
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"c","output_index":0,"delta":"x"}"#.to_owned(),
        r#"{"type":"response.custom_tool_call_input.done","item_id":"c","output_index":0,"input":"x"}"#.to_owned(),
        r#"{"type":"response.mcp_call.executing","item_id":"c","output_index":0}"#.to_owned(),
        r#"{"type":"response.mcp_call_arguments.delta","item_id":"c","output_index":0,"delta":"x"}"#.to_owned(),
        r#"{"type":"response.mcp_call_arguments.done","item_id":"c","output_index":0}"#.to_owned(),
        r#"{"type":"response.mcp_call_output.delta","item_id":"c","output_index":0,"delta":"x"}"#.to_owned(),
        r#"{"type":"response.mcp_call_output.done","item_id":"c","output_index":0}"#.to_owned(),
        completed("completed", USAGE),
    ])]);
}

#[test]
fn finish_reasons_and_usage() {
    run("xai status other", "grok-4.3", jobj! {}, vec![sse(&[completed("cancelled", USAGE)])]);
    run("xai no usage or completion", "grok-4.3", jobj! {}, vec![sse(&[created()])]);
    run("xai empty stream", "grok-4.3", jobj! {}, vec![Reply::sse_raw("")]);
    run("xai done alias", "grok-4.3", jobj! {}, vec![sse(&[format!(r#"{{"type":"response.done","response":{{"object":"response","output":[],"status":"completed","usage":{USAGE}}}}}"#)])]);
    run("xai cached on top", "grok-4.3", jobj! {}, vec![sse(&[completed("completed", r#"{"input_tokens":2,"output_tokens":1,"input_tokens_details":{"cached_tokens":8}}"#)])]);
    run("xai bare usage", "grok-4.3", jobj! {}, vec![sse(&[completed("completed", r#"{"input_tokens":2,"output_tokens":1,"cost_in_usd_ticks":null}"#)])]);
    for (name, details) in [
        ("max tokens", r#"{"reason":"max_output_tokens"}"#),
        ("filter", r#"{"reason":"content_filter"}"#),
        ("other", r#"{"reason":"whatever"}"#),
        ("null", "null"),
    ] {
        run(&format!("xai incomplete {name}"), "grok-4.3", jobj! {}, vec![sse(&[
            r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"content_index":0,"delta":"cut"}"#.to_owned(),
            format!(r#"{{"type":"response.incomplete","response":{{"incomplete_details":{details},"usage":{USAGE},"service_tier":"priority"}}}}"#),
        ])]);
    }
}

#[test]
fn errors_and_classification() {
    run("xai 429", "grok-4.3", jobj! {}, vec![Reply::json(429, r#"{"error":{"message":"slow down","type":"rate_limit"}}"#).header("retry-after", "7")]);
    run("xai responses shape", "grok-4.3", jobj! {}, vec![Reply::json(400, r#"{"code":"invalid_argument","error":"bad field"}"#)]);
    run("xai bare error", "grok-4.3", jobj! {}, vec![Reply::json(403, r#"{"error":"forbidden"}"#)]);
    run("xai numeric code bare error", "grok-4.3", jobj! {}, vec![Reply::json(403, r#"{"code":403,"error":"forbidden"}"#)]);
    run("xai bad type falls back", "grok-4.3", jobj! {}, vec![Reply::json(400, r#"{"error":{"message":"m","type":1}}"#)]);
    run("xai other shape", "grok-4.3", jobj! {}, vec![Reply::json(422, r#"{"message":"flat"}"#)]);
    run("xai 500 empty", "grok-4.3", jobj! {}, vec![Reply::new(500)]);
    run("xai 503 html", "grok-4.3", jobj! {}, vec![Reply::new(503).body("<html>")]);
    for code in ["rate_limit_exceeded", "insufficient_quota", "api_error", "overloaded_error", "timeout", "invalid_api_key", "permission_error", "model_not_found", "context_length_exceeded", "503", "404", "unknown_code", "please_try_again"] {
        run(&format!("xai stream error {code}"), "grok-4.3", jobj! {}, vec![sse(&[format!(r#"{{"type":"error","code":"{code}","message":"m"}}"#)])]);
        run(&format!("xai failed {code}"), "grok-4.3", jobj! {}, vec![sse(&[
            created(),
            format!(r#"{{"type":"response.failed","response":{{"error":{{"code":"{code}","message":"m"}},"usage":{USAGE}}}}}"#),
        ])]);
    }
    run("xai stream error no code", "grok-4.3", jobj! {}, vec![sse(&[r#"{"type":"error","message":"Overloaded","code":null,"param":null}"#.to_owned()])]);
    run("xai failed no error", "grok-4.3", jobj! {}, vec![sse(&[r#"{"type":"response.failed","response":{}}"#.to_owned()])]);
    run("xai failed with reason", "grok-4.3", jobj! {}, vec![sse(&[r#"{"type":"response.failed","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#.to_owned()])]);
    run("xai truncated", "grok-4.3", jobj! {}, vec![Reply::sse_raw(&format!("data: {}\n\n", created())).truncated()]);
    differential_normalized("xai bad json", Some("/v1"), base("grok-4.3", jobj! {}), vec![Reply::sse_raw("data: {not json\n\n")], &without_zod_report);
    for (name, event) in [
        ("unknown event", r#"{"type":"response.brand_new"}"#),
        ("unknown item", r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"brand_new","id":"x","status":"s"}}"#),
        ("created without object", r#"{"type":"response.created","response":{"output":[]}}"#),
        ("completed without status", r#"{"type":"response.completed","response":{"object":"response","output":[]}}"#),
        ("delta without index", r#"{"type":"response.output_text.delta","item_id":"m","delta":"x"}"#),
        ("function call without name", r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"f","call_id":"c","arguments":""}}"#),
        ("usage string", r#"{"type":"response.completed","response":{"object":"response","output":[],"status":"completed","usage":{"input_tokens":"1","output_tokens":1}}}"#),
        ("infinite count", r#"{"type":"response.completed","response":{"object":"response","output":[],"status":"completed","usage":{"input_tokens":1e400,"output_tokens":1}}}"#),
        ("scalar", "5"),
    ] {
        differential_normalized(&format!("xai invalid {name}"), Some("/v1"), base("grok-4.3", jobj! {}), vec![Reply::sse(&[event])], &without_zod_report);
    }
}

#[test]
fn prompt_forms_and_step_loop() {
    run("xai foreign reasoning marks", "grok-4.3", jobj! { "messages": vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "t", "providerOptions": jobj! { "mewrk": jobj! { "model": "other" }, "xai": jobj! { "itemId": "rs" } } },
            jobj! { "type": "text", "text": "a" },
        ] },
        jobj! { "role": "assistant", "content": "plain" },
        jobj! { "role": "user", "content": vec![jobj! { "type": "image", "image": PNG, "mediaType": "image/png" }] },
    ] }, vec![text_stream()]);
    // A URL of a kind the model does not take is downloaded first, and a loopback one is refused.
    run("xai video url refused", "grok-4.3", jobj! { "messages": vec![jobj! { "role": "user", "content": vec![
        jobj! { "type": "file", "data": jobj! { "type": "url", "url": "http://127.0.0.1:9/x.mp4" }, "mediaType": "video/mp4" },
    ] }] }, vec![text_stream()]);
    run("xai appended tools", "grok-4.3", jobj! {
        "toolAppend": true,
        "messages": vec![
            jobj! { "role": "user", "content": "q" },
            jobj! { "role": "system", "content": "", "providerOptions": jobj! { "mewrk": jobj! { "toolAddition": vec![Json::from("ls")] } } },
        ],
    }, vec![text_stream()]);
    run("xai system append", "grok-4.3", jobj! {
        "systemAppend": true,
        "messages": vec![
            jobj! { "role": "user", "content": "q" },
            jobj! { "role": "system", "content": "plan", "providerOptions": jobj! { "mewrk": jobj! { "systemAppend": true } } },
        ],
    }, vec![text_stream()]);
    // The step loop around a server tool, with more steps allowed than the search needs.
    run("xai search with steps to spare", "grok-4.3", jobj! { "maxSteps": 4u64, "tools": Vec::<Json>::new(), "nativeSearch": jobj! { "maxUses": 2u64 } }, vec![sse(&[
        created(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed","name":"web_search","arguments":"{\"query\":\"q\"}"}}"#.to_owned(),
        r#"{"type":"response.output_text.delta","item_id":"m","output_index":1,"content_index":0,"delta":"found"}"#.to_owned(),
        completed("completed", USAGE),
    ]), text_stream()]);
    run("xai search and a host tool", "grok-4.3", jobj! { "maxSteps": 4u64, "nativeSearch": jobj! { "maxUses": 2u64 } }, vec![sse(&[
        created(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed","name":"web_search"}}"#.to_owned(),
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"f","call_id":"c","name":"ls","arguments":""}}"#.to_owned(),
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"f","call_id":"c","name":"ls","arguments":"{}"}}"#.to_owned(),
        completed("completed", USAGE),
    ]), text_stream()]);
}
