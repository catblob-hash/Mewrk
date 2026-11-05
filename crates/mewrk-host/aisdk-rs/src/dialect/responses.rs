//! Responses-compatible endpoint dialect (`responses-dialect.ts`).
//!
//! Plaintext-reasoning models map DeepSeek's `reasoning_text` content events to the summary events the SDK
//! models. Requests are untouched; encrypted replay data stays in every mode.

use std::sync::Arc;

use crate::http::DynFetch;
use crate::jobj;
use crate::json::Json;
use crate::sse::{Rewrite, SseDialectFetch};

const REASONING_TEXT_MARKER: &str = "reasoning_text";

/// Translates one SSE line. Exactly three frame shapes are recognized; everything else passes through.
fn translate_reasoning_text_line(line: &str) -> Rewrite {
    if !line.starts_with("data:") || !line.contains(REASONING_TEXT_MARKER) {
        return Rewrite::Keep;
    }
    let payload = line["data:".len()..].trim();
    if payload.is_empty() || payload == "[DONE]" {
        return Rewrite::Keep;
    }
    let Ok(frame) = Json::parse(payload) else { return Rewrite::Keep };
    let (Some(kind), Some(item_id)) = (frame.str_at("type"), frame.str_at("item_id")) else {
        return Rewrite::Keep;
    };
    let summary_index = frame.get("content_index").filter(|v| v.is_num()).cloned().unwrap_or(Json::from(0u64));
    let output_index = frame.get("output_index").filter(|v| v.is_num()).cloned().unwrap_or(Json::Null);
    if kind == "response.reasoning_text.delta" {
        if let Some(delta) = frame.str_at("delta") {
            return Rewrite::Replace(format!(
                "data: {}",
                jobj! {
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": item_id,
                    "output_index": output_index,
                    "summary_index": summary_index,
                    "delta": delta,
                }
                .to_text()
            ));
        }
    }
    let is_reasoning_part = frame.path(&["part", "type"]).and_then(Json::as_str) == Some("reasoning_text");
    let mapped = match kind {
        "response.content_part.added" if is_reasoning_part => "response.reasoning_summary_part.added",
        "response.content_part.done" if is_reasoning_part => "response.reasoning_summary_part.done",
        _ => return Rewrite::Keep,
    };
    Rewrite::Replace(format!(
        "data: {}",
        jobj! { "type": mapped, "item_id": item_id, "output_index": output_index, "summary_index": summary_index }.to_text()
    ))
}

/// Dialect fetch for plaintext-reasoning Responses models.
pub fn plaintext_reasoning_fetch(inner: DynFetch) -> DynFetch {
    Arc::new(SseDialectFetch {
        make_rewrite: Box::new(|| Box::new(translate_reasoning_text_line)),
        patch_request: None,
        inner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(line: &str) -> String {
        match translate_reasoning_text_line(line) {
            Rewrite::Keep => line.to_owned(),
            Rewrite::Drop => String::new(),
            Rewrite::Replace(text) => text,
            Rewrite::Fail(message) => format!("FAIL {message}"),
        }
    }

    #[test]
    fn the_three_frames_translate() {
        assert_eq!(
            apply(r#"data: {"type":"response.reasoning_text.delta","item_id":"i","output_index":1,"content_index":2,"delta":"d"}"#),
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"i","output_index":1,"summary_index":2,"delta":"d"}"#
        );
        assert_eq!(
            apply(r#"data: {"type":"response.content_part.added","item_id":"i","part":{"type":"reasoning_text"}}"#),
            r#"data: {"type":"response.reasoning_summary_part.added","item_id":"i","output_index":null,"summary_index":0}"#
        );
        assert_eq!(
            apply(r#"data: {"type":"response.content_part.done","item_id":"i","content_index":1,"part":{"type":"reasoning_text"}}"#),
            r#"data: {"type":"response.reasoning_summary_part.done","item_id":"i","output_index":null,"summary_index":1}"#
        );
    }

    #[test]
    fn everything_else_passes() {
        for line in [
            "event: reasoning_text",
            r#"data: {"type":"response.output_text.delta","item_id":"i","delta":"reasoning_text"}"#,
            r#"data: {"type":"response.content_part.added","item_id":"i","part":{"type":"output_text","text":"reasoning_text"}}"#,
        ] {
            assert_eq!(apply(line), line);
        }
    }
}
