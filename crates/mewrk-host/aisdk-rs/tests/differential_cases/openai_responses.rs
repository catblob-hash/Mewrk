use mewrk_aisdk::jobj;
use mewrk_aisdk::json::Json;

use super::{differential, differential_normalized};
use crate::common::Reply;

fn base(family: &str, model: &str, extra: Json) -> Json {
    let key = if family == "azure" { "azure" } else { "openai" };
    let mut payload = jobj! {
        "family": family,
        "apiKey": "sk-test-xyz",
        "modelId": model,
        "system": "S",
        "messages": vec![jobj! { "role": "user", "content": "hello" }],
        "tools": vec![jobj! { "name": "read", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "path": jobj! { "type": "string" } } } }],
        "maxSteps": 1u64,
    };
    if let Some(object) = payload.as_obj_mut() {
        let mut options = mewrk_aisdk::json::Obj::new();
        options.insert(key, jobj! { "store": false, "reasoningSummary": Json::Null, "include": vec![Json::from("reasoning.encrypted_content")], "promptCacheKey": "conv" });
        object.insert("providerOptions", Json::Obj(options));
    }
    if let (Some(object), Some(extra)) = (payload.as_obj_mut(), extra.as_obj()) {
        object.extend_from(extra);
    }
    payload
}

fn fixture(name: &str) -> Reply {
    let path = format!("{}/../aisdk-service/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    Reply::sse_raw(&std::fs::read_to_string(path).expect("fixture"))
}

const COMPLETED: &str = r#"{"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}}"#;

fn text_stream() -> Reply {
    Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r1","created_at":1,"model":"gpt-5-real"}}"#,
        r#"{"type":"response.in_progress","response":{"id":"r1","created_at":1,"model":"gpt-5-real"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m1"}}"#,
        r#"{"type":"response.content_part.added","item_id":"m1","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}"#,
        r#"{"type":"response.output_text.delta","item_id":"m1","output_index":0,"delta":"hi"}"#,
        r#"{"type":"response.output_text.delta","item_id":"m1","output_index":0,"delta":" there"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"m1"}}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":2}},"service_tier":"default"}}"#,
    ])
}

fn tool_stream() -> Reply {
    Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r1","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs1","encrypted_content":null}}"#,
        r#"{"type":"response.reasoning_summary_part.added","item_id":"rs1","output_index":0,"summary_index":0}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs1","output_index":0,"summary_index":0,"delta":"think"}"#,
        r#"{"type":"response.reasoning_summary_part.done","item_id":"rs1","output_index":0,"summary_index":0}"#,
        r#"{"type":"response.reasoning_summary_part.added","item_id":"rs1","output_index":0,"summary_index":1}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs1","output_index":0,"summary_index":1,"delta":"more"}"#,
        r#"{"type":"response.reasoning_summary_part.done","item_id":"rs1","output_index":0,"summary_index":1}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs1","encrypted_content":"ENC"}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc1","call_id":"c1","name":"read","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc1","output_index":1,"delta":"{\"path\":"}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc1","output_index":1,"delta":"\"a\"}"}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"fc1","output_index":1,"arguments":"{\"path\":\"a\"}"}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc1","call_id":"c1","name":"read","arguments":"{\"path\":\"a\"}","status":"completed"}}"#,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":9,"output_tokens_details":{"reasoning_tokens":5}}}}"#,
    ])
}

fn replay_history() -> Vec<Json> {
    vec![
        jobj! { "role": "user", "content": vec![
            jobj! { "type": "text", "text": "look at this", "providerOptions": jobj! { "openai": jobj! { "promptCacheBreakpoint": jobj! { "mode": "explicit" } } } },
            jobj! { "type": "file", "data": "iVBORw0KGgo=", "mediaType": "image/png", "providerOptions": jobj! { "openai": jobj! { "imageDetail": "low" } } },
            jobj! { "type": "file", "data": "JVBERi0=", "mediaType": "application/pdf" },
            jobj! { "type": "file", "data": "JVBERi0=", "mediaType": "application/pdf", "filename": "named.pdf" },
            jobj! { "type": "file", "data": "https://X.example/a b.png", "mediaType": "image/png" },
            jobj! { "type": "file", "data": "HTTP://x.example:80", "mediaType": "application/pdf" },
            jobj! { "type": "file", "data": "file-abc", "mediaType": "image/png" },
            jobj! { "type": "file", "data": "file-def", "mediaType": "application/pdf" },
            jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "openai": "file-ref", "azure": "assistant-ref" } }, "mediaType": "image/png" },
            jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "openai": "file-ref2", "azure": "assistant-ref2" } }, "mediaType": "application/pdf" },
        ] },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "first", "providerOptions": jobj! { "openai": jobj! { "itemId": "rs_1", "reasoningEncryptedContent": Json::Null } } },
            jobj! { "type": "reasoning", "text": "second", "providerOptions": jobj! { "openai": jobj! { "itemId": "rs_1", "reasoningEncryptedContent": "ENC" } } },
            jobj! { "type": "reasoning", "text": "", "providerOptions": jobj! { "openai": jobj! { "itemId": "rs_2", "reasoningEncryptedContent": "ENC2" } } },
            jobj! { "type": "reasoning", "text": "no enc", "providerOptions": jobj! { "openai": jobj! { "itemId": "rs_3" } } },
            jobj! { "type": "reasoning", "text": "no id", "providerOptions": jobj! { "openai": jobj! { "reasoningEncryptedContent": "ENC4" } } },
            jobj! { "type": "reasoning", "text": "foreign" },
            jobj! { "type": "text", "text": "a", "providerOptions": jobj! { "openai": jobj! { "itemId": "m_1", "phase": "commentary" } } },
            jobj! { "type": "text", "text": "plain" },
            jobj! { "type": "tool-call", "toolCallId": "ws", "toolName": "web_search", "input": jobj! {}, "providerExecuted": true },
            jobj! { "type": "tool-result", "toolCallId": "ws", "toolName": "web_search", "output": jobj! { "type": "json", "value": jobj! { "action": jobj! { "type": "search", "query": "q" } } } },
            jobj! { "type": "tool-call", "toolCallId": "c0", "toolName": "read", "input": jobj! { "path": "x" }, "providerOptions": jobj! { "openai": jobj! { "itemId": "fc_0", "namespace": "ns", "caller": jobj! { "type": "program", "callerId": "p1" } } } },
            jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "read", "input": jobj! { "path": "y" } },
            jobj! { "type": "tool-call", "toolCallId": "c2", "toolName": "read", "input": jobj! { "path": "z" } },
            jobj! { "type": "tool-call", "toolCallId": "c3", "toolName": "read", "input": jobj! { "path": "w" } },
            jobj! { "type": "tool-call", "toolCallId": "c4", "toolName": "read", "input": jobj! { "path": "v" } },
        ] },
        jobj! { "role": "tool", "content": vec![
            jobj! { "type": "tool-result", "toolCallId": "c0", "toolName": "read", "output": jobj! { "type": "content", "value": vec![
                jobj! { "type": "text", "text": "body" },
                jobj! { "type": "file", "data": jobj! { "type": "data", "data": "iVBORw0KGgo=" }, "mediaType": "image/png" },
                jobj! { "type": "file", "data": jobj! { "type": "data", "data": "JVBERi0=" }, "mediaType": "application/pdf", "filename": "f.pdf" },
                jobj! { "type": "file", "data": jobj! { "type": "data", "data": "JVBERi0=" }, "mediaType": "application/pdf" },
                jobj! { "type": "image-url", "url": "https://X.example/b c.png" },
                jobj! { "type": "file-url", "url": "https://x.example/b.pdf", "mediaType": "application/pdf" },
                jobj! { "type": "file-id", "fileId": "file-1" },
                jobj! { "type": "custom" },
            ] }, "providerOptions": jobj! { "openai": jobj! { "caller": jobj! { "type": "program", "callerId": "p1" } } } },
            jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "read", "output": jobj! { "type": "text", "value": "plain" } },
            jobj! { "type": "tool-result", "toolCallId": "c2", "toolName": "read", "output": jobj! { "type": "json", "value": jobj! { "z": 1u64, "a": 2u64 } } },
            jobj! { "type": "tool-result", "toolCallId": "c3", "toolName": "read", "output": jobj! { "type": "error-text", "value": "bad" } },
            jobj! { "type": "tool-result", "toolCallId": "c4", "toolName": "read", "output": jobj! { "type": "execution-denied", "reason": "no" } },
        ] },
        jobj! { "role": "user", "content": "again" },
    ]
}

