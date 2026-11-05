//! Anthropic Messages.

use agent_rt::NetError;
use serde::Deserialize;
use serde_json::{json, Value};

use super::*;
use crate::model::Item;

/// Claude 4.6 and later think adaptively and take an effort level, and no longer accept a token budget.
fn adaptive_thinking(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("claude-") && ["-4-6", "-4-7", "-5-", "-5.", "fable"].iter().any(|v| m.contains(v))
}

fn budget_for(effort: &str) -> Option<u32> {
    match effort {
        "low" => Some(2_000),
        "medium" => Some(8_000),
        "high" => Some(16_000),
        "max" => Some(32_000),
        _ => None,
    }
}

/// Pushes into the current message of `role`, or starts a new one. Anthropic wants roles to alternate.
fn push_block(messages: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = messages.last_mut()
        && last["role"] == role
        && let Some(content) = last["content"].as_array_mut()
    {
        content.push(block);
        return;
    }
    messages.push(json!({"role": role, "content": [block]}));
}

pub fn encode(req: &TurnRequest<'_>) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::with_capacity(req.items.len());
    // Tool results must come first in the user message that follows the calls, whatever else came after them in the timeline.
    let mut pending_results: Vec<Value> = Vec::new();
    let flush_results = |messages: &mut Vec<Value>, pending: &mut Vec<Value>| {
        for block in pending.drain(..) {
            push_block(messages, "user", block);
        }
    };

    for item in request_items(req.items) {
        match item {
            Item::User { text, images, .. } => {
                flush_results(&mut messages, &mut pending_results);
                for a in images {
                    if let Some(b64) = image_b64(a) {
                        push_block(&mut messages, "user", json!({"type": "image", "source": {"type": "base64", "media_type": a.mime, "data": b64}}));
                    }
                }
                push_block(&mut messages, "user", json!({"type": "text", "text": text}));
            }
            Item::Host { text, .. } => {
                flush_results(&mut messages, &mut pending_results);
                push_block(&mut messages, "user", json!({"type": "text", "text": text}));
            }
            Item::Compaction { summary, .. } => push_block(&mut messages, "user", json!({"type": "text", "text": summary})),
            Item::Thinking { text, signature, .. } => {
                flush_results(&mut messages, &mut pending_results);
                // Reasoning without its proof cannot go back, and Anthropic does not need it to continue.
                if !signature.is_empty() {
                    if text.is_empty() {
                        push_block(&mut messages, "assistant", json!({"type": "redacted_thinking", "data": signature}));
                    } else {
                        push_block(&mut messages, "assistant", json!({"type": "thinking", "thinking": text, "signature": signature}));
                    }
                }
            }
            Item::Text { text, .. } => {
                flush_results(&mut messages, &mut pending_results);
                if !text.is_empty() {
                    push_block(&mut messages, "assistant", json!({"type": "text", "text": text}));
                }
            }
            Item::Tool(t) => {
                push_block(&mut messages, "assistant", json!({"type": "tool_use", "id": t.call_id, "name": t.name, "input": args_object(&t.args)}));
                let mut result = json!({"type": "tool_result", "tool_use_id": t.call_id, "content": tool_output_for_model(t)});
                if t.status != crate::model::ToolStatus::Done {
                    result["is_error"] = json!(true);
                }
                pending_results.push(result);
            }
            Item::Turn { .. } => flush_results(&mut messages, &mut pending_results),
            Item::Notice { .. } => {}
        }
    }
    flush_results(&mut messages, &mut pending_results);

    if !req.prefill_text.is_empty() {
        push_block(&mut messages, "assistant", json!({"type": "text", "text": req.prefill_text.trim_end()}));
    }

    // The conversation so far is the expensive thing to reprocess, so the cache breakpoint sits at its end.
    if let Some(block) = messages.iter_mut().rev().find(|m| m["role"] == "user").and_then(|m| m["content"].as_array_mut()).and_then(|c| c.last_mut()) {
        block["cache_control"] = json!({"type": "ephemeral"});
    }

    let mut body = json!({
        "model": req.model.id,
        "max_tokens": req.max_tokens,
        "messages": messages,
        "stream": true,
    });
    if !req.system.is_empty() {
        body["system"] = json!([{"type": "text", "text": req.system, "cache_control": {"type": "ephemeral"}}]);
    }
    if !req.tools.is_empty() {
        let mut tools: Vec<Value> = req.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters})).collect();
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        body["tools"] = Value::Array(tools);
    }
    if adaptive_thinking(&req.model.id) {
        if req.effort != "off" {
            body["thinking"] = json!({"type": "adaptive"});
            if !req.effort.is_empty() {
                body["output_config"] = json!({"effort": req.effort});
            }
        }
    } else if let Some(budget) = budget_for(req.effort) {
        let budget = budget.min(req.max_tokens.saturating_sub(1024).max(1024));
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    }
    Ok(body)
}

#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(default)]
    index: u32,
    #[serde(default)]
    delta: Option<Value>,
    #[serde(default)]
    content_block: Option<Value>,
    #[serde(default)]
    message: Option<Value>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

