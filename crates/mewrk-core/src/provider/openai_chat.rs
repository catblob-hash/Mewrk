//! OpenAI Chat Completions, which most other servers speak too.

use std::borrow::Cow;

use agent_rt::NetError;
use serde::Deserialize;
use serde_json::{json, Value};

use super::*;
use crate::model::{Item, ProviderConfig};

fn is_real_openai(p: &ProviderConfig) -> bool {
    p.base_url.contains("api.openai.com")
}

pub fn encode(provider: &ProviderConfig, req: &TurnRequest<'_>) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::with_capacity(req.items.len() + 2);
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    // Servers that return reasoning generally want it back on the same turn. OpenAI's own API returns none and rejects unknown fields.
    let echo_reasoning = !is_real_openai(provider);

    let mut reasoning = String::new();
    let mut text = String::new();
    let mut calls: Vec<Value> = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    let flush = |messages: &mut Vec<Value>, reasoning: &mut String, text: &mut String, calls: &mut Vec<Value>, results: &mut Vec<Value>| {
        if !reasoning.is_empty() || !text.is_empty() || !calls.is_empty() {
            let mut m = json!({"role": "assistant", "content": text.as_str()});
            if echo_reasoning && !reasoning.is_empty() {
                m["reasoning_content"] = json!(reasoning.as_str());
            }
            if !calls.is_empty() {
                m["tool_calls"] = Value::Array(std::mem::take(calls));
            }
            messages.push(m);
        }
        reasoning.clear();
        text.clear();
        messages.append(results);
    };

    for item in request_items(req.items) {
        match item {
            Item::User { text: t, images, .. } => {
                flush(&mut messages, &mut reasoning, &mut text, &mut calls, &mut results);
                if images.is_empty() {
                    messages.push(json!({"role": "user", "content": t}));
                } else {
                    let mut parts = Vec::new();
                    for a in images {
                        if let Some(b64) = image_b64(a) {
                            parts.push(json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{b64}", a.mime)}}));
                        }
                    }
                    parts.push(json!({"type": "text", "text": t}));
                    messages.push(json!({"role": "user", "content": parts}));
                }
            }
            Item::Host { text: t, .. } => {
                flush(&mut messages, &mut reasoning, &mut text, &mut calls, &mut results);
                messages.push(json!({"role": "user", "content": t}));
            }
            Item::Compaction { summary, .. } => {
                messages.push(json!({"role": "user", "content": summary}));
            }
            Item::Thinking { text: t, .. } => reasoning.push_str(t),
            Item::Text { text: t, .. } => {
                // Text after tool calls belongs to a later assistant message.
                if !calls.is_empty() {
                    flush(&mut messages, &mut reasoning, &mut text, &mut calls, &mut results);
                }
                text.push_str(t);
            }
            Item::Tool(t) => {
                calls.push(json!({"id": t.call_id, "type": "function", "function": {"name": t.name, "arguments": args_string(&t.args)}}));
                results.push(json!({"role": "tool", "tool_call_id": t.call_id, "content": tool_output_for_model(t)}));
            }
            Item::Turn { .. } => flush(&mut messages, &mut reasoning, &mut text, &mut calls, &mut results),
            Item::Notice { .. } => {}
        }
    }
    flush(&mut messages, &mut reasoning, &mut text, &mut calls, &mut results);

    if !req.prefill_text.is_empty() || !req.prefill_reasoning.is_empty() {
        let mut m = json!({"role": "assistant", "content": req.prefill_text});
        if !req.prefill_reasoning.is_empty() {
            m["reasoning_content"] = json!(req.prefill_reasoning);
        }
        messages.push(m);
    }

    let mut body = json!({
        "model": req.model.id,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    let tokens_key = if is_real_openai(provider) { "max_completion_tokens" } else { "max_tokens" };
    body[tokens_key] = json!(req.max_tokens);
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
    }
    match req.effort {
        "" | "off" => {}
        "max" => body["reasoning_effort"] = json!("high"),
        e => body["reasoning_effort"] = json!(e),
    }
    Ok(body)
}

#[derive(Deserialize)]
struct Chunk<'a> {
    #[serde(borrow, default)]
    choices: Vec<Choice<'a>>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(borrow, default)]
    error: Option<ApiError<'a>>,
}

#[derive(Deserialize)]
struct ApiError<'a> {
    #[serde(borrow, default)]
    message: Option<Cow<'a, str>>,
    #[serde(default)]
    code: Option<Value>,
}

#[derive(Deserialize)]
struct Choice<'a> {
    #[serde(borrow, default)]
    delta: Option<WireDelta<'a>>,
    #[serde(borrow, default)]
    finish_reason: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct WireDelta<'a> {
    #[serde(borrow, default)]
    content: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    reasoning_content: Option<Cow<'a, str>>,
    /// OpenRouter and some others spell it this way.
    #[serde(borrow, default)]
    reasoning: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    tool_calls: Option<Vec<WireCall<'a>>>,
}

