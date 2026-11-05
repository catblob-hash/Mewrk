//! OpenAI-compatible chat wire format, both directions.
//!
//! Inbound: chunks borrow their strings from the network buffer whenever the JSON has no escapes, so a
//! token costs no allocation. Outbound: requests are ropes of `Bytes`, so a video's base64 is shared by
//! every retry instead of being copied into a new JSON document each time.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, SeqAccess, Visitor};
use serde::Deserialize;

use crate::media::{MediaInput, MediaKind};

// ---------------------------------------------------------------- inbound

fn opt_cow<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Cow<'de, str>>, D::Error> {
    struct Str;
    impl<'de> Visitor<'de> for Str {
        type Value = Option<Cow<'de, str>>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("string or null")
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            d.deserialize_str(Str)
        }
        fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Borrowed(v)))
        }
        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Owned(v.to_owned())))
        }
    }
    d.deserialize_option(Str)
}

/// Only `n = 1` is ever requested, so only the first choice is materialised.
fn first_choice<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Choice<'de>>, D::Error> {
    struct First;
    impl<'de> Visitor<'de> for First {
        type Value = Option<Choice<'de>>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("array of choices")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let first = seq.next_element_seed(ChoiceSeed)?;
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(first)
        }
    }
    struct ChoiceSeed;
    impl<'de> DeserializeSeed<'de> for ChoiceSeed {
        type Value = Choice<'de>;
        fn deserialize<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            Choice::deserialize(d)
        }
    }
    d.deserialize_seq(First)
}

#[derive(Deserialize, Debug, Default)]
pub struct Chunk<'a> {
    #[serde(borrow, default, deserialize_with = "first_choice")]
    pub choices: Option<Choice<'a>>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub timings: Option<Timings>,
    #[serde(default)]
    pub prompt_progress: Option<PromptProgress>,
    #[serde(borrow, default)]
    pub error: Option<ApiError<'a>>,
}

#[derive(Deserialize, Debug)]
pub struct Choice<'a> {
    #[serde(borrow, default)]
    pub delta: Delta<'a>,
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub finish_reason: Option<Cow<'a, str>>,
}

#[derive(Deserialize, Debug, Default)]
pub struct Delta<'a> {
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub content: Option<Cow<'a, str>>,
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub reasoning_content: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub tool_calls: Option<Vec<ToolCallDelta<'a>>>,
}

#[derive(Deserialize, Debug)]
pub struct ToolCallDelta<'a> {
    #[serde(default)]
    pub index: u32,
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub function: Option<FunctionDelta<'a>>,
}

#[derive(Deserialize, Debug)]
pub struct FunctionDelta<'a> {
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub name: Option<Cow<'a, str>>,
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub arguments: Option<Cow<'a, str>>,
}

#[derive(Deserialize, Debug, Clone, Copy, Default)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

/// llama-server's own account of a request, used to check client-side measurements against.
#[derive(Deserialize, Debug, Clone, Copy, Default)]
pub struct Timings {
    #[serde(default)]
    pub cache_n: u32,
    #[serde(default)]
    pub prompt_n: u32,
    #[serde(default)]
    pub prompt_ms: f64,
    #[serde(default)]
    pub predicted_n: u32,
    #[serde(default)]
    pub predicted_ms: f64,
}

#[derive(Deserialize, Debug, Clone, Copy, Default)]
pub struct PromptProgress {
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub processed: u32,
}

#[derive(Deserialize, Debug)]
pub struct ApiError<'a> {
    #[serde(default)]
    pub code: Option<u32>,
    #[serde(borrow, default, deserialize_with = "opt_cow")]
    pub message: Option<Cow<'a, str>>,
}

// ---------------------------------------------------------------- outbound

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub arguments: Arc<str>,
}

#[derive(Clone)]
pub enum Part {
    Text(Arc<str>),
    Media(Arc<MediaInput>),
}

#[derive(Clone)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<Arc<str>>,
}

impl Message {
    pub fn text(role: Role, text: impl Into<Arc<str>>) -> Self {
        Self { role, parts: vec![Part::Text(text.into())], tool_calls: Vec::new(), tool_call_id: None }
    }

    pub fn user_with_media(text: impl Into<Arc<str>>, media: Vec<Arc<MediaInput>>) -> Self {
        let mut parts: Vec<Part> = media.into_iter().map(Part::Media).collect();
        parts.push(Part::Text(text.into()));
        Self { role: Role::User, parts, tool_calls: Vec::new(), tool_call_id: None }
    }

    pub fn has_media(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, Part::Media(_)))
    }

    /// Estimated server encoding time of everything attached to this message.
    pub fn media_estimate_ms(&self) -> u64 {
        self.parts
            .iter()
            .map(|p| match p {
                Part::Media(m) => m.encode_estimate_ms(),
                Part::Text(_) => 0,
            })
            .sum()
    }
}

/// A request body as a list of shared buffers. Cloning it never copies media.
#[derive(Clone, Default)]
pub struct Rope {
    pub parts: Vec<Bytes>,
    pub len: u64,
}

