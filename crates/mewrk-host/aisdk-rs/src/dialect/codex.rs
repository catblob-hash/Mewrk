//! ChatGPT-subscription Codex Responses dialect (`codex-dialect.ts`).
//!
//! Codex accepts only streamed Responses requests, rejects `max_output_tokens`, requires `store:false` and
//! encrypted reasoning in `include`, and sends its SSE body without a `content-type` header. Only this crate's
//! streaming path calls it, so the wrapper's non-streaming branch is kept for the same reason the TypeScript
//! keeps it: the request's own `stream` value decides.

use std::sync::Arc;

use crate::error::AiError;
use crate::http::{Body, BoxFuture, DynFetch, Fetch, Headers, HttpRequest, HttpResponse};
use crate::jobj;
use crate::json::Json;

const ENCRYPTED_REASONING_INCLUDE: &str = "reasoning.encrypted_content";

fn coerce_request_body(text: &str) -> (String, bool) {
    let Ok(mut value) = Json::parse(text) else { return (text.to_owned(), false) };
    let Some(object) = value.as_obj_mut() else { return (text.to_owned(), false) };
    let was_streaming = object.get("stream").and_then(Json::as_bool) == Some(true);
    object.insert("store", false);
    object.insert("stream", true);
    object.remove("max_output_tokens");
    match object.get_mut("include") {
        Some(Json::Arr(include)) => {
            if !include.iter().any(|item| item.as_str() == Some(ENCRYPTED_REASONING_INCLUDE)) {
                include.push(Json::from(ENCRYPTED_REASONING_INCLUDE));
            }
        }
        _ => object.insert("include", vec![Json::from(ENCRYPTED_REASONING_INCLUDE)]),
    }
    (value.to_text(), was_streaming)
}

fn error_result(event: &Json) -> (u16, String) {
    let response_error = event.get("response").and_then(|r| r.get("error")).filter(|e| e.is_obj());
    let message = response_error
        .and_then(|e| e.str_at("message"))
        .filter(|m| !m.is_empty())
        .or_else(|| event.str_at("message").filter(|m| !m.is_empty()))
        .unwrap_or("Codex 后端返回失败事件");
    let kind = response_error
        .and_then(|e| e.str_at("code"))
        .filter(|m| !m.is_empty())
        .or_else(|| event.str_at("code").filter(|m| !m.is_empty()))
        .unwrap_or("codex_stream_failed");
    (502, jobj! { "error": jobj! { "message": message, "type": kind } }.to_text())
}

/// Turn Codex's completed SSE transport response into the normal Responses JSON a non-streaming call expects.
fn response_from_completed_sse(text: &str) -> (u16, String) {
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(payload) = line.strip_prefix("data:").map(str::trim) else { continue };
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(event) = Json::parse(payload) else { continue };
        match event.str_at("type") {
            Some("response.completed") | Some("response.incomplete") => {
                return (200, event.get("response").map_or_else(|| "null".to_owned(), Json::to_text));
            }
            Some("response.failed") | Some("error") => return error_result(&event),
            _ => {}
        }
    }
    (502, jobj! { "error": jobj! { "message": "Codex 后端在没有终止事件的情况下结束了响应", "type": "codex_stream_failed" } }.to_text())
}

struct CodexFetch {
    inner: DynFetch,
}

impl CodexFetch {
    async fn run(&self, mut request: HttpRequest) -> Result<HttpResponse, AiError> {
        let mut was_streaming = false;
        if let Some(body) = &request.body {
            let (coerced, streaming) = coerce_request_body(body);
            was_streaming = streaming;
            request.body = Some(coerced);
        }
        let mut response = self.inner.fetch(request).await?;
        if response.status != 200 {
            return Ok(response);
        }
        if was_streaming {
            if !response.headers.has("content-type") {
                response.headers.set("content-type", "text/event-stream");
            }
            return Ok(response);
        }
        let text = response.body.read_text().await?;
        let (status, body) = response_from_completed_sse(&text);
        let mut headers = Headers::new();
        headers.set("content-type", "application/json");
        Ok(HttpResponse { status, status_text: String::new(), headers, body: Body::text(body) })
    }
}

impl Fetch for CodexFetch {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

/// Adapts Codex's required SSE transport without changing its authorization model: the host supplies the
/// OAuth bearer token and account headers directly.
pub fn codex_dialect_fetch(inner: DynFetch) -> DynFetch {
    Arc::new(CodexFetch { inner })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_shape_is_invariant() {
        let (body, streaming) = coerce_request_body(r#"{"model":"m","stream":true,"max_output_tokens":10,"include":["x"]}"#);
        assert!(streaming);
        assert_eq!(body, r#"{"model":"m","stream":true,"include":["x","reasoning.encrypted_content"],"store":false}"#);
        let (body, streaming) = coerce_request_body(r#"{"model":"m"}"#);
        assert!(!streaming);
        assert_eq!(body, r#"{"model":"m","store":false,"stream":true,"include":["reasoning.encrypted_content"]}"#);
        assert_eq!(coerce_request_body("not json").0, "not json");
    }

    #[test]
    fn terminal_events_decide_the_json_answer() {
        assert_eq!(response_from_completed_sse("data: {\"type\":\"x\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n"), (200, r#"{"id":"r"}"#.to_owned()));
        let (status, body) = response_from_completed_sse("data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"no\",\"code\":\"rate_limit\"}}}\n");
        assert_eq!(status, 502);
        assert_eq!(body, r#"{"error":{"message":"no","type":"rate_limit"}}"#);
        assert_eq!(response_from_completed_sse("").0, 502);
    }
}
