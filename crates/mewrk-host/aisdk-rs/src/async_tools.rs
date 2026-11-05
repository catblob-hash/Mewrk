//! Calls whose result arrives later, on the call itself (`async-tools.ts`).
//!
//! Where the request declares asynchronous tools (only the Responses protocol has them), a launch's receipt
//! gives way to a placeholder the fetch wrapper removes, and a result for a call still waiting becomes that
//! call's `function_call_output`. Everywhere else both stay what the host projected. The marks never reach
//! a provider.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::AiError;
use crate::http::{BoxFuture, DynFetch, Fetch, HttpRequest, HttpResponse};
use crate::jobj;
use crate::json::{Json, Obj};

const MARKER_OPTIONS_KEY: &str = "mewrk";
const HOST_MESSAGE_MARKER: &str = "hostMessage";
const ASYNC_RESULT_MARKER: &str = "asyncResult";
const ASYNC_LAUNCH_MARKER: &str = "asyncLaunch";

/// The output a pending launch carries through the prompt conversion, which would otherwise refuse a call
/// with no result. The NUL keeps it from ever matching text a tool returned.
pub const ASYNC_PENDING_SENTINEL: &str = "\u{0000}mewrk:async-pending";

fn declares_async_tools(request: &Json) -> bool {
    matches!(request.str_at("family"), Some("openai-responses") | Some("openai-codex") | Some("azure"))
        && request.get("asyncTools").and_then(Json::as_arr).is_some_and(|tools| !tools.is_empty())
}

fn marks_of(holder: &Json) -> Option<&Json> {
    holder.path(&["providerOptions", MARKER_OPTIONS_KEY]).filter(|m| m.is_obj())
}

fn strip_marks(holder: &mut Obj, keep: &[&str]) {
    let Some(Json::Obj(options)) = holder.get_mut("providerOptions") else { return };
    let Some(Json::Obj(marks)) = options.get_mut(MARKER_OPTIONS_KEY) else { return };
    marks.0.retain(|(key, _)| keep.contains(&key.as_str()));
    if marks.is_empty() {
        options.remove(MARKER_OPTIONS_KEY);
    }
    if options.is_empty() {
        holder.remove("providerOptions");
    }
}

/// Whether `message` is a message the host wrote, not the user.
pub fn is_host_message(message: &Json) -> bool {
    message.str_at("role") == Some("user")
        && marks_of(message).and_then(|m| m.get(HOST_MESSAGE_MARKER)).and_then(Json::as_bool) == Some(true)
}

/// Shapes the host's deliveries for this request, in place. The `hostMessage` mark stays only for the
/// Claude Code session, which folds such messages behind a parked round's results.
pub fn apply_async_tools(request: &mut Json) {
    let native = declares_async_tools(request);
    let keep_host: &[&str] = if request.str_at("family") == Some("claude-agent") { &[HOST_MESSAGE_MARKER] } else { &[] };
    let Some(messages) = request.as_obj_mut().and_then(|o| o.remove("messages")) else { return };
    let Json::Arr(messages) = messages else {
        if let Some(object) = request.as_obj_mut() {
            object.insert("messages", messages);
        }
        return;
    };
    let mut pending: HashMap<String, String> = HashMap::new();
    // How many converted results each tool message in `out` already starts with, by its position.
    let mut leading: HashMap<usize, usize> = HashMap::new();
    let mut out: Vec<Json> = Vec::with_capacity(messages.len());
    for mut message in messages {
        if !message.is_obj() {
            out.push(message);
            continue;
        }
        let role = message.str_at("role").unwrap_or("").to_owned();
        if role == "tool" && message.get("content").is_some_and(Json::is_arr) {
            if let Some(parts) = message.get_mut("content").and_then(Json::as_arr_mut) {
                for part in parts {
                    if part.str_at("type") != Some("tool-result") {
                        continue;
                    }
                    let launch = marks_of(part).and_then(|m| m.get(ASYNC_LAUNCH_MARKER)).and_then(Json::as_bool) == Some(true);
                    if let Some(object) = part.as_obj_mut() {
                        if launch && native {
                            if let Some(id) = object.get("toolCallId").and_then(Json::as_str).map(str::to_owned) {
                                let tool = object.get("toolName").and_then(Json::as_str).unwrap_or("").to_owned();
                                object.insert("output", jobj! { "type": "text", "value": ASYNC_PENDING_SENTINEL });
                                pending.insert(id, tool);
                            }
                        }
                        strip_marks(object, &[]);
                    }
                }
            }
            out.push(message);
            continue;
        }
        if role == "user" {
            let result = marks_of(&message).and_then(|m| m.get(ASYNC_RESULT_MARKER)).filter(|r| r.is_obj()).cloned();
            if let (true, Some(result)) = (native, &result) {
                if let (Some(id), Some(output)) = (result.str_at("toolCallId"), result.str_at("output")) {
                    if let Some(tool) = pending.remove(id) {
                        let part = jobj! {
                            "type": "tool-result",
                            "toolCallId": id,
                            "toolName": tool,
                            "output": jobj! { "type": "text", "value": output },
                        };
                        let last = out.len().wrapping_sub(1);
                        match out.last_mut() {
                            Some(previous) if previous.str_at("role") == Some("tool") && previous.get("content").is_some_and(Json::is_arr) => {
                                let at = leading.get(&last).copied().unwrap_or(0);
                                if let Some(parts) = previous.get_mut("content").and_then(Json::as_arr_mut) {
                                    parts.insert(at, part);
                                }
                                leading.insert(last, at + 1);
                            }
                            _ => {
                                out.push(jobj! { "role": "tool", "content": vec![part] });
                                leading.insert(out.len() - 1, 1);
                            }
                        }
                        continue;
                    }
                }
            }
            if marks_of(&message).is_some() {
                if let Some(object) = message.as_obj_mut() {
                    strip_marks(object, keep_host);
                }
            }
        }
        out.push(message);
    }
    if let Some(object) = request.as_obj_mut() {
        object.insert("messages", out);
    }
}