fn compact_generated_ids(text: &str) -> String {
    let re = regex::Regex::new(r#""toolCallId":"[0-9A-Za-z]{16}""#).unwrap();
    re.replace_all(text, r#""toolCallId":"<generated>""#).into_owned()
}

#[test]
fn plain_text_all_families() {
    differential("responses text", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![text_stream()]);
    differential("responses trailing slash", Some("/v1/"), base("openai-responses", "gpt-4o", jobj! {}), vec![text_stream()]);
    differential("codex text", Some("/backend-api/codex"), base("openai-codex", "gpt-5", jobj! { "maxOutputTokens": 64u64, "headers": jobj! { "chatgpt-account-id": "a", "originator": "mewrk" } }), vec![text_stream()]);
    differential("azure gateway text", Some("/gw"), base("azure", "gpt-5", jobj! { "settings": jobj! { "apiVersion": "2025-04-01-preview" } }), vec![text_stream()]);
    differential("azure headers", Some("/gw"), base("azure", "dep-1", jobj! { "headers": jobj! { "api-key": "override", "x-extra": "1", "User-Agent": "custom/1" } }), vec![text_stream()]);
    differential("responses empty key", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "apiKey": "" }), vec![text_stream()]);
    differential("azure no key", Some("/gw"), base("azure", "gpt-5", jobj! { "apiKey": "" }), vec![text_stream()]);
    differential("responses auth override", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "headers": jobj! { "Authorization": "Bearer other", "x-a": "b" } }), vec![text_stream()]);
    differential("azure no base", None, base("azure", "gpt-5", jobj! {}), vec![text_stream()]);
    differential("no options at all", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "providerOptions": Json::Null, "reasoning": "high" }), vec![text_stream()]);
    differential("no options custom model", Some("/v1"), base("openai-responses", "custom-model", jobj! { "providerOptions": Json::Null, "reasoning": "high", "system": "sys" }), vec![text_stream()]);
    differential("no tools", Some("/v1"), base("openai-responses", "gpt-5.2", jobj! { "tools": Vec::<Json>::new(), "reasoning": "xhigh" }), vec![text_stream()]);
}

#[test]
fn reasoning_fixtures() {
    for mode in ["encrypted", "plaintext"] {
        differential(
            &format!("fixture reasoning {mode}"),
            Some("/v1"),
            base("openai-responses", "responses-reasoning", jobj! { "reasoning": "high", "reasoningContent": mode }),
            vec![fixture("openai-responses-reasoning.sse")],
        );
        differential(
            &format!("fixture encrypted only {mode}"),
            Some("/v1"),
            base("openai-responses", "responses-encrypted-only", jobj! { "reasoning": "high", "reasoningContent": mode }),
            vec![fixture("openai-responses-encrypted-only.sse")],
        );
        differential(
            &format!("fixture reasoning gpt {mode}"),
            Some("/v1"),
            base("openai-responses", "gpt-5", jobj! { "reasoning": "high", "reasoningContent": mode }),
            vec![fixture("openai-responses-reasoning.sse")],
        );
    }
    differential(
        "azure fixture",
        Some("/gw"),
        base("azure", "gpt-5", jobj! { "reasoning": "max", "reasoningContent": "encrypted" }),
        vec![fixture("openai-responses-reasoning.sse")],
    );
    differential(
        "codex fixture",
        Some("/backend-api/codex"),
        base("openai-codex", "gpt-5.6", jobj! { "reasoning": "max", "reasoningContent": "plaintext" }),
        vec![fixture("openai-responses-reasoning.sse")],
    );
}

#[test]
fn reasoning_summary_parts_and_tools() {
    differential("responses summary parts, not stored", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "reasoning": "medium", "reasoningContent": "encrypted" }), vec![tool_stream()]);
    let mut stored = base("openai-responses", "gpt-5", jobj! { "reasoning": "medium", "reasoningContent": "plaintext" });
    stored.as_obj_mut().unwrap().insert("providerOptions", jobj! { "openai": jobj! { "store": true } });
    differential("responses summary parts, stored", Some("/v1"), stored, vec![tool_stream()]);
    // Interleaved parts: a second item starts while the first still has one open, and ids rotate between events.
    let interleaved = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"a"}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"b","encrypted_content":"E-b"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rotated-a","output_index":0,"summary_index":0,"delta":"a0"}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"b","output_index":1,"summary_index":0,"delta":"b0"}"#,
        r#"{"type":"response.reasoning_summary_part.done","item_id":"rotated-a","output_index":0,"summary_index":0}"#,
        r#"{"type":"response.reasoning_summary_part.added","item_id":"rotated-a","output_index":0,"summary_index":2}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rotated-a","summary_index":2,"delta":"a2 no index"}"#,
        r#"{"type":"response.reasoning_summary_part.done","item_id":"b","output_index":1,"summary_index":0}"#,
        r#"{"type":"response.reasoning_summary_part.added","item_id":"b","output_index":1,"summary_index":1}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"b","output_index":1,"summary_index":1,"delta":"b1"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rotated-again","encrypted_content":"E-a"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"b","encrypted_content":"E-b2"}}"#,
        r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"reasoning","id":"never-started"}}"#,
        COMPLETED,
    ]);
    differential("interleaved summary parts", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "reasoning": "high", "reasoningContent": "plaintext" }), vec![interleaved]);
    // What a turn persisted goes back out.
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "think", "providerOptions": jobj! { "openai": jobj! { "itemId": "rs1" } } },
            jobj! { "type": "reasoning", "text": "more", "providerOptions": jobj! { "openai": jobj! { "itemId": "rs1", "reasoningEncryptedContent": "ENC" } } },
            jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "read", "input": jobj! { "path": "a" }, "providerOptions": jobj! { "openai": jobj! { "itemId": "fc1" } } },
        ] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "read", "output": jobj! { "type": "text", "value": "ok" } }] },
    ];
    differential("replay not stored", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "messages": history }), vec![text_stream()]);
}

#[test]
fn replayed_history_with_images_and_results() {
    differential("replay not stored", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "messages": replay_history() }), vec![text_stream()]);
    let mut stored = base("openai-responses", "gpt-5", jobj! { "messages": replay_history() });
    stored.as_obj_mut().unwrap().insert("providerOptions", jobj! { "openai": jobj! { "store": true } });
    differential("replay stored", Some("/v1"), stored, vec![text_stream()]);
    let mut default_store = base("openai-responses", "gpt-4o", jobj! { "messages": replay_history() });
    default_store.as_obj_mut().unwrap().remove("providerOptions");
    differential("replay default store", Some("/v1"), default_store, vec![text_stream()]);
    for (name, key, value) in [
        ("replay conversation", "conversation", "conv_1"),
        ("replay previous response", "previousResponseId", "resp_0"),
    ] {
        let mut options = mewrk_aisdk::json::Obj::new();
        options.insert("store", false);
        options.insert(key, value);
        let mut chained = base("openai-responses", "gpt-5", jobj! { "messages": replay_history() });
        chained.as_obj_mut().unwrap().insert("providerOptions", jobj! { "openai": Json::Obj(options.clone()) });
        differential(name, Some("/v1"), chained, vec![text_stream()]);
        options.insert("store", true);
        let mut chained = base("openai-responses", "gpt-5", jobj! { "messages": replay_history() });
        chained.as_obj_mut().unwrap().insert("providerOptions", jobj! { "openai": Json::Obj(options) });
        differential(name, Some("/v1"), chained, vec![text_stream()]);
    }
    // Azure reads `azure` options and has its own file id prefix; `openai` options are its fallback.
    let azure_history = vec![
        jobj! { "role": "user", "content": vec![
            jobj! { "type": "file", "data": "assistant-abcdefghijklmnop", "mediaType": "image/png", "providerOptions": jobj! { "azure": jobj! { "imageDetail": "high" }, "openai": jobj! { "imageDetail": "low" } } },
            jobj! { "type": "file", "data": "assistant-abcdefghijklmnoq", "mediaType": "application/pdf" },
            jobj! { "type": "file", "data": jobj! { "type": "reference", "reference": jobj! { "azure": "assistant-ref" } }, "mediaType": "application/pdf" },
        ] },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "reasoning", "text": "r", "providerOptions": jobj! { "azure": jobj! { "itemId": "rs", "reasoningEncryptedContent": "E" }, "openai": jobj! { "itemId": "wrong" } } },
            jobj! { "type": "text", "text": "t", "providerOptions": jobj! { "azure": jobj! { "itemId": "m", "phase": "final_answer" } } },
        ] },
        jobj! { "role": "user", "content": "next" },
    ];
    differential("azure replay", Some("/gw"), base("azure", "gpt-5", jobj! { "messages": azure_history.clone() }), vec![text_stream()]);
    let mut fallback = base("azure", "gpt-5", jobj! { "messages": azure_history });
    fallback.as_obj_mut().unwrap().insert("providerOptions", jobj! { "openai": jobj! { "store": false, "serviceTier": "flex" } });
    differential("azure openai fallback", Some("/gw"), fallback, vec![text_stream()]);
    // A file part the provider refuses.
    let refused = |data: Json, media: &str| jobj! { "messages": vec![jobj! { "role": "user", "content": vec![jobj! { "type": "file", "data": data, "mediaType": media }] }] };
    differential("refused media type", Some("/v1"), base("openai-responses", "gpt-5", refused(Json::from("aGVsbG8="), "text/plain")), vec![text_stream()]);
    differential("refused text file", Some("/v1"), base("openai-responses", "gpt-5", refused(jobj! { "type": "text", "text": "x" }, "text/plain")), vec![text_stream()]);
    differential("missing reference", Some("/v1"), base("openai-responses", "gpt-5", refused(jobj! { "type": "reference", "reference": jobj! { "other": "x", "more": "y" } }, "image/png")), vec![text_stream()]);
}

