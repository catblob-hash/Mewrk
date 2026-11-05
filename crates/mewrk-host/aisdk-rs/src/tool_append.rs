//! Tools that join a conversation mid-way, handed over through each protocol's own append interface
//! (`tool-append.ts`).
//!
//! The host marks the point a tool joined with one `system` message with no text whose
//! `providerOptions.mewrk.toolAddition` names the tools. On Anthropic it becomes a mid-conversation system
//! message of `tool_addition` blocks with the tool declared `defer_loading: true`; on Responses an
//! `additional_tools` input item with the tool left out of `tools`; elsewhere the marker goes and the tool
//! stays declared.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::AiError;
use crate::http::{BoxFuture, DynFetch, Fetch, HttpRequest, HttpResponse, url_without_query};
use crate::jobj;
use crate::json::Json;
use crate::system_append::anthropic_system_placement_holds;

const MARKER_OPTIONS_KEY: &str = "mewrk";
const MARKER_TOOLS_KEY: &str = "toolAddition";

/// The text a Responses-bound marker carries through the prompt conversion, which has no `additional_tools`
/// item of its own. The NUL keeps it from ever matching text a person wrote.
const RESPONSES_SENTINEL: &str = "\u{0000}mewrk:additional_tools:";

/// The tools a host marker names, or `None` when `message` is not one.
pub fn marker_tools(message: &Json) -> Option<Vec<String>> {
    if message.str_at("role") != Some("system") {
        return None;
    }
    let tools = message.path(&["providerOptions", MARKER_OPTIONS_KEY, MARKER_TOOLS_KEY])?.as_arr()?;
    Some(tools.iter().filter_map(Json::as_str).map(str::to_owned).collect())
}

#[derive(PartialEq, Clone, Copy)]
enum AppendMode {
    Anthropic,
    Responses,
    None,
}

fn append_mode(family: &str, tool_append: bool) -> AppendMode {
    if !tool_append {
        return AppendMode::None;
    }
    match family {
        "anthropic" => AppendMode::Anthropic,
        "openai-responses" | "openai-codex" | "azure" => AppendMode::Responses,
        _ => AppendMode::None,
    }
}

pub struct PreparedToolAppend {
    pub messages: Vec<Json>,
    /// Tools to declare `defer_loading` (Anthropic only).
    pub deferred: HashSet<String>,
    /// Whether any system message remains among `messages` because of a tool addition.
    pub system_in_messages: bool,
}

/// Turns every host marker into the family's own addition, or drops it. Only tools the request actually
/// declares count: a marker keeps its place in the history after one of its tools is withdrawn.
pub fn prepare_tool_append(family: &str, messages: Vec<Json>, declared: &HashSet<String>, tool_append: bool) -> PreparedToolAppend {
    let mode = append_mode(family, tool_append);
    let mut deferred = HashSet::new();
    let mut out = Vec::with_capacity(messages.len());
    let mut system_in_messages = false;
    for (index, message) in messages.iter().enumerate() {
        let Some(named) = marker_tools(message) else {
            out.push(message.clone());
            continue;
        };
        let names: Vec<String> = named.into_iter().filter(|name| declared.contains(name)).collect();
        if names.is_empty() || mode == AppendMode::None {
            continue;
        }
        if mode == AppendMode::Anthropic {
            if !anthropic_system_placement_holds(&messages, index) {
                continue;
            }
            for name in &names {
                deferred.insert(name.clone());
            }
            let changes: Vec<Json> = names.iter().map(|name| jobj! { "type": "tool_addition", "toolName": name.as_str() }).collect();
            out.push(jobj! {
                "role": "system",
                "content": "",
                "providerOptions": jobj! { "anthropic": jobj! { "toolChanges": changes } },
            });
        } else {
            let names_json: Vec<Json> = names.iter().map(Json::from).collect();
            out.push(jobj! { "role": "system", "content": format!("{RESPONSES_SENTINEL}{}", Json::Arr(names_json).to_text()) });
        }
        system_in_messages = true;
    }
    PreparedToolAppend { messages: out, deferred, system_in_messages }
}

// ------------------------------------------------------------ Responses

fn refused() -> &'static Mutex<HashSet<String>> {
    static REFUSED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    REFUSED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn item_text(item: &Json) -> Option<&str> {
    match item.get("content") {
        Some(Json::Str(text)) => Some(text),
        Some(Json::Arr(content)) if content.len() == 1 => content[0].str_at("text"),
        _ => None,
    }
}

fn sentinel_tools(item: &Json) -> Option<Vec<String>> {
    if !matches!(item.str_at("role"), Some("developer") | Some("system")) {
        return None;
    }
    let text = item_text(item)?;
    let rest = text.strip_prefix(RESPONSES_SENTINEL)?;
    Some(match Json::parse(rest) {
        Ok(Json::Arr(names)) => names.iter().filter_map(Json::as_str).map(str::to_owned).collect(),
        _ => Vec::new(),
    })
}

fn tool_name(tool: &Json) -> Option<&str> {
    if tool.str_at("type") == Some("function") { tool.str_at("name") } else { None }
}

