//! Model protocols. Each one turns the timeline into a request body, and a stream of SSE payloads into
//! [`Delta`]s. Nothing above this module knows which protocol it is talking to.

pub mod anthropic;
pub mod openai_chat;
pub mod responses;

use std::path::Path;
use std::sync::Arc;

use agent_rt::{FailKind, NetError, Transport, TransportConfig};
use serde_json::{json, Value};

use crate::model::{Attachment, Item, ModelProfile, ProviderConfig, ProviderKind, ToolItem, ToolStatus, Usage};

/// A tool as the model is told about it.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

pub struct TurnRequest<'a> {
    pub model: &'a ModelProfile,
    pub system: &'a str,
    pub items: &'a [Item],
    pub tools: &'a [ToolSpec],
    pub effort: &'a str,
    pub max_tokens: u32,
    /// What this turn already delivered before a connection died, for a server that can continue from it.
    pub prefill_reasoning: &'a str,
    pub prefill_text: &'a str,
}

/// One step of an assistant turn, whatever the protocol called it.
#[derive(Debug, PartialEq)]
pub enum Delta<'a> {
    Reasoning(&'a str),
    /// A complete opaque proof for the reasoning streamed so far.
    ReasoningSignature(&'a str),
    Text(&'a str),
    ToolStart { index: u32, call_id: &'a str, name: &'a str },
    ToolArgs { index: u32, delta: &'a str },
    Usage(Usage),
    Finish(&'a str),
    /// The stream will send nothing more that matters.
    End,
}

pub trait Decoder: Send {
    fn feed(&mut self, payload: &[u8], on: &mut dyn FnMut(Delta<'_>)) -> Result<(), NetError>;
}

pub fn build_transport(p: &ProviderConfig, api_key: Option<&str>) -> Result<Arc<Transport>, String> {
    let mut headers = Vec::new();
    let header = |name: &'static str, value: &str| -> Result<(http::HeaderName, http::HeaderValue), String> {
        let mut v = http::HeaderValue::from_str(value.trim()).map_err(|_| format!("{name} 的值含有非法字符"))?;
        v.set_sensitive(true);
        Ok((http::HeaderName::from_static(name), v))
    };
    match p.kind {
        ProviderKind::Anthropic => {
            if let Some(key) = api_key {
                headers.push(header("x-api-key", key)?);
            }
            headers.push(header("anthropic-version", "2023-06-01")?);
        }
        ProviderKind::OpenAiChat | ProviderKind::OpenAiResponses => {
            if let Some(key) = api_key {
                headers.push(header("authorization", &format!("Bearer {}", key.trim()))?);
            }
        }
    }
    Transport::new(TransportConfig { base_url: p.base_url.trim().trim_end_matches('/').to_owned(), headers, ..Default::default() })
}

pub fn path(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAiChat => "/chat/completions",
        ProviderKind::OpenAiResponses => "/responses",
        ProviderKind::Anthropic => "/messages",
    }
}

pub fn encode(kind: ProviderKind, provider: &ProviderConfig, req: &TurnRequest<'_>) -> Result<Vec<u8>, String> {
    let body = match kind {
        ProviderKind::OpenAiChat => openai_chat::encode(provider, req),
        ProviderKind::OpenAiResponses => responses::encode(req),
        ProviderKind::Anthropic => anthropic::encode(req),
    }?;
    serde_json::to_vec(&body).map_err(|e| e.to_string())
}

pub fn decoder(kind: ProviderKind) -> Box<dyn Decoder> {
    match kind {
        ProviderKind::OpenAiChat => Box::new(openai_chat::ChatDecoder::default()),
        ProviderKind::OpenAiResponses => Box::new(responses::ResponsesDecoder),
        ProviderKind::Anthropic => Box::new(anthropic::AnthropicDecoder::default()),
    }
}

/// The part of the timeline a request carries: from the latest compaction on, without display-only items.
pub fn request_items(items: &[Item]) -> &[Item] {
    let start = items.iter().rposition(|i| matches!(i, Item::Compaction { .. })).unwrap_or(0);
    &items[start..]
}

/// A tool call the model made that never got an answer still needs one, or every provider rejects the request.
pub fn tool_output_for_model(t: &ToolItem) -> String {
    match t.status {
        ToolStatus::Done | ToolStatus::Error => t.output.clone(),
        ToolStatus::Denied => {
            if t.output.is_empty() {
                "The user denied this tool call.".to_owned()
            } else {
                t.output.clone()
            }
        }
        ToolStatus::Cancelled | ToolStatus::Streaming | ToolStatus::Pending | ToolStatus::Running => "The run was stopped before this tool call finished.".to_owned(),
    }
}

/// Tool arguments must go back as an object. A model that streamed garbage still gets the conversation it wrote, with an empty object in its place.
pub fn args_object(args: &str) -> Value {
    match serde_json::from_str::<Value>(args) {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    }
}

pub fn args_string(args: &str) -> String {
    if serde_json::from_str::<Value>(args).is_ok_and(|v| v.is_object()) {
        args.to_owned()
    } else {
        "{}".to_owned()
    }
}

/// The attachment as base64, read when the request is built. A file that has gone away becomes a note instead of a failure.
pub fn image_b64(a: &Attachment) -> Option<String> {
    let bytes = std::fs::read(Path::new(&a.path)).ok()?;
    Some(base64_simd::STANDARD.encode_to_string(bytes))
}

pub(crate) fn bad_chunk(e: impl std::fmt::Display) -> NetError {
    NetError::new(FailKind::Protocol, format!("bad chunk: {e}"))
}

pub(crate) fn server_error(code: u16, message: &str) -> NetError {
    NetError::new(FailKind::Server(code), message)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::model::*;

    pub fn sample_items() -> Vec<Item> {
        vec![
            Item::User { id: 1, text: "hi".into(), images: vec![], at: 0 },
            Item::Thinking { id: 2, text: "hmm".into(), signature: "sig".into(), tokens: 1 },
            Item::Text { id: 3, text: "let me look".into(), tokens: 1 },
            Item::Tool(ToolItem { id: 4, call_id: "c1".into(), name: "read".into(), args: r#"{"path":"a"}"#.into(), status: ToolStatus::Done, output: "body".into(), ..Default::default() }),
            Item::Tool(ToolItem { id: 5, call_id: "c2".into(), name: "ls".into(), args: "not json".into(), status: ToolStatus::Cancelled, ..Default::default() }),
            Item::Turn { id: 6, model: "m".into(), usage: Usage::default(), finish: "tool_calls".into(), at: 0 },
            Item::Notice { id: 7, level: NoticeLevel::Info, text: "the model must not see this".into() },
            Item::Host { id: 8, text: "<system-reminder>x</system-reminder>".into(), at: 0 },
            Item::User { id: 9, text: "next".into(), images: vec![], at: 0 },
        ]
    }

    pub fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec { name: "read".into(), description: "Read".into(), parameters: json!({"type": "object"}) }]
    }

    pub fn collect(dec: &mut dyn Decoder, payloads: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for p in payloads {
            dec.feed(p.as_bytes(), &mut |d| out.push(format!("{d:?}"))).unwrap();
        }
        out
    }
}