#[test]
fn parallel_wrapper() {
    let wrapper = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc","call_id":"cw","name":"parallel","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc","output_index":0,"delta":"{\"tool_uses\":[{\"recipient_name\":\"functions.read\",\"parameters\":{\"path\":\"a\"}},{\"recipient_name\":\"functions.read\",\"parameters\":{\"path\":\"b\"}}]}"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc","call_id":"cw","name":"parallel","arguments":"{\"tool_uses\":[{\"recipient_name\":\"functions.read\",\"parameters\":{\"path\":\"a\"}},{\"recipient_name\":\"functions.read\",\"parameters\":{\"path\":\"b\"}}]}","status":"completed"}}"#,
        COMPLETED,
    ]);
    differential("parallel expands", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![wrapper]);
    let unexpandable = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc","call_id":"cw","name":"parallel","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc","output_index":0,"delta":"{\"tool_uses\":[{\"recipient_name\":\"functions.gone\","}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc","output_index":0,"delta":"\"parameters\":{}}]}"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc","call_id":"cw","name":"parallel","arguments":"{\"tool_uses\":[{\"recipient_name\":\"functions.gone\",\"parameters\":{}}]}","status":"completed"}}"#,
        COMPLETED,
    ]);
    differential("parallel not expanded", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![unexpandable]);
    let cut = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc","call_id":"cw","name":"parallel","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc","output_index":0,"delta":"{\"tool_uses\""}"#,
    ]);
    differential("parallel cut off", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![cut]);
    let parallel_options = |index: u64| jobj! { "openai": jobj! { "parallelToolCall": jobj! { "itemId": "fc", "toolCallId": "cw", "toolName": "parallel", "input": "{}", "index": index, "count": 2u64 } } };
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "tool-call", "toolCallId": "cw_0", "toolName": "read", "input": jobj! { "path": "a" }, "providerOptions": parallel_options(0) },
            jobj! { "type": "tool-call", "toolCallId": "cw_1", "toolName": "read", "input": jobj! { "path": "b" }, "providerOptions": parallel_options(1) },
        ] },
        jobj! { "role": "tool", "content": vec![
            jobj! { "type": "tool-result", "toolCallId": "cw_1", "toolName": "read", "output": jobj! { "type": "text", "value": "B" }, "providerOptions": parallel_options(1) },
            jobj! { "type": "tool-result", "toolCallId": "cw_0", "toolName": "read", "output": jobj! { "type": "json", "value": jobj! { "a": 1u64 } }, "providerOptions": parallel_options(0) },
        ] },
    ];
    differential("parallel replay plain", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "messages": history.clone() }), vec![text_stream()]);
    for key in ["previousResponseId", "conversation"] {
        let mut options = mewrk_aisdk::json::Obj::new();
        options.insert("store", false);
        options.insert(key, "x");
        let mut chained = base("openai-responses", "gpt-5", jobj! { "messages": history.clone() });
        chained.as_obj_mut().unwrap().insert("providerOptions", jobj! { "openai": Json::Obj(options) });
        differential("parallel replay chained", Some("/v1"), chained, vec![text_stream()]);
    }
}

#[test]
fn provider_options() {
    let options = |value: Json| jobj! { "providerOptions": jobj! { "openai": value } };
    let run = |name: &str, model: &str, value: Json| {
        differential(name, Some("/v1"), base("openai-responses", model, options(value)), vec![text_stream()]);
    };
    run(
        "all options",
        "gpt-5",
        jobj! {
            "store": false, "reasoningEffort": "low", "reasoningSummary": "auto", "reasoningMode": "pro", "reasoningContext": "auto",
            "include": vec![Json::from("file_search_call.results"), Json::from("reasoning.encrypted_content")],
            "conversation": Json::Null, "maxToolCalls": 3u64, "metadata": jobj! { "z": 1u64, "a": jobj! { "k": "v" } }, "parallelToolCalls": false,
            "previousResponseId": Json::Null, "user": "u", "instructions": "i", "serviceTier": "priority",
            "promptCacheKey": "k", "promptCacheOptions": jobj! { "ttl": "30m", "mode": "explicit", "unknown": 1u64 }, "promptCacheRetention": "24h",
            "safetyIdentifier": "s", "logprobs": true, "truncation": "auto", "textVerbosity": "low",
            "contextManagement": vec![jobj! { "compactThreshold": 100u64, "type": "compaction", "x": 1u64 }],
            "compactionTrigger": true,
        },
    );
    run("nulls stay", "gpt-5", jobj! {
        "store": Json::Null, "conversation": Json::Null, "maxToolCalls": Json::Null, "metadata": Json::Null, "parallelToolCalls": Json::Null,
        "previousResponseId": Json::Null, "user": Json::Null, "instructions": Json::Null, "serviceTier": Json::Null, "include": Json::Null,
        "promptCacheKey": Json::Null, "promptCacheRetention": Json::Null, "safetyIdentifier": Json::Null, "truncation": Json::Null,
        "reasoningEffort": Json::Null, "reasoningSummary": Json::Null, "contextManagement": Json::Null, "textVerbosity": Json::Null,
    });
    run("logprobs number", "gpt-4o", jobj! { "logprobs": 5u64, "include": Json::Null });
    run("service tier flex refused", "gpt-4o", jobj! { "serviceTier": "flex" });
    run("service tier flex taken", "gpt-5", jobj! { "serviceTier": "flex" });
    run("service tier fast refused", "gpt-5-nano", jobj! { "serviceTier": "fast" });
    run("service tier priority taken", "gpt-4o", jobj! { "serviceTier": "priority" });
    run("service tier default", "gpt-3.5", jobj! { "serviceTier": "default" });
    run("reasoning on a non-reasoning model", "gpt-4o", jobj! { "reasoningEffort": "low", "reasoningSummary": "auto", "store": false });
    run("force reasoning", "my-proxy", jobj! { "forceReasoning": true, "reasoningEffort": "low", "store": false });
    run("force no reasoning", "gpt-5", jobj! { "forceReasoning": false, "reasoningEffort": "low", "store": false });
    run("effort none", "gpt-5.1", jobj! { "reasoningEffort": "none" });
    run("summary null with effort", "gpt-5", jobj! { "reasoningEffort": "high", "reasoningSummary": Json::Null });
    run("system mode system", "gpt-5", jobj! { "systemMessageMode": "system" });
    run("system mode remove", "gpt-5", jobj! { "systemMessageMode": "remove" });
    run("system mode developer", "gpt-4o", jobj! { "systemMessageMode": "developer" });
    run("allowed tools", "gpt-5", jobj! { "allowedTools": jobj! { "toolNames": vec![Json::from("read"), Json::from("nope")], "mode": "required" } });
    run("pass through files", "gpt-5", jobj! { "passThroughUnsupportedFiles": true });
    run("bad include", "gpt-5", jobj! { "include": vec![Json::from("web_search_call.action.sources")] });
    run("bad tier", "gpt-5", jobj! { "serviceTier": "nope" });
    run("null where optional", "gpt-5", jobj! { "forceReasoning": Json::Null });
    run("unknown key ignored", "gpt-5", jobj! { "whatever": 1u64 });
    differential("options not an object", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "providerOptions": jobj! { "openai": "x" } }), vec![text_stream()]);
    differential("pass through file taken", Some("/v1"), base("openai-responses", "gpt-5", jobj! {
        "providerOptions": jobj! { "openai": jobj! { "passThroughUnsupportedFiles": true } },
        "messages": vec![jobj! { "role": "user", "content": vec![jobj! { "type": "file", "data": "aGVsbG8=", "mediaType": "text/plain" }] }],
    }), vec![text_stream()]);
    let reasoning_bad = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "reasoning", "text": "t", "providerOptions": jobj! { "openai": jobj! { "itemId": 7u64 } } }] },
        jobj! { "role": "user", "content": "q2" },
    ];
    differential("bad reasoning options", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "messages": reasoning_bad }), vec![text_stream()]);
    // A tool choice cannot be set from a frame except through the search budget, covered with search below.
}