#[derive(Deserialize)]
struct WireCall<'a> {
    #[serde(default)]
    index: u32,
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    function: Option<WireFunction<'a>>,
}

#[derive(Deserialize)]
struct WireFunction<'a> {
    #[serde(borrow, default)]
    name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    arguments: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<CachedDetails>,
    /// DeepSeek reports the cached part here.
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct CachedDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Default)]
pub struct ChatDecoder {
    started: Vec<u32>,
}

impl Decoder for ChatDecoder {
    fn feed(&mut self, payload: &[u8], on: &mut dyn FnMut(Delta<'_>)) -> Result<(), NetError> {
        if payload == b"[DONE]" {
            on(Delta::End);
            return Ok(());
        }
        let chunk: Chunk = serde_json::from_slice(payload).map_err(bad_chunk)?;
        if let Some(e) = chunk.error {
            let code = e.code.as_ref().and_then(Value::as_u64).unwrap_or(0) as u16;
            return Err(server_error(code, e.message.as_deref().unwrap_or("server error")));
        }
        if let Some(u) = chunk.usage {
            let cached = u.prompt_tokens_details.map(|d| d.cached_tokens).or(u.prompt_cache_hit_tokens).unwrap_or(0);
            on(Delta::Usage(Usage { input: u.prompt_tokens.saturating_sub(cached), output: u.completion_tokens, cache_read: cached, cache_write: 0 }));
        }
        for choice in chunk.choices {
            if let Some(d) = choice.delta {
                if let Some(r) = d.reasoning_content.as_deref().or(d.reasoning.as_deref()).filter(|s| !s.is_empty()) {
                    on(Delta::Reasoning(r));
                }
                if let Some(t) = d.content.as_deref().filter(|s| !s.is_empty()) {
                    on(Delta::Text(t));
                }
                for call in d.tool_calls.unwrap_or_default() {
                    let function = call.function;
                    let name = function.as_ref().and_then(|f| f.name.as_deref()).filter(|s| !s.is_empty());
                    if let Some(name) = name
                        && !self.started.contains(&call.index)
                    {
                        self.started.push(call.index);
                        on(Delta::ToolStart { index: call.index, call_id: call.id.as_deref().unwrap_or(""), name });
                    }
                    if let Some(args) = function.as_ref().and_then(|f| f.arguments.as_deref()).filter(|s| !s.is_empty()) {
                        on(Delta::ToolArgs { index: call.index, delta: args });
                    }
                }
            }
            if let Some(reason) = choice.finish_reason.as_deref() {
                on(Delta::Finish(reason));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::model::*;

    fn provider() -> ProviderConfig {
        ProviderConfig { base_url: "https://x.example/v1".into(), ..Default::default() }
    }

    #[test]
    fn the_timeline_becomes_messages_and_every_call_gets_an_answer() {
        let (items, tools, model) = (sample_items(), tools(), ModelProfile { id: "m".into(), ..Default::default() });
        let req = TurnRequest { model: &model, system: "sys", items: &items, tools: &tools, effort: "high", max_tokens: 100, prefill_reasoning: "", prefill_text: "" };
        let body = encode(&provider(), &req).unwrap();
        let m = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "tool", "user", "user"]);
        assert_eq!(m[2]["reasoning_content"], "hmm");
        assert_eq!(m[2]["tool_calls"][1]["function"]["arguments"], "{}");
        assert!(m[4]["content"].as_str().unwrap().contains("stopped"));
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_tokens"], 100);
        assert!(!body.to_string().contains("must not see"));
    }

    #[test]
    fn decoding_handles_text_reasoning_tools_and_usage() {
        let mut d = ChatDecoder::default();
        let out = collect(
            &mut d,
            &[
                r#"{"choices":[{"delta":{"reasoning_content":"r"}}]}"#,
                r#"{"choices":[{"delta":{"content":"t"}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"{"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4}}}"#,
                "[DONE]",
            ],
        );
        assert_eq!(out[0], r#"Reasoning("r")"#);
        assert_eq!(out[1], r#"Text("t")"#);
        assert!(out[2].starts_with("ToolStart"));
        assert_eq!(out[3], r#"ToolArgs { index: 0, delta: "{" }"#);
        assert_eq!(out[5], r#"Finish("tool_calls")"#);
        assert!(out[6].contains("input: 6") && out[6].contains("cache_read: 4"));
        assert_eq!(out[7], "End");
    }

    #[test]
    fn an_error_in_the_stream_is_an_error() {
        let mut d = ChatDecoder::default();
        let e = d.feed(br#"{"error":{"message":"overloaded","code":503}}"#, &mut |_| {}).unwrap_err();
        assert_eq!(e.kind, agent_rt::FailKind::Server(503));
    }
}