fn u64_of(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

#[derive(Default)]
pub struct AnthropicDecoder {
    usage: Usage,
}

impl Decoder for AnthropicDecoder {
    fn feed(&mut self, payload: &[u8], on: &mut dyn FnMut(Delta<'_>)) -> Result<(), NetError> {
        let ev: Event = serde_json::from_slice(payload).map_err(bad_chunk)?;
        match ev.kind {
            "message_start" => {
                if let Some(u) = ev.message.as_ref().and_then(|m| m.get("usage")) {
                    self.usage.input = u64_of(u, "input_tokens");
                    self.usage.cache_read = u64_of(u, "cache_read_input_tokens");
                    self.usage.cache_write = u64_of(u, "cache_creation_input_tokens");
                    self.usage.output = u64_of(u, "output_tokens");
                }
            }
            "content_block_start" => {
                if let Some(b) = &ev.content_block {
                    match b.get("type").and_then(Value::as_str) {
                        Some("tool_use") => on(Delta::ToolStart {
                            index: ev.index,
                            call_id: b.get("id").and_then(Value::as_str).unwrap_or(""),
                            name: b.get("name").and_then(Value::as_str).unwrap_or(""),
                        }),
                        // Nothing to show, but it has to be handed back on the next request.
                        Some("redacted_thinking") => {
                            if let Some(data) = b.get("data").and_then(Value::as_str) {
                                on(Delta::ReasoningSignature(data));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_delta" => {
                if let Some(d) = &ev.delta {
                    let s = |key: &str| d.get(key).and_then(Value::as_str).unwrap_or("");
                    match d.get("type").and_then(Value::as_str) {
                        Some("text_delta") => on(Delta::Text(s("text"))),
                        Some("thinking_delta") => on(Delta::Reasoning(s("thinking"))),
                        Some("signature_delta") => on(Delta::ReasoningSignature(s("signature"))),
                        Some("input_json_delta") => {
                            let delta = s("partial_json");
                            if !delta.is_empty() {
                                on(Delta::ToolArgs { index: ev.index, delta });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(u) = &ev.usage {
                    self.usage.output = u64_of(u, "output_tokens").max(self.usage.output);
                }
                if let Some(reason) = ev.delta.as_ref().and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                    on(Delta::Finish(reason));
                }
            }
            "message_stop" => {
                on(Delta::Usage(self.usage));
                on(Delta::End);
            }
            "error" => {
                let e = ev.error.unwrap_or(Value::Null);
                let message = e.get("message").and_then(Value::as_str).unwrap_or("server error");
                let code = match e.get("type").and_then(Value::as_str) {
                    Some("overloaded_error") => 529,
                    Some("rate_limit_error") => 429,
                    Some("api_error") => 500,
                    Some("invalid_request_error") => 400,
                    Some("authentication_error") => 401,
                    Some("permission_error") => 403,
                    _ => 0,
                };
                return Err(server_error(code, message));
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::model::*;

    #[test]
    fn roles_alternate_and_results_lead_the_next_user_message() {
        let (items, tools, model) = (sample_items(), tools(), ModelProfile { id: "claude-sonnet-4-5".into(), ..Default::default() });
        let req = TurnRequest { model: &model, system: "sys", items: &items, tools: &tools, effort: "low", max_tokens: 8000, prefill_reasoning: "", prefill_text: "" };
        let body = encode(&req).unwrap();
        let m = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        let assistant = m[1]["content"].as_array().unwrap();
        assert_eq!(assistant[0]["type"], "thinking");
        assert_eq!(assistant[0]["signature"], "sig");
        assert_eq!(assistant[3]["input"], json!({}));
        let user = m[2]["content"].as_array().unwrap();
        assert_eq!(user[0]["type"], "tool_result");
        assert_eq!(user[1]["is_error"], true);
        assert_eq!(user.last().unwrap()["text"], "next");
        assert_eq!(user.last().unwrap()["cache_control"]["type"], "ephemeral");
        assert_eq!(body["thinking"]["budget_tokens"], 2000);
        assert!(!body.to_string().contains("must not see"));
    }

    #[test]
    fn newer_claude_models_get_adaptive_thinking() {
        let (items, model) = (sample_items(), ModelProfile { id: "claude-opus-5-5".into(), ..Default::default() });
        let req = TurnRequest { model: &model, system: "", items: &items, tools: &[], effort: "high", max_tokens: 8000, prefill_reasoning: "", prefill_text: "" };
        let body = encode(&req).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn decoding_a_whole_message() {
        let mut d = AnthropicDecoder::default();
        let out = collect(
            &mut d,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":7,"output_tokens":1}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"r"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s"}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"t"}}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"i","name":"read"}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
                r#"{"type":"ping"}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(out[0], r#"Reasoning("r")"#);
        assert_eq!(out[1], r#"ReasoningSignature("s")"#);
        assert_eq!(out[2], r#"Text("t")"#);
        assert!(out[3].contains(r#"call_id: "i""#));
        assert_eq!(out[5], r#"Finish("tool_use")"#);
        assert!(out[6].contains("input: 5") && out[6].contains("output: 9") && out[6].contains("cache_read: 7"));
        assert_eq!(out[7], "End");
    }

    #[test]
    fn overload_is_retryable_and_a_bad_request_is_not() {
        let mut d = AnthropicDecoder::default();
        let e = d.feed(br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#, &mut |_| {}).unwrap_err();
        assert!(e.kind.retryable());
        let e = d.feed(br#"{"type":"error","error":{"type":"invalid_request_error","message":"no"}}"#, &mut |_| {}).unwrap_err();
        assert!(!e.kind.retryable());
    }
}