fn web_search_stream() -> Reply {
    Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws1","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws1","status":"completed","action":{"type":"search","query":"q","queries":["q","r"],"sources":[{"url":"https://a.example/","type":"url","x":1},{"type":"api","name":"oai"},{"type":"url","url":"file:///nope"}]}}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"web_search_call","id":"ws2","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"web_search_call","id":"ws2","status":"completed","action":{"type":"open_page","url":"https://b.example/"}}}"#,
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"web_search_call","id":"ws3","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"web_search_call","id":"ws3","status":"completed","action":{"type":"find_in_page","url":"https://c.example/","pattern":"p"}}}"#,
        r#"{"type":"response.output_item.added","output_index":3,"item":{"type":"web_search_call","id":"ws4","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"web_search_call","id":"ws4","status":"completed"}}"#,
        r#"{"type":"response.output_item.added","output_index":4,"item":{"type":"message","id":"m0","phase":"commentary"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"m0","output_index":4,"delta":"PROCESS"}"#,
        r#"{"type":"response.output_text.annotation.added","output_index":4,"item_id":"m0","annotation":{"type":"url_citation","url":"https://cited.example/","title":"T","start_index":0,"end_index":3}}"#,
        r#"{"type":"response.output_item.done","output_index":4,"item":{"type":"message","id":"m0","phase":"commentary"}}"#,
        r#"{"type":"response.output_item.added","output_index":5,"item":{"type":"message","id":"m1"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"m1","output_index":5,"delta":"ANSWER"}"#,
        r#"{"type":"response.output_text.annotation.added","output_index":5,"item_id":"m1","annotation":{"type":"file_citation","file_id":"f","filename":"n.txt","index":1}}"#,
        r#"{"type":"response.output_text.annotation.added","output_index":5,"item_id":"m1","annotation":{"type":"container_file_citation","container_id":"c","file_id":"f2","filename":"n2.txt","start_index":0,"end_index":1}}"#,
        r#"{"type":"response.output_text.annotation.added","output_index":5,"item_id":"m1","annotation":{"type":"file_path","file_id":"f3","index":2}}"#,
        r#"{"type":"response.output_item.done","output_index":5,"item":{"type":"message","id":"m1","phase":"final_answer"}}"#,
        COMPLETED,
    ])
}

#[test]
fn native_search_and_phases() {
    let search = jobj! { "nativeSearch": jobj! { "maxUses": 3u64 } };
    differential("search openai", Some("/v1"), base("openai-responses", "gpt-5", search.clone()), vec![web_search_stream()]);
    differential("search azure", Some("/gw"), base("azure", "gpt-5", search.clone()), vec![web_search_stream()]);
    differential("search codex", Some("/backend-api/codex"), base("openai-codex", "gpt-5", search), vec![web_search_stream()]);
    differential("search without native", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![web_search_stream()]);
    let exhausted = jobj! { "nativeSearch": jobj! { "maxUses": 1u64, "previousCallIds": vec![Json::from("old")] } };
    let mut history = base("openai-responses", "gpt-5", exhausted);
    let messages = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "tool-call", "toolCallId": "old2", "toolName": "web_search", "input": jobj! {}, "providerExecuted": true },
            jobj! { "type": "tool-result", "toolCallId": "old2", "toolName": "web_search", "output": jobj! { "type": "json", "value": jobj! {} } },
            jobj! { "type": "text", "text": "ok" },
        ] },
        jobj! { "role": "user", "content": "more" },
    ];
    history.as_obj_mut().unwrap().insert("messages", messages);
    differential("search budget and replay", Some("/v1"), history, vec![web_search_stream()]);
    for (name, phase) in [("late phase", ""), ("commentary only", "commentary")] {
        let added = format!(r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"message","id":"m","phase":{}}}}}"#, if phase.is_empty() { "null".to_owned() } else { format!("\"{phase}\"") });
        let done = format!(r#"{{"type":"response.output_item.done","output_index":0,"item":{{"type":"message","id":"m","phase":"{}"}}}}"#, if phase.is_empty() { "final_answer" } else { phase });
        let reply = Reply::sse(&[
            r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
            added.as_str(),
            r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"x"}"#,
            done.as_str(),
            COMPLETED,
        ]);
        differential(name, Some("/v1"), base("openai-responses", "gpt-5", jobj! { "nativeSearch": jobj! { "maxUses": 3u64 } }), vec![reply.clone()]);
        differential(name, Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![reply]);
    }
}

#[test]
fn annotations_persist_from_the_one_live_array() {
    // Upstream's parts share the array, so a part's persisted annotations are that array's state at the end of the step.
    let one = |first: &'static str, second: &'static str| {
        let mut events: Vec<&str> = vec![
            r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"a"}}"#,
            r#"{"type":"response.output_text.delta","item_id":"a","output_index":0,"delta":"x"}"#,
        ];
        if !first.is_empty() {
            events.push(first);
        }
        events.push(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"a"}}"#);
        events.push(r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"b"}}"#);
        events.push(r#"{"type":"response.output_text.delta","item_id":"b","output_index":1,"delta":"y"}"#);
        if !second.is_empty() {
            events.push(second);
        }
        events.push(r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"b"}}"#);
        events.push(COMPLETED);
        Reply::sse(&events)
    };
    let citation_a = r#"{"type":"response.output_text.annotation.added","annotation":{"type":"url_citation","url":"https://a.example/","title":"A","start_index":0,"end_index":1}}"#;
    let citation_b = r#"{"type":"response.output_text.annotation.added","annotation":{"type":"url_citation","url":"https://b.example/","title":"B","start_index":0,"end_index":1}}"#;
    differential("annotations on both", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![one(citation_a, citation_b)]);
    differential("annotations on the first", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![one(citation_a, "")]);
    differential("annotations on the second", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![one("", citation_b)]);
}

#[test]
fn compaction_items() {
    let compaction = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"compaction","id":"cp1"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","id":"cp1","encrypted_content":"CP-ENC"}}"#,
        COMPLETED,
    ]);
    let trigger = jobj! { "providerOptions": jobj! { "openai": jobj! { "store": false, "compactionTrigger": true } } };
    differential("compaction request and part", Some("/backend-api/codex"), base("openai-codex", "gpt-5", trigger), vec![compaction.clone()]);
    differential("azure compaction part", Some("/gw"), base("azure", "gpt-5", jobj! { "providerOptions": jobj! { "azure": jobj! { "store": false, "compactionTrigger": true } } }), vec![compaction]);
    let history = |store: bool| {
        jobj! {
            "providerOptions": jobj! { "openai": jobj! { "store": store } },
            "messages": vec![
                jobj! { "role": "user", "content": "q" },
                jobj! { "role": "assistant", "content": vec![
                    jobj! { "type": "custom", "kind": "openai.compaction", "providerOptions": jobj! { "openai": jobj! { "type": "compaction", "itemId": "cp1", "encryptedContent": "CP-ENC" } } },
                    jobj! { "type": "custom", "kind": "other.kind", "providerOptions": jobj! { "openai": jobj! { "itemId": "x" } } },
                ] },
                jobj! { "role": "user", "content": "next" },
            ],
        }
    };
    differential("compaction replay", Some("/v1"), base("openai-responses", "gpt-5", history(false)), vec![text_stream()]);
    differential("compaction replay stored", Some("/v1"), base("openai-responses", "gpt-5", history(true)), vec![text_stream()]);
}