/// The function tools a Responses body declares, and the calls and outputs in its input.
fn rewrite_async_body(body: &mut Json, async_tools: &[String]) -> bool {
    let mut changed = false;
    if let Some(tools) = body.get_mut("tools").and_then(Json::as_arr_mut) {
        for tool in tools {
            let wanted = tool.str_at("type") == Some("function")
                && tool.str_at("name").is_some_and(|name| async_tools.iter().any(|t| t == name))
                && tool.get("async").and_then(Json::as_bool) != Some(true);
            if wanted {
                if let Some(object) = tool.as_obj_mut() {
                    object.insert("async", true);
                }
                changed = true;
            }
        }
    }
    if let Some(input) = body.get_mut("input").and_then(Json::as_arr_mut) {
        let mut launches: Vec<String> = Vec::new();
        input.retain(|item| {
            if item.str_at("type") == Some("function_call_output") && item.str_at("output") == Some(ASYNC_PENDING_SENTINEL) {
                if let Some(id) = item.str_at("call_id") {
                    launches.push(id.to_owned());
                    return false;
                }
            }
            true
        });
        for item in input.iter_mut() {
            let launched = item.str_at("type") == Some("function_call")
                && item.str_at("call_id").is_some_and(|id| launches.iter().any(|l| l == id));
            if launched {
                if let Some(object) = item.as_obj_mut() {
                    object.insert("async", true);
                }
            }
        }
        if !launches.is_empty() {
            changed = true;
        }
    }
    changed
}

/// Wraps a Responses `fetch` so the tools the host declares asynchronous go out with `async: true`, and so
/// does every launch still waiting for its output. With no asynchronous tools it is not wrapped at all.
pub fn async_tools_responses_fetch(inner: DynFetch, async_tools: Vec<String>) -> DynFetch {
    if async_tools.is_empty() {
        return inner;
    }
    Arc::new(AsyncToolsFetch { inner, async_tools })
}

struct AsyncToolsFetch {
    inner: DynFetch,
    async_tools: Vec<String>,
}

impl AsyncToolsFetch {
    async fn run(&self, mut request: HttpRequest) -> Result<HttpResponse, AiError> {
        if let Some(text) = &request.body {
            if let Ok(mut body) = Json::parse(text) {
                if body.is_obj() && rewrite_async_body(&mut body, &self.async_tools) {
                    request.body = Some(body.to_text());
                }
            }
        }
        self.inner.fetch(request).await
    }
}

impl Fetch for AsyncToolsFetch {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch_transcript(family: &str, async_tools: Option<Vec<Json>>) -> Json {
        jobj! {
            "family": family,
            "asyncTools"?: async_tools,
            "messages": vec![
                jobj! { "role": "tool", "content": vec![jobj! {
                    "type": "tool-result", "toolCallId": "c1", "toolName": "bg",
                    "output": jobj! { "type": "text", "value": "receipt" },
                    "providerOptions": jobj! { "mewrk": jobj! { "asyncLaunch": true } },
                }] },
                jobj! {
                    "role": "user", "content": "<result>",
                    "providerOptions": jobj! { "mewrk": jobj! {
                        "hostMessage": true,
                        "asyncResult": jobj! { "toolCallId": "c1", "output": "done" },
                    } },
                },
            ],
        }
    }

    #[test]
    fn launches_turn_into_call_outputs_only_where_declared() {
        let mut native = launch_transcript("openai-responses", Some(vec![Json::from("bg")]));
        apply_async_tools(&mut native);
        let messages = native.get("messages").unwrap().as_arr().unwrap();
        assert_eq!(messages.len(), 1);
        let parts = messages[0].get("content").unwrap().as_arr().unwrap();
        assert_eq!(parts[0].path(&["output", "value"]).unwrap().as_str(), Some("done"));
        assert_eq!(parts[1].path(&["output", "value"]).unwrap().as_str(), Some(ASYNC_PENDING_SENTINEL));
        assert!(parts[1].get("providerOptions").is_none());

        let mut plain = launch_transcript("anthropic", None);
        apply_async_tools(&mut plain);
        let messages = plain.get("messages").unwrap().as_arr().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].to_text(), r#"{"role":"user","content":"<result>"}"#);

        let mut agent = launch_transcript("claude-agent", None);
        apply_async_tools(&mut agent);
        let messages = agent.get("messages").unwrap().as_arr().unwrap();
        assert_eq!(messages[1].get("providerOptions").unwrap().to_text(), r#"{"mewrk":{"hostMessage":true}}"#);
    }

    #[test]
    fn the_wire_body_drops_the_placeholder_and_marks_the_call() {
        let mut body = Json::parse(&format!(
            r#"{{"tools":[{{"type":"function","name":"bg"}},{{"type":"function","name":"sync"}}],"input":[{{"type":"function_call","call_id":"c1"}},{{"type":"function_call_output","call_id":"c1","output":{sentinel}}}]}}"#,
            sentinel = Json::from(ASYNC_PENDING_SENTINEL).to_text()
        ))
        .unwrap();
        assert!(rewrite_async_body(&mut body, &["bg".to_owned()]));
        assert_eq!(body.to_text(), r#"{"tools":[{"type":"function","name":"bg","async":true},{"type":"function","name":"sync"}],"input":[{"type":"function_call","call_id":"c1","async":true}]}"#);
    }
}