/// Each sentinel item becomes an `additional_tools` item and its tools leave `tools`; with `append` off the
/// sentinels are dropped and every tool stays declared.
fn rewrite_responses_body(body: &mut Json, append: bool) -> bool {
    let Some(input) = body.get("input").and_then(Json::as_arr) else { return false };
    if !input.iter().any(|item| sentinel_tools(item).is_some()) {
        return false;
    }
    let tools: Vec<Json> = body.get("tools").and_then(Json::as_arr).cloned().unwrap_or_default();
    let mut handed: HashSet<String> = HashSet::new();
    let mut next: Vec<Json> = Vec::with_capacity(input.len());
    for item in input {
        let Some(names) = sentinel_tools(item) else {
            next.push(item.clone());
            continue;
        };
        if !append {
            continue;
        }
        let definitions: Vec<Json> = names
            .iter()
            .filter_map(|name| tools.iter().find(|tool| tool_name(tool) == Some(name.as_str())).cloned())
            .collect();
        if definitions.is_empty() {
            continue;
        }
        for name in &names {
            if tools.iter().any(|tool| tool_name(tool) == Some(name.as_str())) {
                handed.insert(name.clone());
            }
        }
        next.push(jobj! { "type": "additional_tools", "role": "developer", "tools": definitions });
    }
    let Some(object) = body.as_obj_mut() else { return false };
    object.insert("input", next);
    if !handed.is_empty() {
        let kept: Vec<Json> = tools.into_iter().filter(|tool| tool_name(tool).is_none_or(|name| !handed.contains(name))).collect();
        object.insert("tools", kept);
    }
    true
}

struct ToolAppendFetch {
    inner: DynFetch,
    append_tools: bool,
}

impl ToolAppendFetch {
    async fn run(&self, mut request: HttpRequest) -> Result<HttpResponse, AiError> {
        let parsed = request.body.as_deref().and_then(|text| Json::parse(text).ok()).filter(Json::is_obj);
        let Some(mut body) = parsed else { return self.inner.fetch(request).await };
        let key = format!("{}|{}", url_without_query(&request.url), body.str_at("model").unwrap_or(""));
        let append = self.append_tools && !refused().lock().unwrap_or_else(|p| p.into_inner()).contains(&key);
        let mut fallback = body.clone();
        if !rewrite_responses_body(&mut body, append) {
            return self.inner.fetch(request).await;
        }
        request.body = Some(body.to_text());
        let mut retry = request.clone();
        let mut response = self.inner.fetch(request).await?;
        if !append || response.status != 400 {
            return Ok(response);
        }
        let text = response.body.read_text().await?;
        if !text.to_ascii_lowercase().contains("additional_tools") {
            return Ok(HttpResponse::with_text(response.status, &response.status_text, response.headers, text));
        }
        refused().lock().unwrap_or_else(|p| p.into_inner()).insert(key);
        rewrite_responses_body(&mut fallback, false);
        retry.body = Some(fallback.to_text());
        self.inner.fetch(retry).await
    }
}

impl Fetch for ToolAppendFetch {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

/// Wraps a Responses `fetch` so a host marker reaches the endpoint as an `additional_tools` item. A 400 that
/// names the item turns appends off for that endpoint and model and resends with the tools declared.
pub fn tool_append_responses_fetch(inner: DynFetch, append_tools: bool) -> DynFetch {
    Arc::new(ToolAppendFetch { inner, append_tools })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(names: &[&str]) -> Json {
        let names: Vec<Json> = names.iter().map(|n| Json::from(*n)).collect();
        jobj! { "role": "system", "content": "", "providerOptions": jobj! { "mewrk": jobj! { "toolAddition": names } } }
    }

    fn declared() -> HashSet<String> {
        ["a", "b"].iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn markers_by_family() {
        let user = jobj! { "role": "user", "content": "u" };
        let prepared = prepare_tool_append("anthropic", vec![user.clone(), marker(&["a", "gone"])], &declared(), true);
        assert_eq!(prepared.messages[1].to_text(), r#"{"role":"system","content":"","providerOptions":{"anthropic":{"toolChanges":[{"type":"tool_addition","toolName":"a"}]}}}"#);
        assert!(prepared.deferred.contains("a") && prepared.system_in_messages);

        let off = prepare_tool_append("anthropic", vec![user.clone(), marker(&["a"])], &declared(), false);
        assert_eq!(off.messages.len(), 1);
        assert!(off.deferred.is_empty() && !off.system_in_messages);

        let chat = prepare_tool_append("openai-chat", vec![user.clone(), marker(&["a"])], &declared(), true);
        assert_eq!(chat.messages.len(), 1);

        let responses = prepare_tool_append("azure", vec![user, marker(&["a"])], &declared(), true);
        assert!(responses.messages[1].str_at("content").unwrap().starts_with(RESPONSES_SENTINEL));
    }

    #[test]
    fn a_placement_the_rule_forbids_leaves_the_tool_declared() {
        let assistant = jobj! { "role": "assistant", "content": "a" };
        let prepared = prepare_tool_append("anthropic", vec![assistant, marker(&["a"])], &declared(), true);
        assert_eq!(prepared.messages.len(), 1);
        assert!(prepared.deferred.is_empty());
    }

    #[test]
    fn the_wire_item_replaces_the_sentinel() {
        let names = Json::Arr(vec![Json::from("a")]).to_text();
        let sentinel = Json::from(format!("{RESPONSES_SENTINEL}{names}")).to_text();
        let text = format!(r#"{{"tools":[{{"type":"function","name":"a"}},{{"type":"function","name":"b"}}],"input":[{{"role":"developer","content":{sentinel}}}]}}"#);
        let mut body = Json::parse(&text).unwrap();
        assert!(rewrite_responses_body(&mut body, true));
        assert_eq!(body.to_text(), r#"{"tools":[{"type":"function","name":"b"}],"input":[{"type":"additional_tools","role":"developer","tools":[{"type":"function","name":"a"}]}]}"#);
        let mut body = Json::parse(&text).unwrap();
        assert!(rewrite_responses_body(&mut body, false));
        assert_eq!(body.to_text(), r#"{"tools":[{"type":"function","name":"a"},{"type":"function","name":"b"}],"input":[]}"#);
    }
}