#[test]
fn every_other_output_item() {
    let reply = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","id":"ct","call_id":"cc","name":"read","input":""}}"#,
        r#"{"type":"response.custom_tool_call_input.delta","item_id":"ct","output_index":0,"delta":"raw"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"custom_tool_call","id":"ct","call_id":"cc","name":"read","input":"raw","status":"completed"}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"code_interpreter_call","id":"ci","container_id":"box","code":null,"outputs":null,"status":"in_progress"}}"#,
        r#"{"type":"response.code_interpreter_call_code.delta","item_id":"ci","output_index":1,"delta":"pri\"nt"}"#,
        r#"{"type":"response.code_interpreter_call_code.done","item_id":"ci","output_index":1,"code":"pri\"nt"}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"code_interpreter_call","id":"ci","container_id":"box","code":"x","outputs":[{"type":"logs","logs":"l"}]}}"#,
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"file_search_call","id":"fs"}}"#,
        r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"file_search_call","id":"fs","queries":["q"],"results":[{"attributes":{"a":1},"file_id":"f","filename":"n","score":0.5,"text":"t"}]}}"#,
        r#"{"type":"response.output_item.added","output_index":3,"item":{"type":"image_generation_call","id":"ig"}}"#,
        r#"{"type":"response.image_generation_call.partial_image","item_id":"ig","output_index":3,"partial_image_b64":"AA=="}"#,
        r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"image_generation_call","id":"ig","result":"AAA="}}"#,
        r#"{"type":"response.output_item.added","output_index":4,"item":{"type":"apply_patch_call","id":"ap","call_id":"apc","status":"in_progress","operation":{"type":"create_file","path":"a\"b","diff":""}}}"#,
        r#"{"type":"response.apply_patch_call_operation_diff.delta","item_id":"ap","output_index":4,"delta":"+x\n"}"#,
        r#"{"type":"response.apply_patch_call_operation_diff.done","item_id":"ap","output_index":4,"diff":"+x\n"}"#,
        r#"{"type":"response.output_item.done","output_index":4,"item":{"type":"apply_patch_call","id":"ap","call_id":"apc","status":"completed","operation":{"type":"create_file","path":"a\"b","diff":"+x\n"}}}"#,
        r#"{"type":"response.output_item.added","output_index":5,"item":{"type":"apply_patch_call","id":"ap2","call_id":"apd","status":"in_progress","operation":{"type":"delete_file","path":"gone"}}}"#,
        r#"{"type":"response.output_item.done","output_index":5,"item":{"type":"apply_patch_call","id":"ap2","call_id":"apd","status":"completed","operation":{"type":"delete_file","path":"gone"}}}"#,
        r#"{"type":"response.output_item.added","output_index":6,"item":{"type":"shell_call","id":"sh","call_id":"shc","status":"in_progress","action":{"commands":[]}}}"#,
        r#"{"type":"response.output_item.done","output_index":6,"item":{"type":"shell_call","id":"sh","call_id":"shc","status":"completed","action":{"commands":["ls"]}}}"#,
        r#"{"type":"response.output_item.added","output_index":7,"item":{"type":"shell_call_output","id":"sho","call_id":"shc","status":"in_progress","output":[]}}"#,
        r#"{"type":"response.output_item.done","output_index":7,"item":{"type":"shell_call_output","id":"sho","call_id":"shc","status":"completed","output":[{"stdout":"o","stderr":"e","outcome":{"type":"exit","exit_code":0}},{"stdout":"","stderr":"","outcome":{"type":"timeout"}}]}}"#,
        r#"{"type":"response.output_item.added","output_index":8,"item":{"type":"local_shell_call","id":"ls","call_id":"lsc","action":{"type":"exec","command":["ls"]}}}"#,
        r#"{"type":"response.output_item.done","output_index":8,"item":{"type":"local_shell_call","id":"ls","call_id":"lsc","action":{"type":"exec","command":["ls"],"timeout_ms":5,"working_directory":"/","env":{"A":"1"}}}}"#,
        r#"{"type":"response.output_item.added","output_index":9,"item":{"type":"computer_call","id":"cu","call_id":"cuc","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":9,"item":{"type":"computer_call","id":"cu","call_id":"cuc","status":"completed","actions":[{"type":"click","button":"left","x":1,"y":2,"keys":null},{"type":"scroll","x":1,"y":2,"scroll_x":3,"scroll_y":4,"keys":["a"]},{"type":"keypress","keys":["b"]},{"type":"drag","path":[{"x":1,"y":2}]}],"pending_safety_checks":[{"id":"s","code":null,"message":"m"}]}}"#,
        r#"{"type":"response.output_item.added","output_index":10,"item":{"type":"computer_call","id":"cu2","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":10,"item":{"type":"computer_call","id":"cu2","status":"completed","action":{"type":"wait"}}}"#,
        r#"{"type":"response.output_item.added","output_index":11,"item":{"type":"tool_search_call","id":"ts","execution":"server","call_id":null,"status":"in_progress","arguments":{}}}"#,
        r#"{"type":"response.output_item.done","output_index":11,"item":{"type":"tool_search_call","id":"ts","execution":"server","call_id":null,"status":"completed","arguments":{"q":1}}}"#,
        r#"{"type":"response.output_item.added","output_index":12,"item":{"type":"tool_search_output","id":"tso","execution":"server","call_id":null,"status":"in_progress","tools":[]}}"#,
        r#"{"type":"response.output_item.done","output_index":12,"item":{"type":"tool_search_output","id":"tso","execution":"server","call_id":null,"status":"completed","tools":[{"name":"t"}]}}"#,
        r#"{"type":"response.output_item.added","output_index":13,"item":{"type":"mcp_call","id":"mc","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":13,"item":{"type":"mcp_call","id":"mc","status":"completed","arguments":"{}","name":"tool","server_label":"srv","output":"o","error":{"message":"m","extra":1}}}"#,
        r#"{"type":"response.output_item.added","output_index":14,"item":{"type":"mcp_list_tools","id":"ml"}}"#,
        r#"{"type":"response.output_item.done","output_index":14,"item":{"type":"mcp_list_tools","id":"ml","server_label":"srv","tools":[]}}"#,
        r#"{"type":"response.output_item.added","output_index":15,"item":{"type":"program","id":"pg","call_id":"pgc","code":"c","fingerprint":"f"}}"#,
        r#"{"type":"response.output_item.done","output_index":15,"item":{"type":"program","id":"pg","call_id":"pgc","code":"c","fingerprint":"f"}}"#,
        r#"{"type":"response.output_item.done","output_index":16,"item":{"type":"program_output","id":"pgo","call_id":"pgc","result":"r","status":"completed"}}"#,
        r#"{"type":"response.output_item.done","output_index":17,"item":{"type":"brand_new_item","id":"x"}}"#,
        r#"{"type":"response.brand_new_event","x":1}"#,
        COMPLETED,
    ]);
    differential("every other item", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![reply]);
    let program_history = |store: bool| {
        jobj! {
            "providerOptions": jobj! { "openai": jobj! { "store": store } },
            "messages": vec![
                jobj! { "role": "user", "content": "q" },
                jobj! { "role": "assistant", "content": vec![
                    jobj! { "type": "tool-call", "toolCallId": "pgc", "toolName": "programmatic_tool_calling", "input": jobj! { "code": "c", "fingerprint": "f" }, "providerExecuted": true, "providerOptions": jobj! { "openai": jobj! { "itemId": "pg" } } },
                    jobj! { "type": "tool-result", "toolCallId": "pgc", "toolName": "programmatic_tool_calling", "output": jobj! { "type": "json", "value": jobj! { "result": "r", "status": "completed" } }, "providerOptions": jobj! { "openai": jobj! { "itemId": "pgo" } } },
                    jobj! { "type": "tool-call", "toolCallId": "fs", "toolName": "file_search", "input": jobj! {}, "providerExecuted": true, "providerOptions": jobj! { "openai": jobj! { "itemId": "fsi" } } },
                ] },
                jobj! { "role": "user", "content": "next" },
            ],
        }
    };
    differential("program replay", Some("/v1"), base("openai-responses", "gpt-5", program_history(false)), vec![text_stream()]);
    differential("program replay stored", Some("/v1"), base("openai-responses", "gpt-5", program_history(true)), vec![text_stream()]);
    let approval = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"mcp_call","id":"mc","status":"completed","arguments":"{}","name":"tool","server_label":"srv","approval_request_id":"from-prompt"}}"#,
        COMPLETED,
    ]);
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "dummy", "toolName": "read", "input": jobj! {}, "providerOptions": jobj! { "openai": jobj! { "approvalRequestId": "from-prompt" } } }] },
        jobj! { "role": "tool", "content": vec![
            jobj! { "type": "tool-result", "toolCallId": "dummy", "toolName": "read", "output": jobj! { "type": "execution-denied", "reason": "r", "providerOptions": jobj! { "openai": jobj! { "approvalId": "appr" } } } },
        ] },
    ];
    differential_normalized("mcp alias", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "messages": history }), vec![approval], &compact_generated_ids);
}