impl Rope {
    /// A body that is one buffer.
    pub fn from_vec(body: Vec<u8>) -> Self {
        let len = body.len() as u64;
        Self { parts: vec![Bytes::from(body)], len }
    }
}

struct RopeWriter {
    cur: Vec<u8>,
    rope: Rope,
}

impl RopeWriter {
    fn new() -> Self {
        Self { cur: Vec::with_capacity(4096), rope: Rope::default() }
    }

    fn raw(&mut self, s: &str) {
        self.cur.extend_from_slice(s.as_bytes());
    }

    fn json_str(&mut self, s: &str) {
        // Serialising a str into a Vec cannot fail.
        serde_json::to_writer(&mut self.cur, s).expect("string serialisation");
    }

    fn shared(&mut self, b: &Bytes) {
        self.flush();
        self.rope.len += b.len() as u64;
        self.rope.parts.push(b.clone());
    }

    fn flush(&mut self) {
        if !self.cur.is_empty() {
            let cur = std::mem::take(&mut self.cur);
            self.rope.len += cur.len() as u64;
            self.rope.parts.push(Bytes::from(cur));
        }
    }

    fn finish(mut self) -> Rope {
        self.flush();
        self.rope
    }
}

/// Everything about a completion request except the conversation.
#[derive(Clone)]
pub struct ChatParams {
    pub model: Arc<str>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub seed: Option<u64>,
    /// `None` leaves the template default, which for LFM3 means thinking on.
    pub thinking: Option<bool>,
    /// Ask for periodic events during prompt processing, which separates "slow" from "dead".
    pub return_progress: bool,
    /// Serialised `tools` array, or nothing.
    pub tools_json: Option<Bytes>,
}

/// Text already delivered to the user in this turn, which the model must continue rather than repeat.
#[derive(Clone, Copy, Default)]
pub struct Prefill<'a> {
    pub reasoning: &'a str,
    pub text: &'a str,
}

impl Prefill<'_> {
    pub fn is_empty(&self) -> bool {
        self.reasoning.is_empty() && self.text.is_empty()
    }
}

fn write_message(w: &mut RopeWriter, m: &Message) {
    w.raw("{\"role\":\"");
    w.raw(m.role.as_str());
    w.raw("\",\"content\":");
    match m.parts.as_slice() {
        [] => w.raw("\"\""),
        [Part::Text(t)] => w.json_str(t),
        parts => {
            w.raw("[");
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    w.raw(",");
                }
                match part {
                    Part::Text(t) => {
                        w.raw("{\"type\":\"text\",\"text\":");
                        w.json_str(t);
                        w.raw("}");
                    }
                    Part::Media(media) => {
                        let (ty, field) = match media.kind {
                            MediaKind::Image => ("image_url", "image_url"),
                            MediaKind::Video => ("video_url", "video_url"),
                        };
                        w.raw("{\"type\":\"");
                        w.raw(ty);
                        w.raw("\",\"");
                        w.raw(field);
                        w.raw("\":{\"url\":\"data:");
                        w.raw(media.mime);
                        w.raw(";base64,");
                        w.shared(&media.b64);
                        w.raw("\"}}");
                    }
                }
            }
            w.raw("]");
        }
    }
    if !m.tool_calls.is_empty() {
        w.raw(",\"tool_calls\":[");
        for (i, call) in m.tool_calls.iter().enumerate() {
            if i > 0 {
                w.raw(",");
            }
            w.raw("{\"id\":");
            w.json_str(&call.id);
            w.raw(",\"type\":\"function\",\"function\":{\"name\":");
            w.json_str(&call.name);
            w.raw(",\"arguments\":");
            w.json_str(&call.arguments);
            w.raw("}}");
        }
        w.raw("]");
    }
    if let Some(id) = &m.tool_call_id {
        w.raw(",\"tool_call_id\":");
        w.json_str(id);
    }
    w.raw("}");
}

