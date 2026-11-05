//! `lib/timelineClipboard.ts`: copying contexts off a timeline and pasting them onto one.
//!
//! What is copied is held by the app, not by the system clipboard: a context is more than its text, and what comes back
//! has to be the cards themselves. The text goes to the system clipboard as well, so a copy pastes somewhere else as
//! something readable. A paste never puts the copied contexts themselves down: each is a new card with a new id.

use std::collections::{HashMap, HashSet};

use mewrk_client::model::{ContextItem, ToolCategory, ToolDescriptor};
use mewrk_client::runs::image_short_ids::renumber_images_for_send;

/// Contexts copied off a timeline, as they were when copied.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineClipboard {
    /// The conversation they were copied out of, which a tool card's proof names.
    pub conversation_id: String,
    pub contexts: Vec<ContextItem>,
}

/// Ids the host mints for cards of its own. The id is how the host recognises such a card, so a copy under a new id would
/// be a different thing, and one under the same prefix a forgery.
const HOST_CARD_ID_PREFIXES: [&str; 7] = ["ctx_agent-result_", "ctx_agent-message_", "ctx_structured-output-nudge_", "ctx_system-append_", "ctx_handoff_index", "ctx_skill_", "ctx_hook-continuation_"];

/// `timelineCopyable`: a context whose meaning is its own content, not its place in a run. `streaming` is what the
/// rendered timeline says of it; only persisted contexts reach a clipboard otherwise.
pub fn timeline_copyable(context: &ContextItem, streaming: bool, tools: &[ToolDescriptor]) -> bool {
    if streaming || HOST_CARD_ID_PREFIXES.iter().any(|prefix| context.id().starts_with(prefix)) {
        return false;
    }
    match context {
        ContextItem::System { native_compaction, tools_added, hook_execution, .. } => native_compaction.is_none() && tools_added.is_empty() && hook_execution.is_none(),
        ContextItem::Tool { subagent, notice, tool_name, .. } => {
            subagent.is_none() && notice.is_none() && tools.iter().find(|tool| tool.name == *tool_name).is_none_or(|tool| tool.category != ToolCategory::Orchestration)
        }
        _ => true,
    }
}

/// `timelineMovable`: every request starts from the last compaction, so one carried down the timeline would silently drop
/// everything above where it landed.
pub fn timeline_movable(context: &ContextItem) -> bool {
    !matches!(context, ContextItem::System { native_compaction: Some(_), .. })
}

/// `timelineClipboardText`: what copied contexts read as anywhere that takes only text.
pub fn timeline_clipboard_text(contexts: &[ContextItem]) -> String {
    contexts
        .iter()
        .map(|context| match context {
            // serde_json's compact form is what `JSON.stringify` writes.
            ContextItem::Tool { tool_name, input, result, .. } => format!("{tool_name} {}\n{}", serde_json::to_string(input).unwrap_or_default(), result.output).trim().to_owned(),
            ContextItem::User { content, .. } | ContextItem::Assistant { content, .. } | ContextItem::System { content, .. } => content.trim().to_owned(),
            ContextItem::Reasoning { content, .. } => content.as_deref().unwrap_or("").trim().to_owned(),
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `PasteCopies`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PasteCopies {
    /// The copies, in order. A tool card among them has no proof yet.
    pub contexts: Vec<ContextItem>,
    /// Each copied tool card with the card it copies, for the host to prove and vouch for.
    pub tools: Vec<(String, ContextItem)>,
    /// How many copied contexts could not be pasted here.
    pub skipped: usize,
}

/// `copiesForPaste`: the cards a paste puts down. Every card gets a new id; a user message's images are numbered again
/// against what the destination uses; a tool card's numbers are written into its attested result, so it is pasted only
/// where they do not clash; what one round emitted stays one group under a turn id of its own; the provider's call id is
/// not carried over, since two calls answering to one id cannot both go to the provider; cited sources are left behind.
pub fn copies_for_paste(clipboard: &TimelineClipboard, destination_conversation_id: &str, taken_image_ids: &mut HashSet<u32>, new_id: &mut dyn FnMut(&str) -> String) -> PasteCopies {
    let mut turn_ids: HashMap<String, String> = HashMap::new();
    let same_conversation = clipboard.conversation_id == destination_conversation_id;
    let mut copies = PasteCopies::default();
    let mut turn_of = |source: &Option<String>, new_id: &mut dyn FnMut(&str) -> String| -> Option<String> {
        let source = source.as_ref()?;
        Some(turn_ids.entry(source.clone()).or_insert_with(|| new_id("turn")).clone())
    };
    for source in &clipboard.contexts {
        match source {
            ContextItem::User { content, images, files, created_at, .. } => {
                let (content, images) = if images.is_empty() {
                    (content.clone(), images.clone())
                } else {
                    // Numbers are handed out across the whole paste, so two copies never share one.
                    let (text, numbered) = renumber_images_for_send(taken_image_ids.clone(), content, images);
                    taken_image_ids.extend(numbered.iter().filter_map(|image| image.short_id));
                    (text, numbered)
                };
                copies.contexts.push(ContextItem::User { id: new_id("ctx"), content, images, files: files.clone(), created_at: created_at.clone() });
            }
            ContextItem::System { .. } => {
                let mut copy = source.clone();
                if let ContextItem::System { id, .. } = &mut copy {
                    *id = new_id("ctx");
                }
                copies.contexts.push(copy);
            }
            ContextItem::Assistant { content, round, model_turn_id, interrupted, created_at, .. } => {
                let turn = turn_of(model_turn_id, new_id);
                copies.contexts.push(ContextItem::Assistant { id: new_id("ctx"), content: content.clone(), round: *round, model_turn_id: turn, interrupted: *interrupted, sources: Vec::new(), created_at: created_at.clone() });
            }
            ContextItem::Reasoning { .. } => {
                let mut copy = source.clone();
                if let ContextItem::Reasoning { id, model_turn_id, .. } = &mut copy {
                    *model_turn_id = turn_of(model_turn_id, new_id);
                    *id = new_id("ctx");
                }
                copies.contexts.push(copy);
            }
            ContextItem::Tool { result, .. } => {
                let image_ids: Vec<u32> = result.images.iter().filter_map(|image| image.short_id).collect();
                if !same_conversation && image_ids.iter().any(|id| taken_image_ids.contains(id)) {
                    copies.skipped += 1;
                    continue;
                }
                taken_image_ids.extend(image_ids);
                let mut copy = source.clone();
                if let ContextItem::Tool { id, provider_call_id, attestation, model_turn_id, .. } = &mut copy {
                    *provider_call_id = None;
                    attestation.clear();
                    *model_turn_id = turn_of(model_turn_id, new_id);
                    *id = new_id("ctx");
                }
                copies.tools.push((copy.id().to_owned(), source.clone()));
                copies.contexts.push(copy);
            }
        }
    }
    copies
}

/// `withCopiedToolProofs`: the copies with each tool card carrying the proof the host issued for it.
pub fn with_copied_tool_proofs(copies: &PasteCopies, proofs: &[String]) -> Vec<ContextItem> {
    let proof_by_id: HashMap<&str, &String> = copies.tools.iter().zip(proofs).map(|((id, _), proof)| (id.as_str(), proof)).collect();
    copies
        .contexts
        .iter()
        .map(|context| {
            let mut context = context.clone();
            if let ContextItem::Tool { id, attestation, .. } = &mut context
                && let Some(proof) = proof_by_id.get(id.as_str())
            {
                *attestation = (*proof).clone();
            }
            context
        })
        .collect()
}