#[test]
fn finish_reasons_and_usage() {
    let finish = |event: &'static str| {
        Reply::sse(&[
            r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m"}}"#,
            r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"x"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"m"}}"#,
            event,
        ])
    };
    let run = |name: &str, event: &'static str| differential(name, Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![finish(event)]);
    run("length", r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":2}}}"#);
    run("filter", r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"usage":{"input_tokens":1,"output_tokens":2}}}"#);
    run("other", r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"something"},"usage":{"input_tokens":1,"output_tokens":2}}}"#);
    run("details null", r#"{"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":1,"output_tokens":2}}}"#);
    run("cache write", r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":2,"cache_write_tokens":3},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":1}},"reasoning":{"context":"ctx"}}}"#);
    run("nulls in usage", r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"input_tokens_details":null,"output_tokens":5,"output_tokens_details":{"reasoning_tokens":null}}}}"#);
    run("failed without error", r#"{"type":"response.failed","sequence_number":1,"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":2}}}"#);
    run("failed with nothing", r#"{"type":"response.failed","sequence_number":1,"response":{}}"#);
    let tools = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"f","call_id":"c","name":"read","arguments":"{}","status":"completed"}}"#,
        r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"something"},"usage":{"input_tokens":1,"output_tokens":2}}}"#,
    ]);
    differential("tool calls win over other", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![tools]);
    let unknown_tool = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"f","call_id":"c","name":"gone","arguments":""}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"f","call_id":"c","name":"gone","arguments":"not json","status":"completed"}}"#,
        COMPLETED,
    ]);
    differential("unknown tool", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![unknown_tool]);
    differential("no finish event", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![Reply::sse(&[r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#])]);
    differential("empty stream", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![Reply::sse_raw("data: [DONE]\n\n")]);
}

/// The validation detail is Zod's own issue list upstream: only the prefix with the value is comparable.
fn validation_detail_off(text: &str) -> String {
    let re = regex::Regex::new(r"\\nError message: .*").unwrap();
    re.replace_all(text, "<detail>").into_owned()
}

#[test]
fn errors() {
    let run = |name: &str, reply: Reply| differential(name, Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![reply]);
    run("429", Reply::json(429, r#"{"error":{"message":"slow","type":"rate_limit","code":"rate_limit_exceeded","param":null}}"#).header("retry-after", "7"));
    run("429 date", Reply::json(429, r#"{"error":{"message":"slow"}}"#).header("retry-after-ms", "100"));
    run("401", Reply::json(401, r#"{"error":{"message":"bad key","code":401}}"#));
    run("400 detail body", Reply::json(400, r#"{"detail":"Stream must be set to true"}"#));
    run("400 code object", Reply::json(400, r#"{"error":{"message":"m","code":{"a":1}}}"#));
    run("500 empty", Reply::new(500));
    run("503 text", Reply::new(503).body("upstream down"));
    run("404 html", Reply::new(404).header("content-type", "text/html").body("<html>"));
    let before = |event: &'static str| Reply::sse(&[r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#, event]);
    let early = [
        ("failed first", r#"{"type":"response.failed","sequence_number":1,"response":{"error":{"code":"rate_limit_exceeded","message":"slow down"}}}"#),
        ("failed server error", r#"{"type":"response.failed","sequence_number":1,"response":{"error":{"code":"server_error","message":"boom"}}}"#),
        ("failed no code", r#"{"type":"response.failed","sequence_number":1,"response":{"error":{"message":"boom"}}}"#),
        ("failed quota", r#"{"type":"response.failed","sequence_number":1,"response":{"error":{"code":"insufficient_quota","message":"pay"}}}"#),
        ("error nested quota", r#"{"type":"error","sequence_number":1,"error":{"type":"insufficient_quota","code":"insufficient_quota","message":"pay","param":null}}"#),
        ("error flat", r#"{"type":"error","sequence_number":1,"code":"server_error","message":"boom","param":null}"#),
        ("error flat invalid", r#"{"type":"error","sequence_number":1,"code":"invalid_prompt","message":"no"}"#),
        ("error flat status", r#"{"type":"error","sequence_number":1,"code":"503","message":"busy"}"#),
        ("error flat overloaded text", r#"{"type":"error","sequence_number":1,"message":"Overloaded"}"#),
    ];
    for (name, event) in early {
        run(name, Reply::sse(&[event]));
        run(name, before(event));
        run(name, before(event).header("retry-after", "3"));
    }
    let midstream = |event: &'static str| {
        Reply::sse(&[
            r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m"}}"#,
            r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"x"}"#,
            event,
            COMPLETED,
        ])
    };
    for (name, event) in early {
        run(name, midstream(event).header("retry-after", "3"));
    }
    // After `response.in_progress` an error flushed with it is still a failed call, and one that comes later is a
    // stream error: the two differ in that only the first carries Retry-After.
    let accepted = r#"{"type":"response.in_progress","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#;
    let quota = r#"{"type":"error","sequence_number":1,"code":"insufficient_quota","message":"pay"}"#;
    let mut flushed = Reply::new(200).header("content-type", "text/event-stream").header("retry-after", "3");
    flushed.chunks.push(format!("data: {accepted}\n\ndata: {quota}\n\n").into_bytes());
    run("in progress then error in one read", flushed);
    let late = Reply::sse(&[accepted, quota]).header("retry-after", "3").pause(std::time::Duration::from_millis(300));
    run("in progress then error after the grace", late);
    let chat = Reply::sse(&[r#"{"id":"c","object":"chat.completion.chunk","choices":[{"delta":{"content":"x"}}]}"#]).header("retry-after", "3");
    run("chat completions stream", chat);

    let invalid = [
        ("codex failed no sequence number", r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"rate_limit_exceeded","message":"slow down"}}}"#),
        ("error no sequence number", r#"{"type":"error","message":"m"}"#),
        ("completed without usage", r#"{"type":"response.completed","response":{}}"#),
        ("created without model", r#"{"type":"response.created","response":{"id":"r","created_at":1}}"#),
        ("delta without item", r#"{"type":"response.output_text.delta","delta":"x"}"#),
        ("item without type", r#"{"type":"response.output_item.added","output_index":0,"item":{}}"#),
        ("known item bad", r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#),
        ("bad phase", r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m","phase":"nope"}}"#),
        ("bad annotation", r#"{"type":"response.output_text.annotation.added","annotation":{"type":"new_kind"}}"#),
        ("type not string", r#"{"type":1}"#),
        ("array", r#"[1,2]"#),
    ];
    for (name, event) in invalid {
        differential_normalized(name, Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![Reply::sse(&[event])], &validation_detail_off);
    }
    differential_normalized(
        "not json",
        Some("/v1"),
        base("openai-responses", "gpt-5", jobj! {}),
        vec![Reply::sse_raw("data: {not json\n\n")],
        &validation_detail_off,
    );
}

#[test]
fn codex_transport() {
    let run = |name: &str, model: &str, reply: Reply| {
        differential(name, Some("/backend-api/codex"), base("openai-codex", model, jobj! { "maxOutputTokens": 64u64, "headers": jobj! { "chatgpt-account-id": "acct", "originator": "mewrk" } }), vec![reply]);
    };
    let mut no_content_type = Reply::new(200);
    no_content_type.chunks = text_stream().chunks;
    run("codex without content type", "gpt-5", no_content_type);
    run("codex 400", "gpt-5", Reply::json(400, r#"{"detail":"Stream must be set to true"}"#));
    let mut failed = Reply::new(200);
    failed.chunks = vec![br#"data: {"type":"response.failed","sequence_number":1,"response":{"status":"failed","error":{"code":"rate_limit_exceeded","message":"slow down"}}}

data: [DONE]

"#.to_vec()];
    run("codex failed", "gpt-5", failed);
    differential_normalized(
        "codex failed without sequence number",
        Some("/backend-api/codex"),
        base("openai-codex", "gpt-5", jobj! {}),
        vec![Reply::sse(&[r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"rate_limit_exceeded","message":"slow down"}}}"#])],
        &validation_detail_off,
    );
    // The default base URL cannot be reached from here, but a body with no `include` shows the dialect's own one.
    differential("codex no include", Some("/backend-api/codex"), base("openai-codex", "gpt-5", jobj! { "providerOptions": jobj! { "openai": jobj! { "store": true } } }), vec![text_stream()]);
}

#[test]
fn tool_append_and_async_tools() {
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "system", "content": "", "providerOptions": jobj! { "mewrk": jobj! { "toolAddition": vec![Json::from("read"), Json::from("gone")] } } },
        jobj! { "role": "user", "content": "more" },
    ];
    differential("append on", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "messages": history.clone(), "toolAppend": true }), vec![text_stream()]);
    differential("append on system role", Some("/v1"), base("openai-responses", "gpt-4o", jobj! { "messages": history.clone(), "toolAppend": true }), vec![text_stream()]);
    differential("append off", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "messages": history.clone() }), vec![text_stream()]);
    differential("append codex", Some("/backend-api/codex"), base("openai-codex", "gpt-5", jobj! { "messages": history, "toolAppend": true }), vec![text_stream()]);
    differential("async tools", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "asyncTools": vec![Json::from("read")] }), vec![tool_stream()]);
    let plaintext = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"deepseek"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs"}}"#,
        r#"{"type":"response.content_part.added","item_id":"rs","output_index":0,"content_index":0,"part":{"type":"reasoning_text"}}"#,
        r#"{"type":"response.reasoning_text.delta","item_id":"rs","output_index":0,"content_index":0,"delta":"think"}"#,
        r#"{"type":"response.content_part.done","item_id":"rs","output_index":0,"content_index":0,"part":{"type":"reasoning_text"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs","encrypted_content":"E"}}"#,
        COMPLETED,
    ]);
    differential("plaintext dialect", Some("/v1"), base("openai-responses", "deepseek-r1", jobj! { "reasoning": "high", "reasoningContent": "plaintext" }), vec![plaintext]);
}

#[test]
fn stream_shapes_a_relay_can_send() {
    let run = |name: &str, reply: Reply| differential(name, Some("/v1"), base("openai-responses", "gpt-5", jobj! { "reasoning": "high", "reasoningContent": "plaintext" }), vec![reply]);
    run("reasoning delta without a start", Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs","summary_index":0,"delta":"x"}"#,
        COMPLETED,
    ]));
    run("text delta without a start", Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"m","delta":"x"}"#,
        COMPLETED,
    ]));
    run("message ids repeat", Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"same"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"same","output_index":0,"delta":"a"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"same"}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"same"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"same","output_index":1,"delta":"b"}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"same"}}"#,
        COMPLETED,
    ]));
    run("ids rotate between events", Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"first"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"second","output_index":0,"delta":"a"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"third"}}"#,
        COMPLETED,
    ]));
    run("reasoning ids repeat across items", Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs","output_index":0,"summary_index":0,"delta":"a"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs"}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs","output_index":1,"summary_index":0,"delta":"b"}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs"}}"#,
        COMPLETED,
    ]));
    run("cut before output", Reply::sse(&[r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#]).truncated());
    run("cut after output", Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"x"}"#,
    ]).truncated());
    run("ends with no terminal event after text", Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"x"}"#,
    ]));
    run("forbidden key", Reply::sse(&[r#"{"type":"response.created","__proto__":{"a":1},"response":{"id":"r","created_at":1,"model":"gpt-5"}}"#]));
    run("event names and comments", Reply::sse_raw(": hi\n\nevent: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"created_at\":1,\"model\":\"gpt-5\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\ndata: [DONE]\n\n"));
}

#[test]
fn a_second_step_after_the_search_budget() {
    let first = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r1","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs","output_index":0,"summary_index":0,"delta":"plan"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs","encrypted_content":"ENC"}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"web_search_call","id":"ws","status":"in_progress"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"web_search_call","id":"ws","status":"completed","action":{"type":"search","query":"q","sources":[{"type":"url","url":"https://a.example/"}]}}}"#,
        r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"message","id":"m","phase":"commentary"}}"#,
        r#"{"type":"response.output_text.delta","item_id":"m","output_index":2,"delta":"working"}"#,
        r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"message","id":"m","phase":"commentary"}}"#,
        r#"{"type":"response.output_item.added","output_index":3,"item":{"type":"function_call","id":"f","call_id":"c","name":"gone","arguments":""}}"#,
        r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"function_call","id":"f","call_id":"c","name":"gone","arguments":"{}","status":"completed"}}"#,
        COMPLETED,
    ]);
    differential(
        "budget exhausted on the second step",
        Some("/v1"),
        base("openai-responses", "gpt-5", jobj! { "maxSteps": 3u64, "nativeSearch": jobj! { "maxUses": 1u64 } }),
        vec![first.clone(), text_stream()],
    );
    differential(
        "budget left on the second step",
        Some("/v1"),
        base("openai-responses", "gpt-5", jobj! { "maxSteps": 3u64, "nativeSearch": jobj! { "maxUses": 5u64 } }),
        vec![first.clone(), text_stream()],
    );
    differential("no search on the second step", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "maxSteps": 3u64 }), vec![first, text_stream()]);
}

