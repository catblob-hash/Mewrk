//! Native, provider-executed `web_search` and `web_fetch` (`search.ts`).
//!
//! Factories are selected by wire-protocol family, not by vendor or model id. `openai-chat` and
//! `openai-compatible` have none, and the host then returns a recoverable rejection rather than silently
//! skipping search.

use crate::jobj;
use crate::json::Json;
use crate::model::ProviderTool;

const TOOL_NAME: &str = "web_search";

/// An Anthropic server tool from the versions this build can emit; an unknown version takes the basic one.
fn messages_search_tool(tool_type: Option<&str>, args: Json) -> ProviderTool {
    let id = match tool_type {
        Some("web_search_20260209") => "anthropic.web_search_20260209",
        _ => "anthropic.web_search_20250305",
    };
    ProviderTool { id, args, supports_deferred_results: true }
}

fn messages_fetch_tool(tool_type: Option<&str>, args: Json) -> ProviderTool {
    let id = match tool_type {
        Some("web_fetch_20260209") => "anthropic.web_fetch_20260209",
        _ => "anthropic.web_fetch_20250910",
    };
    ProviderTool { id, args, supports_deferred_results: true }
}

/// The native search tool for this request and the name it is declared under, or `None` when the family
/// lacks one.
pub fn native_search_tool(family: &str, max_uses: u64, tool_type: Option<&str>) -> Option<(String, ProviderTool)> {
    match family {
        // Responses has no `max_uses` equivalent.
        "openai-responses" | "openai-codex" | "azure" => Some((
            TOOL_NAME.to_owned(),
            ProviderTool { id: "openai.web_search", args: Json::obj(), supports_deferred_results: false },
        )),
        "anthropic" | "bedrock" => {
            // Zero means unlimited, so the field is left out.
            let args = if max_uses > 0 { jobj! { "maxUses": max_uses } } else { Json::obj() };
            Some((TOOL_NAME.to_owned(), messages_search_tool(tool_type, args)))
        }
        "google" | "vertex" => Some((
            "google_search".to_owned(),
            ProviderTool { id: "google.google_search", args: Json::obj(), supports_deferred_results: false },
        )),
        "xai" => Some((
            TOOL_NAME.to_owned(),
            ProviderTool { id: "xai.web_search", args: Json::obj(), supports_deferred_results: false },
        )),
        _ => None,
    }
}

/// The native page-fetch tool, or `None`. Only Anthropic has one whose result the host can read as text.
pub fn native_fetch_tool(
    family: &str,
    max_uses: u64,
    tool_type: Option<&str>,
    max_content_tokens: Option<u64>,
) -> Option<(String, ProviderTool)> {
    if family != "anthropic" && family != "bedrock" {
        return None;
    }
    let args = jobj! {
        "maxUses"?: (max_uses > 0).then_some(max_uses),
        "maxContentTokens"?: max_content_tokens.filter(|tokens| *tokens > 0),
    };
    Some(("web_fetch".to_owned(), messages_fetch_tool(tool_type, args)))
}

/// Whether this family supports native search; the host checks before deriving a task.
pub fn family_supports_native_search(family: &str) -> bool {
    matches!(family, "openai-responses" | "openai-codex" | "azure" | "anthropic" | "bedrock" | "google" | "vertex" | "xai" | "claude-agent")
}

pub fn family_supports_native_fetch(family: &str) -> bool {
    matches!(family, "anthropic" | "bedrock" | "claude-agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_by_family() {
        assert!(native_search_tool("openai-chat", 3, None).is_none());
        assert!(native_search_tool("openai-compatible", 3, None).is_none());
        let (name, tool) = native_search_tool("anthropic", 3, Some("web_search_20260209")).unwrap();
        assert_eq!((name.as_str(), tool.id), ("web_search", "anthropic.web_search_20260209"));
        assert_eq!(tool.args.to_text(), r#"{"maxUses":3}"#);
        let (_, tool) = native_search_tool("bedrock", 0, Some("web_search_unknown")).unwrap();
        assert_eq!((tool.id, tool.args.to_text().as_str()), ("anthropic.web_search_20250305", "{}"));
        assert_eq!(native_search_tool("vertex", 1, None).unwrap().0, "google_search");
        let (name, tool) = native_fetch_tool("anthropic", 2, None, Some(100)).unwrap();
        assert_eq!((name.as_str(), tool.id), ("web_fetch", "anthropic.web_fetch_20250910"));
        assert_eq!(tool.args.to_text(), r#"{"maxUses":2,"maxContentTokens":100}"#);
        assert!(native_fetch_tool("google", 2, None, None).is_none());
    }
}
