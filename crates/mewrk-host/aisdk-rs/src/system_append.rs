//! System prompts the host appended mid-conversation (`system-append.ts`).
//!
//! The host records the point an instruction starts to apply as one `system` message whose
//! `providerOptions.mewrk.systemAppend` is `true`. It never reaches a provider as that: it becomes the
//! protocol's own mid-conversation system message, or its text is lifted into the tail of the system prompt
//! where the protocol, the endpoint or the placement cannot take one.

use crate::jobj;
use crate::json::Json;

const MARKER_OPTIONS_KEY: &str = "mewrk";
const MARKER_KEY: &str = "systemAppend";
const SYSTEM_SECTION_SEPARATOR: &str = "\n\n";

fn system_append_text(message: &Json) -> Option<String> {
    if message.str_at("role") != Some("system") {
        return None;
    }
    let marker = message.path(&["providerOptions", MARKER_OPTIONS_KEY]).filter(|m| m.is_obj())?;
    if marker.get(MARKER_KEY).and_then(Json::as_bool) != Some(true) {
        return None;
    }
    Some(message.str_at("content").unwrap_or("").to_owned())
}

#[derive(PartialEq)]
enum AppendMode {
    Anthropic,
    Inline,
    Lift,
}

fn append_mode(request: &Json) -> AppendMode {
    if request.get("systemAppend").and_then(Json::as_bool) != Some(true) {
        return AppendMode::Lift;
    }
    match request.str_at("family").unwrap_or("") {
        "anthropic" => AppendMode::Anthropic,
        "openai-responses" | "openai-codex" | "azure" | "openai-chat" | "openai-compatible" | "xai" => AppendMode::Inline,
        // Google's, Bedrock's and Vertex's prompts put every system message at the head, and the CLI
        // writes its own requests.
        _ => AppendMode::Lift,
    }
}

/// Anthropic's placement rule for a mid-conversation system message, applied to the run of system messages
/// `index` belongs to: the first message before the run is a user turn (or a tool result), and the first
/// after it is an assistant turn or there is none.
pub fn anthropic_system_placement_holds(messages: &[Json], index: usize) -> bool {
    let role = |at: usize| messages.get(at).and_then(|m| m.str_at("role"));
    let mut before = index as isize - 1;
    while before >= 0 && role(before as usize) == Some("system") {
        before -= 1;
    }
    let mut after = index + 1;
    while after < messages.len() && role(after) == Some("system") {
        after += 1;
    }
    let previous = if before >= 0 { role(before as usize) } else { None };
    let next = if after < messages.len() { role(after) } else { None };
    matches!(previous, Some("user") | Some("tool")) && matches!(next, None | Some("assistant"))
}

/// Replaces the markers in `request.messages`; lifted texts join `systemDynamic`, after whatever tail the
/// host already sent, so the stable prefix keeps its own cache breakpoint.
pub fn apply_system_append(request: &mut Json) {
    let Some(messages) = request.get("messages").and_then(Json::as_arr) else { return };
    if !messages.iter().any(|m| system_append_text(m).is_some()) {
        return;
    }
    let mode = append_mode(request);
    let mut out: Vec<Json> = Vec::with_capacity(messages.len());
    let mut lifted: Vec<String> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let Some(text) = system_append_text(message) else {
            out.push(message.clone());
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let in_place = mode == AppendMode::Inline
            || (mode == AppendMode::Anthropic && anthropic_system_placement_holds(messages, index));
        if in_place {
            out.push(jobj! { "role": "system", "content": text });
        } else {
            lifted.push(text);
        }
    }
    let Some(object) = request.as_obj_mut() else { return };
    object.insert("messages", out);
    if lifted.is_empty() {
        return;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(existing) = object.get("systemDynamic").and_then(Json::as_str) {
        if !existing.is_empty() {
            parts.push(existing.to_owned());
        }
    }
    parts.extend(lifted.into_iter().filter(|text| !text.is_empty()));
    object.insert("systemDynamic", parts.join(SYSTEM_SECTION_SEPARATOR));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(text: &str) -> Json {
        jobj! { "role": "system", "content": text, "providerOptions": jobj! { "mewrk": jobj! { "systemAppend": true } } }
    }

    fn user() -> Json {
        jobj! { "role": "user", "content": "u" }
    }

    fn assistant() -> Json {
        jobj! { "role": "assistant", "content": "a" }
    }

    #[test]
    fn modes() {
        let request = |family: &str, on: bool| {
            jobj! {
                "family": family,
                "systemDynamic": "tail",
                "systemAppend"?: on.then_some(true),
                "messages": vec![user(), marker("plan"), assistant(), marker("  "), user(), marker("late")],
            }
        };
        let mut lifted = request("anthropic", false);
        apply_system_append(&mut lifted);
        assert_eq!(lifted.str_at("systemDynamic"), Some("tail\n\nplan\n\nlate"));
        assert_eq!(lifted.get("messages").unwrap().as_arr().unwrap().len(), 3);

        let mut anthropic = request("anthropic", true);
        apply_system_append(&mut anthropic);
        let messages = anthropic.get("messages").unwrap().as_arr().unwrap().clone();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[1].to_text(), r#"{"role":"system","content":"plan"}"#);
        assert_eq!(messages[4].to_text(), r#"{"role":"system","content":"late"}"#);
        assert_eq!(anthropic.str_at("systemDynamic"), Some("tail"));

        let mut chat = request("openai-chat", true);
        apply_system_append(&mut chat);
        assert_eq!(chat.get("messages").unwrap().as_arr().unwrap().len(), 5);

        let mut google = request("google", true);
        apply_system_append(&mut google);
        assert_eq!(google.str_at("systemDynamic"), Some("tail\n\nplan\n\nlate"));
    }

    #[test]
    fn placement_breaks_where_the_rule_says() {
        let mut request = jobj! {
            "family": "anthropic",
            "systemAppend": true,
            "messages": vec![assistant(), marker("after-assistant"), assistant()],
        };
        apply_system_append(&mut request);
        assert_eq!(request.str_at("systemDynamic"), Some("after-assistant"));
    }
}