#[test]
fn prompt_corners() {
    let tools_without_description = jobj! { "tools": vec![jobj! { "name": "a", "inputSchema": jobj! { "type": "object", "z": 1u64, "a": 2u64 } }] };
    differential("tool without description", Some("/v1"), base("openai-responses", "gpt-5", tools_without_description), vec![text_stream()]);
    let history = vec![
        jobj! { "role": "system", "content": "mid", "providerOptions": jobj! { "openai": jobj! { "promptCacheBreakpoint": jobj! { "mode": "explicit" } } } },
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "text", "text": "", "providerOptions": jobj! { "openai": jobj! { "itemId": "m" } } },
            jobj! { "type": "text", "text": "" },
            jobj! { "type": "text", "text": "x", "providerOptions": jobj! { "openai": jobj! { "itemId": Json::Null } } },
            jobj! { "type": "file", "data": "aGk=", "mediaType": "text/plain" },
        ] },
        jobj! { "role": "user", "content": "r" },
    ];
    for model in ["gpt-5", "gpt-4o"] {
        differential("system breakpoint and empty text", Some("/v1"), base("openai-responses", model, jobj! { "messages": history.clone() }), vec![text_stream()]);
    }
    differential("whitespace base url", None, base("openai-responses", "gpt-5", jobj! { "baseURL": "   " }), vec![text_stream()]);
    differential("azure whitespace base url", None, base("azure", "gpt-5", jobj! { "baseURL": "   " }), vec![text_stream()]);
    differential("azure bad base url", None, base("azure", "gpt-5", jobj! { "baseURL": "nonsense" }), vec![text_stream()]);
    let async_history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "read", "input": jobj! { "path": "a" } }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "read", "output": jobj! { "type": "text", "value": "started" } }] },
    ];
    differential("async tool replay", Some("/v1"), base("openai-responses", "gpt-5", jobj! { "messages": async_history, "asyncTools": vec![Json::from("read")] }), vec![text_stream()]);
}

#[test]
fn mcp_approval_request() {
    let approval = Reply::sse(&[
        r#"{"type":"response.created","response":{"id":"r","created_at":1,"model":"gpt-5"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"mcp_approval_request","id":"ma","server_label":"srv","name":"tool","arguments":"{}"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"mcp_approval_request","id":"mb","server_label":"srv","name":"tool","arguments":"{}","approval_request_id":"explicit"}}"#,
        r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"mcp_call","id":"mc","status":"completed","arguments":"{}","name":"tool","server_label":"srv","approval_request_id":"explicit"}}"#,
        COMPLETED,
    ]);
    differential_normalized("mcp approval", Some("/v1"), base("openai-responses", "gpt-5", jobj! {}), vec![approval], &compact_generated_ids);
}