pub fn build_chat_request(params: &ChatParams, messages: &[Message], prefill: Prefill<'_>, max_tokens: u32) -> Rope {
    let mut w = RopeWriter::new();
    w.raw("{\"model\":");
    w.json_str(&params.model);
    w.raw(",\"stream\":true,\"stream_options\":{\"include_usage\":true},\"max_tokens\":");
    w.raw(&max_tokens.to_string());
    if let Some(t) = params.temperature {
        w.raw(",\"temperature\":");
        w.raw(&t.to_string());
    }
    if let Some(s) = params.seed {
        w.raw(",\"seed\":");
        w.raw(&s.to_string());
    }
    if let Some(on) = params.thinking {
        w.raw(",\"chat_template_kwargs\":{\"enable_thinking\":");
        w.raw(if on { "true}" } else { "false}" });
    }
    if params.return_progress {
        w.raw(",\"return_progress\":true");
    }
    if let Some(tools) = &params.tools_json {
        w.raw(",\"tools\":");
        w.shared(tools);
    }
    w.raw(",\"messages\":[");
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            w.raw(",");
        }
        write_message(&mut w, m);
    }
    if !prefill.is_empty() {
        if !messages.is_empty() {
            w.raw(",");
        }
        w.raw("{\"role\":\"assistant\",\"content\":");
        w.json_str(prefill.text);
        if !prefill.reasoning.is_empty() {
            w.raw(",\"reasoning_content\":");
            w.json_str(prefill.reasoning);
        }
        w.raw("}");
    }
    w.raw("]}");
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(rope: &Rope) -> Vec<u8> {
        let mut out = Vec::new();
        for part in &rope.parts {
            out.extend_from_slice(part);
        }
        assert_eq!(out.len() as u64, rope.len);
        out
    }

    fn params() -> ChatParams {
        ChatParams {
            model: "m".into(),
            max_tokens: 8,
            temperature: Some(0.5),
            seed: Some(7),
            thinking: Some(false),
            return_progress: true,
            tools_json: Some(Bytes::from_static(b"[{\"type\":\"function\"}]")),
        }
    }

    #[test]
    fn real_llama_server_token_chunk_borrows_its_text() {
        let raw = br#"{"choices":[{"finish_reason":null,"index":0,"delta":{"content":" hello"}}],"created":1,"id":"x","model":"m","object":"chat.completion.chunk"}"#;
        let chunk: Chunk = serde_json::from_slice(raw).unwrap();
        let choice = chunk.choices.unwrap();
        match choice.delta.content.unwrap() {
            Cow::Borrowed(s) => assert_eq!(s, " hello"),
            Cow::Owned(_) => panic!("token text was copied"),
        }
    }

    #[test]
    fn escapes_still_decode() {
        let raw = br#"{"choices":[{"delta":{"reasoning_content":"a\n\"b\""}}]}"#;
        let chunk: Chunk = serde_json::from_slice(raw).unwrap();
        assert_eq!(chunk.choices.unwrap().delta.reasoning_content.unwrap(), "a\n\"b\"");
    }

    #[test]
    fn real_tool_call_deltas() {
        let raw = br#"{"choices":[{"finish_reason":null,"index":0,"delta":{"tool_calls":[{"index":0,"id":"PBKY","type":"function","function":{"name":"calc","arguments":"{"}}]}}]}"#;
        let chunk: Chunk = serde_json::from_slice(raw).unwrap();
        let calls = chunk.choices.unwrap().delta.tool_calls.unwrap();
        assert_eq!(calls[0].id.as_deref(), Some("PBKY"));
        let f = calls[0].function.as_ref().unwrap();
        assert_eq!(f.name.as_deref(), Some("calc"));
        assert_eq!(f.arguments.as_deref(), Some("{"));
    }

    #[test]
    fn usage_chunk_has_empty_choices() {
        let raw = br#"{"choices":[],"usage":{"completion_tokens":24,"prompt_tokens":16,"total_tokens":40},"timings":{"cache_n":1,"prompt_n":15,"prompt_ms":16.8,"predicted_n":24,"predicted_ms":122.3}}"#;
        let chunk: Chunk = serde_json::from_slice(raw).unwrap();
        assert!(chunk.choices.is_none());
        assert_eq!(chunk.usage.unwrap().completion_tokens, 24);
        assert_eq!(chunk.timings.unwrap().predicted_n, 24);
    }

    #[test]
    fn request_is_valid_json_with_prefill_and_tools() {
        let msgs = vec![
            Message::text(Role::System, "be \"brief\""),
            Message::text(Role::User, "hi\n"),
            Message {
                role: Role::Assistant,
                parts: vec![],
                tool_calls: vec![ToolCall { id: "c1".into(), name: "calc".into(), arguments: "{\"e\":1}".into() }],
                tool_call_id: None,
            },
            Message { role: Role::Tool, parts: vec![Part::Text("2".into())], tool_calls: vec![], tool_call_id: Some("c1".into()) },
        ];
        let rope = build_chat_request(&params(), &msgs, Prefill { reasoning: "thin", text: "par\"t" }, 4);
        let v: serde_json::Value = serde_json::from_slice(&flat(&rope)).unwrap();
        assert_eq!(v["max_tokens"], 4);
        assert_eq!(v["chat_template_kwargs"]["enable_thinking"], false);
        let m = v["messages"].as_array().unwrap();
        assert_eq!(m.len(), 5);
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], "{\"e\":1}");
        assert_eq!(m[3]["tool_call_id"], "c1");
        assert_eq!(m[4]["content"], "par\"t");
        assert_eq!(m[4]["reasoning_content"], "thin");
    }

    #[test]
    fn media_is_shared_not_copied() {
        let media = Arc::new(MediaInput::for_test(MediaKind::Video, "video/mp4", Bytes::from_static(b"QUJD")));
        let msgs = vec![Message::user_with_media("what?", vec![media.clone()])];
        let rope = build_chat_request(&params(), &msgs, Prefill::default(), 4);
        assert!(rope.parts.iter().any(|p| p.as_ptr() == media.b64.as_ptr()));
        let v: serde_json::Value = serde_json::from_slice(&flat(&rope)).unwrap();
        assert_eq!(v["messages"][0]["content"][0]["video_url"]["url"], "data:video/mp4;base64,QUJD");
        assert_eq!(v["messages"][0]["content"][1]["text"], "what?");
    }
}
