//! The questions the send path asks that live elsewhere upstream but have no port yet, on top of those that do.

pub use crate::model_capabilities::{appends_tools, has_usable_base_url, is_encrypted_reasoning, supports_vision, takes_native_compaction};
pub use crate::task_tools::is_preview_page_tool_name;
pub use crate::tool_lock::family_supports_native_fetch;

use crate::model::{ContextItem, ConversationWebSearchSettings, FetchProviderSelection, ProviderFamily};
use crate::task_tools::is_memory_tool_name;

/// `contextsContainProjectedImages` of `lib/imageBudget.ts`.
pub fn contexts_contain_projected_images(contexts: &[ContextItem]) -> bool {
    contexts.iter().any(|item| match item {
        ContextItem::User { images, .. } => !images.is_empty(),
        ContextItem::Tool { tool_name, result, .. } => !is_memory_tool_name(tool_name) && !result.images.is_empty(),
        _ => false,
    })
}

/// `nativeSearchRan` of `lib/webSearch.ts`: a finished native search left a report whose envelope has `findings`. On
/// every other family the call fails before it is sent, so this is what "Native has actually searched here" means to
/// the tool lock.
pub fn native_search_ran(contexts: &[ContextItem]) -> bool {
    contexts.iter().any(|item| match item {
        ContextItem::Tool { tool_name, result, .. } if tool_name == "web_search" && result.success => {
            serde_json::from_str::<serde_json::Value>(&result.output).is_ok_and(|envelope| envelope.get("findings").is_some_and(serde_json::Value::is_string))
        }
        _ => false,
    })
}

/// `grantsWebFetch` of `lib/webSearch.ts`: mirrors Rust `WebSearchSettings::fetch_withheld`, the fetch half of the one
/// rule `web_search::apply_web_tools` applies to both legs. The tool lock records which web tools a run put in front
/// of the model.
pub fn grants_web_fetch(web_search_enabled: bool, conversation: &ConversationWebSearchSettings, family: Option<ProviderFamily>) -> bool {
    if !web_search_enabled {
        return false;
    }
    match conversation.fetch_provider {
        FetchProviderSelection::Disabled => false,
        FetchProviderSelection::Native => family_supports_native_fetch(family),
        FetchProviderSelection::Explicit { .. } | FetchProviderSelection::Unavailable => true,
    }
}

/// Host-authored user contexts (`isHostAuthoredUserContext`): they appear in the timeline but close no round.
pub fn is_host_authored_user_context(context: &ContextItem) -> bool {
    match context {
        ContextItem::User { id, .. } => ["ctx_agent-result_", "ctx_agent-message_", "ctx_structured-output-nudge_"].iter().any(|prefix| id.starts_with(prefix)),
        _ => false,
    }
}