/// The fixtures of `selfcheck.mjs`'s provider-search checks.
fn selfcheck_sources(mode: &str) -> Reply {
    let url = "https://source.example/article";
    let action = match mode {
        "find" => Some(jobj! { "type": "find_in_page", "url": url, "pattern": "facts" }),
        "search" | "bounded" => {
            let count = if mode == "bounded" { 80 } else { 2 };
            let sources: Vec<Json> = (0..count).map(|i| jobj! { "type": "url", "url": if i == 0 { url.to_owned() } else { format!("{url}/{i}") } }).collect();
            Some(jobj! { "type": "search", "query": "facts", "sources": sources })
        }
        "missing" => None,
        "unsafe" => Some(jobj! { "type": "open_page", "url": "file:///secret" }),
        _ => Some(jobj! { "type": "open_page", "url": url }),
    };
    let item = jobj! { "type": "web_search_call", "id": "ws_source", "status": "completed", "action"?: action };
    let mut frames = vec![
        jobj! { "type": "response.created", "response": jobj! { "id": "resp_sources", "created_at": 1u64, "model": "m" } },
        jobj! { "type": "response.output_item.added", "output_index": 0u64, "item": item.clone() },
        jobj! { "type": "response.output_item.done", "output_index": 0u64, "item": item },
        jobj! { "type": "response.output_item.added", "output_index": 1u64, "item": jobj! { "type": "message", "id": "msg_source" } },
        jobj! { "type": "response.output_text.delta", "output_index": 1u64, "item_id": "msg_source", "delta": "ANSWER_WITHOUT_CITATION" },
    ];
    if mode == "citation" {
        frames.push(jobj! {
            "type": "response.output_text.annotation.added", "output_index": 1u64, "item_id": "msg_source", "content_index": 0u64, "annotation_index": 0u64,
            "annotation": jobj! { "type": "url_citation", "url": url, "title": "Explicit citation", "start_index": 0u64, "end_index": 6u64 },
        });
    }
    frames.push(jobj! { "type": "response.output_item.done", "output_index": 1u64, "item": jobj! { "type": "message", "id": "msg_source" } });
    frames.push(jobj! { "type": "response.completed", "response": jobj! { "usage": jobj! { "input_tokens": 3u64, "output_tokens": 3u64 } } });
    let frames: Vec<String> = frames.iter().map(Json::to_text).collect();
    Reply::sse(&frames.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The fixtures of `selfcheck.mjs`'s phase checks.
fn selfcheck_phases(mode: &str) -> Reply {
    let messages: Vec<(&str, Json)> = match mode {
        "commentary-only" => vec![("PROCESS_ONLY", Json::from("commentary"))],
        "legacy" => vec![("LEGACY_ONLY", Json::Null)],
        _ => {
            let answer = match mode {
                "empty" | "mixed-empty" => "",
                "whitespace" => "   ",
                _ => "ANSWER_ONLY",
            };
            let mut all = vec![("PROCESS_ONLY", Json::from("commentary")), (answer, Json::from("final_answer"))];
            if mode.starts_with("mixed") {
                all.push(("UNPHASED_ONLY", Json::Null));
            }
            all
        }
    };
    let mut frames = vec![jobj! { "type": "response.created", "response": jobj! { "id": "resp_phase", "created_at": 1u64, "model": "m" } }];
    for (index, (text, phase)) in messages.into_iter().enumerate() {
        let id = format!("msg_phase_{index}");
        let added = if mode == "late" {
            jobj! { "type": "message", "id": id.as_str(), "role": "assistant" }
        } else {
            jobj! { "type": "message", "id": id.as_str(), "role": "assistant", "phase": phase.clone() }
        };
        frames.push(jobj! { "type": "response.output_item.added", "output_index": index, "item": added });
        if !text.is_empty() {
            frames.push(jobj! { "type": "response.output_text.delta", "item_id": id.as_str(), "output_index": index, "content_index": 0u64, "delta": text });
        }
        frames.push(jobj! { "type": "response.output_item.done", "output_index": index, "item": jobj! { "type": "message", "id": id.as_str(), "role": "assistant", "phase": phase } });
    }
    frames.push(jobj! { "type": "response.completed", "response": jobj! { "usage": jobj! { "input_tokens": 4u64, "output_tokens": 4u64 } } });
    let frames: Vec<String> = frames.iter().map(Json::to_text).collect();
    Reply::sse(&frames.iter().map(String::as_str).collect::<Vec<_>>())
}

#[test]
fn selfcheck_search_sources_and_phases() {
    let bare = |model: &str, native: bool| {
        let mut payload = jobj! {
            "family": "openai-responses",
            "apiKey": "selfcheck-key",
            "modelId": model,
            "maxSteps": 1u64,
            "messages": vec![jobj! { "role": "user", "content": "Search" }],
        };
        if native {
            payload.as_obj_mut().unwrap().insert("nativeSearch", jobj! { "maxUses": 3u64 });
        }
        payload
    };
    for mode in ["open", "find", "search", "citation", "missing", "unsafe", "bounded"] {
        differential(&format!("sources {mode}"), Some("/v1"), bare(&format!("responses-sources-{mode}"), true), vec![selfcheck_sources(mode)]);
    }
    for mode in ["normal", "late", "legacy", "commentary-only"] {
        for native in [true, false] {
            differential(&format!("phase {mode} {native}"), Some("/v1"), bare(&format!("responses-phase-{mode}"), native), vec![selfcheck_phases(mode)]);
        }
    }
    for mode in ["empty", "whitespace", "mixed", "mixed-empty"] {
        differential(&format!("final presence {mode}"), Some("/v1"), bare(&format!("responses-phase-{mode}"), true), vec![selfcheck_phases(mode)]);
    }
}

#[test]
fn selfcheck_summary_defaults() {
    for family in ["openai-responses", "openai-codex", "azure"] {
        let key = if family == "azure" { "azure" } else { "openai" };
        for mode in ["plaintext", "encrypted"] {
            for summary in [Json::Null, Json::from("detailed")] {
                let mut options = mewrk_aisdk::json::Obj::new();
                options.insert("store", false);
                options.insert("reasoningSummary", summary.clone());
                options.insert("promptCacheKey", format!("{family}-{mode}"));
                if mode == "encrypted" {
                    options.insert("include", vec![Json::from("reasoning.encrypted_content")]);
                }
                let mut provider_options = mewrk_aisdk::json::Obj::new();
                provider_options.insert(key, Json::Obj(options));
                let payload = base(family, "gpt-5-selfcheck-plaintext", jobj! {
                    "tools": Vec::<Json>::new(),
                    "reasoning": "high",
                    "reasoningContent": mode,
                    "providerOptions": Json::Obj(provider_options),
                });
                differential(&format!("summary {family} {mode} {summary}"), Some("/v1"), payload, vec![fixture("openai-responses-reasoning.sse")]);
            }
        }
    }
}

#[test]
fn selfcheck_tool_append_refusal() {
    let late = jobj! { "name": "late", "description": "d", "inputSchema": jobj! { "type": "object" } };
    let ls = jobj! { "name": "ls", "description": "d", "inputSchema": jobj! { "type": "object", "properties": jobj! { "p": jobj! { "type": "string" } } } };
    let history = vec![
        jobj! { "role": "user", "content": "q" },
        jobj! { "role": "assistant", "content": vec![jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "ls", "input": jobj! {} }] },
        jobj! { "role": "tool", "content": vec![jobj! { "type": "tool-result", "toolCallId": "c1", "toolName": "ls", "output": jobj! { "type": "text", "value": "ok" } }] },
        jobj! { "role": "system", "content": "", "providerOptions": jobj! { "mewrk": jobj! { "toolAddition": vec![Json::from("late")] } } },
    ];
    let extra = |append: bool| jobj! { "tools": vec![ls.clone(), late.clone()], "messages": history.clone(), "toolAppend": append };
    differential("append declared", Some("/v1"), base("openai-responses", "gpt-5-selfcheck-append", extra(true)), vec![text_stream()]);
    differential("append undeclared", Some("/v1"), base("openai-responses", "gpt-5-selfcheck-append-undeclared", extra(false)), vec![text_stream()]);
    differential("append codex", Some("/backend-api/codex"), base("openai-codex", "gpt-5-selfcheck-append-codex", extra(true)), vec![text_stream()]);
    // A model that refuses the item gets the declared list instead, once, and that is remembered per model.
    differential(
        "append refused",
        Some("/v1"),
        base("openai-responses", "gpt-5-selfcheck-append-refused", extra(true)),
        vec![
            Reply::json(400, r#"{"error":{"type":"invalid_request_error","message":"Invalid value: 'additional_tools'. Supported values are: 'message', 'function_call' and 'function_call_output'."}}"#),
            text_stream(),
        ],
    );
}

#[test]
fn selfcheck_async_tools() {
    let notification = "agent finished";
    let spawn = jobj! { "name": "agent_spawn", "description": "d", "inputSchema": jobj! { "type": "object" } };
    let ls = jobj! { "name": "ls", "description": "d", "inputSchema": jobj! { "type": "object" } };
    let launched = vec![
        jobj! { "role": "user", "content": "go" },
        jobj! { "role": "assistant", "content": vec![
            jobj! { "type": "tool-call", "toolCallId": "call_spawn", "toolName": "agent_spawn", "input": jobj! {} },
            jobj! { "type": "tool-call", "toolCallId": "call_ls", "toolName": "ls", "input": jobj! {} },
        ] },
        jobj! { "role": "tool", "content": vec![
            jobj! { "type": "tool-result", "toolCallId": "call_spawn", "toolName": "agent_spawn", "output": jobj! { "type": "text", "value": "ok" } },
            jobj! { "type": "tool-result", "toolCallId": "call_ls", "toolName": "ls", "output": jobj! { "type": "text", "value": "files" } },
        ] },
    ];
    let result = jobj! {
        "role": "user",
        "content": format!("<system-reminder>\n[SYSTEM NOTIFICATION - NOT USER INPUT]\n...\n\n{notification}\n</system-reminder>"),
        "providerOptions": jobj! { "mewrk": jobj! { "hostMessage": true, "asyncResult": jobj! { "toolCallId": "call_spawn", "output": notification } } },
    };
    let reminder = jobj! { "role": "user", "content": "<system-reminder>\nhook said hi\n</system-reminder>", "providerOptions": jobj! { "mewrk": jobj! { "hostMessage": true } } };
    let mut answered = launched.clone();
    answered.push(result);
    answered.push(reminder);
    let run = |name: &str, messages: Vec<Json>, async_tools: bool| {
        let mut extra = jobj! { "tools": vec![spawn.clone(), ls.clone()], "messages": messages };
        if async_tools {
            extra.as_obj_mut().unwrap().insert("asyncTools", vec![Json::from("agent_spawn")]);
        }
        differential(name, Some("/v1"), base("openai-responses", "gpt-5-selfcheck-codex-async", extra), vec![text_stream()]);
    };
    run("async pending", launched, true);
    run("async answered", answered.clone(), true);
    run("async undeclared", answered, false);
}
