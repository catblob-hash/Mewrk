//! OpenAI Responses.

use agent_rt::NetError;
use serde::Deserialize;
use serde_json::{json, Value};

use super::*;
use crate::model::Item;

pub fn encode(req: &TurnRequest<'_>) -> Result<Value, String> {
    let mut input: Vec<Value> = Vec::with_capacity(req.items.len());
    for item in request_items(req.items) {
        match item {
            Item::User { text, images, .. } => {
                let mut content = Vec::new();
                for a in images {
                    if let Some(b64) = image_b64(a) {
                        content.push(json!({"type": "input_image", "image_url": format!("data:{};base64,{b64}", a.mime)}));
                    }
                }
                content.push(json!({"type": "input_text", "text": text}));
                input.push(json!({"type": "message", "role": "user", "content": content}));
            }
            Item::Host { text, .. } => input.push(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})),
            Item::Compaction { summary, .. } => input.push(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": summary}]})),
            // The signature here is the whole reasoning item as the server sent it, encrypted content and all.
            Item::Thinking { signature, .. } => {
                if let Ok(v) = serde_json::from_str::<Value>(signature) {
                    input.push(v);
                }
            }
            Item::Text { text, .. } => {
                if !text.is_empty() {
                    input.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}));
                }
            }
            Item::Tool(t) => {
                input.push(json!({"type": "function_call", "call_id": t.call_id, "name": t.name, "arguments": args_string(&t.args)}));
                input.push(json!({"type": "function_call_output", "call_id": t.call_id, "output": tool_output_for_model(t)}));
            }
            Item::Turn { .. } | Item::Notice { .. } => {}
        }
    }
    let mut body = json!({
        "model": req.model.id,
        "input": input,
        "stream": true,
        "store": false,
        "max_output_tokens": req.max_tokens,
    });
    if !req.system.is_empty() {
        body["instructions"] = json!(req.system);
    }
    if !req.tools.is_empty() {
        body["tools"] = req.tools.iter().map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": false})).collect();
    }
    match req.effort {
        "off" => {}
        "" => {}
        e => {
            let e = if e == "max" { "high" } else { e };
            body["reasoning"] = json!({"effort": e, "summary": "auto"});
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
    }
    Ok(body)
}

#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    output_index: u32,
    #[serde(default)]
    item: Option<Value>,
    #[serde(default)]
    response: Option<Value>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Default)]
pub struct ResponsesDecoder;

fn usage_of(response: &Value) -> Option<Usage> {
    let u = response.get("usage")?;
    let cached = u.get("input_tokens_details").and_then(|d| d.get("cached_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    Some(Usage { input: input.saturating_sub(cached), output: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0), cache_read: cached, cache_write: 0 })
}

impl Decoder for ResponsesDecoder {
    fn feed(&mut self, payload: &[u8], on: &mut dyn FnMut(Delta<'_>)) -> Result<(), NetError> {
        if payload == b"[DONE]" {
            on(Delta::End);
            return Ok(());
        }
        let ev: Event = serde_json::from_slice(payload).map_err(bad_chunk)?;
        match ev.kind {
            "response.output_text.delta" => on(Delta::Text(ev.delta.as_deref().unwrap_or(""))),
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => on(Delta::Reasoning(ev.delta.as_deref().unwrap_or(""))),
            "response.output_item.added" => {
                if let Some(item) = &ev.item
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                {
                    on(Delta::ToolStart {
                        index: ev.output_index,
                        call_id: item.get("call_id").and_then(Value::as_str).unwrap_or(""),
                        name: item.get("name").and_then(Value::as_str).unwrap_or(""),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                let delta = ev.delta.as_deref().unwrap_or("");
                if !delta.is_empty() {
                    on(Delta::ToolArgs { index: ev.output_index, delta });
                }
            }
            "response.output_item.done" => {
                if let Some(item) = &ev.item
                    && item.get("type").and_then(Value::as_str) == Some("reasoning")
                {
                    let mut item = item.clone();
                    if let Some(o) = item.as_object_mut() {
                        o.remove("status");
                    }
                    on(Delta::ReasoningSignature(&item.to_string()));
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = ev.response.unwrap_or(Value::Null);
                if let Some(u) = usage_of(&response) {
                    on(Delta::Usage(u));
                }
                on(Delta::Finish(if ev.kind == "response.incomplete" { "length" } else { "stop" }));
                on(Delta::End);
            }
            "response.failed" => {
                let e = ev.response.as_ref().and_then(|r| r.get("error")).cloned().unwrap_or(Value::Null);
                let code = e.get("code").and_then(Value::as_str).unwrap_or("");
                let status = match code {
                    "rate_limit_exceeded" => 429,
                    "server_error" | "" => 500,
                    _ => 400,
                };
                return Err(server_error(status, e.get("message").and_then(Value::as_str).unwrap_or("the response failed")));
            }
            "error" => {
                let status = if ev.code.as_deref() == Some("rate_limit_exceeded") { 429 } else { 500 };
                return Err(server_error(status, ev.message.as_deref().unwrap_or("server error")));
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
    fn input_items_pair_calls_with_outputs() {
        let (mut items, tools, model) = (sample_items(), tools(), ModelProfile { id: "gpt".into(), ..Default::default() });
        items[1] = Item::Thinking { id: 2, text: "hmm".into(), signature: r#"{"type":"reasoning","id":"rs_1","encrypted_content":"x"}"#.into(), tokens: 1 };
        let req = TurnRequest { model: &model, system: "sys", items: &items, tools: &tools, effort: "high", max_tokens: 100, prefill_reasoning: "", prefill_text: "" };
        let body = encode(&req).unwrap();
        let input = body["input"].as_array().unwrap();
        let kinds: Vec<&str> = input.iter().map(|x| x["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["message", "reasoning", "message", "function_call", "function_call_output", "function_call", "function_call_output", "message", "message"]);
        assert_eq!(input[1]["encrypted_content"], "x");
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn decoding_a_stream() {
        let mut d = ResponsesDecoder;
        let out = collect(
            &mut d,
            &[
                r#"{"type":"response.reasoning_summary_text.delta","delta":"r"}"#,
                r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs","status":"completed","encrypted_content":"e"}}"#,
                r#"{"type":"response.output_text.delta","delta":"t"}"#,
                r#"{"type":"response.output_item.added","output_index":3,"item":{"type":"function_call","call_id":"c","name":"read"}}"#,
                r#"{"type":"response.function_call_arguments.delta","output_index":3,"delta":"{}"}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":9,"output_tokens":2,"input_tokens_details":{"cached_tokens":4}}}}"#,
            ],
        );
        assert_eq!(out[0], r#"Reasoning("r")"#);
        assert!(out[1].contains("encrypted_content") && !out[1].contains("status"));
        assert_eq!(out[2], r#"Text("t")"#);
        assert!(out[3].contains("index: 3"));
        assert!(out[5].contains("input: 5") && out[5].contains("cache_read: 4"));
        assert_eq!(out[6], r#"Finish("stop")"#);
        assert_eq!(out[7], "End");
    }
}
