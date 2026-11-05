//! Builders for what upstream's fixtures spell out as object literals: the same JSON, read by the host's own types.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::model::{ApiProvider, ContextItem, ModelProfile, ModelStreamEvent};
use crate::runs::model_stream::{reduce_model_stream_event, ModelRunState, ModelStreamEffect};
use crate::runs::types::ModelRunRequest;

pub const AT: &str = "2026-08-04T00:00:01.000Z";

pub fn provider(family: &str) -> ApiProvider {
    serde_json::from_value(json!({ "id": "provider", "name": "provider", "enabled": true, "family": family, "baseUrl": "https://example.test/v1", "models": [] })).unwrap()
}

pub fn model(capabilities: &[&str], reasoning_content: &str) -> ModelProfile {
    serde_json::from_value(json!({ "id": "model", "name": "", "group": "", "capabilities": capabilities, "reasoningContent": reasoning_content })).unwrap()
}

pub fn request(reasoning_content: &str) -> Arc<ModelRunRequest> {
    Arc::new(ModelRunRequest::new(&provider("openai_responses"), &model(&[], reasoning_content), Default::default(), "conv", "/w", vec![], vec![], vec![]).unwrap())
}

/// `runFixture`, with the model upstream's stream fixtures use.
pub fn run_fixture() -> ModelRunState {
    ModelRunState::new("ws", "run_test", request("encrypted"), "2026-08-04T00:00:00.000Z".to_owned())
}

pub fn event(value: Value) -> ModelStreamEvent {
    serde_json::from_value(value).unwrap()
}

pub fn context(value: Value) -> ContextItem {
    serde_json::from_value(value).unwrap()
}

/// `reduceAll`.
pub fn reduce_all(run: &mut ModelRunState, events: Vec<Value>) -> Vec<ModelStreamEffect> {
    let mut effects = Vec::new();
    for value in events {
        effects.extend(reduce_model_stream_event(run, &event(value), AT));
    }
    effects
}

pub fn tool_result(success: bool, output: &str) -> Value {
    json!({ "success": success, "output": output, "executedAt": AT, "durationMs": 1 })
}

pub fn announced(round: usize, call_id: &str, tool_name: &str, context_id: &str) -> Value {
    json!({ "type": "tool_call_announced", "round": round, "callId": call_id, "toolName": tool_name, "contextId": context_id })
}

pub fn nested(call_id: &str, inner: Value) -> Value {
    json!({ "type": "subagent_event", "round": 1, "callId": call_id, "event": inner })
}

pub fn user(id: &str, content: &str) -> ContextItem {
    context(json!({ "kind": "user", "id": id, "content": content, "createdAt": AT }))
}

pub fn assistant(id: &str, content: &str) -> ContextItem {
    context(json!({ "kind": "assistant", "id": id, "content": content, "createdAt": AT }))
}

pub fn ids(contexts: &[crate::runs::model_stream::LiveContext]) -> Vec<String> {
    contexts.iter().map(|context| context.id().to_owned()).collect()
}
