//! Host-attested conversation forking.
//!
//! A branch is an independent conversation that owns a full copy of the history
//! it forked from. Copying is deliberately done here rather than in the
//! renderer because two document invariants forbid a naive client-side copy:
//!
//! 1. Context ids are unique within a conversation
//!    ([`crate::storage::validate_shape`]), and an id is also what says what
//!    some cards are: an appended instruction (`ctx_system-append_…`), a host
//!    notice (`ctx_agent-result_…`), a continuation's notebook index. A copy
//!    keeps every card's kind and renews only the unique part ([`fork_id`],
//!    [`template_id`]), so the branch sends the model exactly what the source
//!    did above the cut.
//! 2. A tool result is only persistable when the host holds an execution
//!    receipt binding it to *that* conversation. Receipts intentionally do not
//!    move between conversations — `tool_context_and_receipt_cannot_move_between_conversations`
//!    exists to keep a renderer from relocating a result it did not execute.
//!
//! Re-attesting here preserves that guarantee instead of weakening it. The host
//! reads the source contexts from its own committed document, so the copy is
//! known-good output the host itself previously produced and validated; the
//! renderer cannot smuggle a forged result in through this path, because
//! nothing it sends is copied — it only names a source conversation and a cut
//! point.
//!
//! Image attachments are shared by reference. The sidecar store is content
//! addressed and reference counted at save time, so a branch that cites the
//! same attachment id keeps it alive without duplicating bytes.

use std::collections::HashMap;

use crate::{
    model::{ContextItem, Conversation, SubagentRunRecord, ToolExecutionRequest, ToolResult},
    state::AppState,
};

/// Whether `target_conversation_id` may receive a forked history.
///
/// The renderer creates the branch conversation and flushes it to disk *before*
/// calling the host, because the host copies from its own committed document and
/// therefore has to be able to see the target. A guard that rejected every
/// existing target contradicted that: the renderer wrote the conversation to
/// satisfy the host, and the host refused because it was written. Every fork
/// past the first message failed deterministically.
///
/// The guard still exists — it just tests the thing that actually matters. A
/// target that already holds history would have that history silently replaced
/// by the copy, so it is still rejected. An empty target is the expected state.
pub fn validate_fork_target(
    target: Option<&Conversation>,
    target_conversation_id: &str,
) -> Result<(), String> {
    let Some(target) = target else {
        return Ok(());
    };
    if !target.contexts.is_empty() {
        return Err(format!(
            "分支目标对话 {target_conversation_id} 已有历史，无法作为分支目标"
        ));
    }
    Ok(())
}

/// Where the copy stops, expressed as an id rather than an index so a
/// concurrent edit cannot silently shift the cut point.
pub struct ForkRequest<'a> {
    pub workspace_path: &'a str,
    pub target_conversation_id: &'a str,
    /// The last context to include. Everything after it is dropped.
    pub through_context_id: &'a str,
}

/// The id a card takes in a branch of its conversation. An id minted with a
/// random tail (`ctx_<kind>_<uuid>`) gets a fresh tail and keeps its kind; any
/// other id — one the host derived from what the card is, like a notebook
/// index or a skill's notice — is kept, since it names the same thing in the
/// branch and ids only have to be unique within one conversation.
pub fn fork_id(source: &str) -> String {
    match source.rsplit_once('_') {
        Some((kind, tail)) if is_minted_tail(tail) => {
            format!("{kind}_{}", uuid::Uuid::new_v4().simple())
        }
        _ => source.to_owned(),
    }
}

/// The id a card takes when a template is applied. A template can be applied
/// twice into one conversation, so every id is renewed — a minted tail
/// replaced, any other id extended — and still begins with its kind.
pub fn template_id(source: &str) -> String {
    match source.rsplit_once('_') {
        Some((kind, tail)) if is_minted_tail(tail) => {
            format!("{kind}_{}", uuid::Uuid::new_v4().simple())
        }
        _ => format!("{source}_{}", uuid::Uuid::new_v4().simple()),
    }
}

/// Whether an id segment is a simple UUID, the tail `api::new_context_id` mints.
fn is_minted_tail(tail: &str) -> bool {
    tail.len() == 32 && tail.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Copies `contexts` up to and including `through_context_id`, renewing every
/// id with `new_id` (given the source id; [`fork_id`] or [`template_id`]) and
/// re-attesting every tool result against `target_conversation_id`.
pub fn fork_contexts(
    state: &AppState,
    request: &ForkRequest<'_>,
    contexts: &[ContextItem],
    mut new_id: impl FnMut(&str) -> String,
) -> Result<Vec<ContextItem>, String> {
    let cut = contexts
        .iter()
        .position(|context| context.id() == request.through_context_id)
        .ok_or_else(|| format!("分支起点 {} 不在源对话中", request.through_context_id))?;

    // `modelTurnId` groups the items one model round emitted. It is a local
    // association, not a provider id, so it is remapped consistently rather
    // than carried over: the branch is a separate session and must not claim
    // the source's turn identities.
    let mut turn_ids = HashMap::<String, String>::new();
    let mut forked = Vec::with_capacity(cut + 1);

    for context in &contexts[..=cut] {
        let id = new_id(context.id());
        let remap = |turn: &Option<String>, turn_ids: &mut HashMap<String, String>| {
            turn.as_ref().map(|source| {
                turn_ids
                    .entry(source.clone())
                    .or_insert_with(|| new_turn_id())
                    .clone()
            })
        };

        forked.push(match context {
            ContextItem::System {
                content,
                local_only,
                hook_execution,
                tools_added,
                native_compaction,
                created_at,
                ..
            } => ContextItem::System {
                id,
                content: content.clone(),
                local_only: *local_only,
                hook_execution: hook_execution.clone(),
                // The tools that joined here joined the branch here too.
                tools_added: tools_added.clone(),
                // The branch talks to the same account, so the compaction
                // stands in for the history above it there as well.
                native_compaction: native_compaction.clone(),
                created_at: created_at.clone(),
            },
            ContextItem::User {
                content,
                images,
                files,
                created_at,
                ..
            } => ContextItem::User {
                id,
                content: content.clone(),
                images: images.clone(),
                files: files.clone(),
                created_at: created_at.clone(),
            },
            ContextItem::Assistant {
                content,
                round,
                model_turn_id,
                interrupted,
                created_at,
                ..
            } => ContextItem::Assistant {
                id,
                content: content.clone(),
                round: *round,
                model_turn_id: remap(model_turn_id, &mut turn_ids),
                interrupted: *interrupted,
                sources: Vec::new(),
                created_at: created_at.clone(),
            },
            ContextItem::Reasoning {
                content,
                form,
                round,
                model_turn_id,
                interrupted,
                duration_ms,
                tokens,
                replay,
                created_at,
                ..
            } => ContextItem::Reasoning {
                id,
                content: content.clone(),
                // Preserve the card's original form. A fork may use a different
                // model, but moving the card does not change the model that produced it.
                form: *form,
                round: *round,
                model_turn_id: remap(model_turn_id, &mut turn_ids),
                interrupted: *interrupted,
                // Preserve metadata so reasoning cards with only encrypted content remain
                // visible in the timeline after the fork.
                duration_ms: *duration_ms,
                tokens: *tokens,
                // The signed payload stays with the card: a fork on the same
                // model replays it exactly like the source conversation would.
                replay: replay.clone(),
                created_at: created_at.clone(),
            },
            ContextItem::Tool {
                tool_name,
                round,
                model_turn_id,
                provider_call_id,
                requested_input,
                input,
                result,
                subagent,
                notice,
                created_at,
                ..
            } => {
                // The copy is only persistable if the host vouches for it in the
                // destination conversation. This is the same attestation the
                // original execution produced, re-issued for the new owner.
                attest(
                    state,
                    request,
                    tool_name,
                    input,
                    requested_input.as_ref(),
                    result,
                    subagent.as_ref(),
                );
                // The token is bound to the conversation and the card id, both
                // of which the fork changes, so the original cannot be carried
                // over — it is re-issued for the copy in its new home.
                let attestation =
                    state.attest_tool_context(&crate::tool_attestation::AttestationSubject {
                        conversation_id: request.target_conversation_id,
                        context_id: &id,
                        tool_name,
                        input,
                        requested_input: requested_input.as_ref(),
                        result,
                        subagent: subagent.as_ref(),
                    });
                ContextItem::Tool {
                    id,
                    tool_name: tool_name.clone(),
                    round: *round,
                    model_turn_id: remap(model_turn_id, &mut turn_ids),
                    // Carried over, unlike the attestation: the id is not bound
                    // to a conversation, and the fork is supposed to replay the
                    // exchange exactly as the source would. It stays unique in
                    // its new home because it was unique in the old one.
                    provider_call_id: provider_call_id.clone(),
                    requested_input: requested_input.clone(),
                    input: input.clone(),
                    result: result.clone(),
                    subagent: subagent.clone(),
                    notice: notice.clone(),
                    attestation,
                    created_at: created_at.clone(),
                }
            }
        });
    }

    Ok(forked)
}

fn new_turn_id() -> String {
    format!("turn_{}", uuid::Uuid::new_v4().simple())
}

fn attest(
    state: &AppState,
    request: &ForkRequest<'_>,
    tool_name: &str,
    input: &crate::model::JsonObject,
    requested_input: Option<&crate::model::JsonObject>,
    result: &ToolResult,
    subagent: Option<&SubagentRunRecord>,
) {
    let execution = ToolExecutionRequest {
        conversation_id: request.target_conversation_id.to_owned(),
        workspace_path: request.workspace_path.to_owned(),
        tool_name: tool_name.to_owned(),
        input: input.clone(),
    };
    match subagent {
        Some(subagent) => {
            state.record_context_subagent_receipt(&execution, result, requested_input, subagent)
        }
        None => state.record_context_receipt(&execution, result, requested_input),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{catalog::default_document, storage::validate_save_transition};

    fn sequential_ids() -> impl FnMut(&str) -> String {
        let mut next = 0usize;
        move |prefix| {
            next += 1;
            format!("{prefix}_forked_{next}")
        }
    }

    #[test]
    fn fork_copies_history_through_the_cut_point_with_fresh_ids() {
        let document = default_document();
        let workspace = &document.workspaces[0];
        let source = &workspace.conversations[0];
        let state = AppState::default();

        let forked = fork_contexts(
            &state,
            &ForkRequest {
                workspace_path: &workspace.path,
                target_conversation_id: "conv_branch",
                through_context_id: "ctx_welcome_tool",
            },
            &source.contexts,
            sequential_ids(),
        )
        .unwrap();

        // Everything through the tool call is copied; the assistant reply after
        // it is not.
        assert_eq!(forked.len(), 4);
        for (copy, original) in forked.iter().zip(&source.contexts) {
            assert!(
                copy.id().starts_with(&format!("{}_forked_", original.id())),
                "{} from {}",
                copy.id(),
                original.id()
            );
        }
        assert!(matches!(forked[3], ContextItem::Tool { .. }));

        // Content is preserved verbatim, only identity changes.
        let (
            ContextItem::User { content, .. },
            ContextItem::User {
                content: source_content,
                ..
            },
        ) = (&forked[1], &source.contexts[1])
        else {
            panic!("seed index 1 must be a user context");
        };
        assert_eq!(content, source_content);
    }

    #[test]
    fn a_copy_keeps_every_cards_kind_and_renews_what_must_be_unique() {
        let minted = format!("ctx_agent-result_{}", uuid::Uuid::new_v4().simple());
        let renewed = fork_id(&minted);
        assert!(renewed.starts_with("ctx_agent-result_") && renewed != minted, "{renewed}");
        let appended = format!("ctx_system-append_plan-mode_{}", uuid::Uuid::new_v4().simple());
        assert!(fork_id(&appended).starts_with("ctx_system-append_plan-mode_"));
        // Derived ids name the same thing in the branch.
        assert_eq!(fork_id(crate::handoff::INDEX_CONTEXT_ID), crate::handoff::INDEX_CONTEXT_ID);
        assert_eq!(fork_id("ctx_agent-result_skill_review"), "ctx_agent-result_skill_review");
        // A template may land twice in one conversation, so nothing is kept.
        assert!(template_id(&minted).starts_with("ctx_agent-result_"));
        assert_ne!(template_id(&minted), minted);
        let derived = template_id("ctx_agent-result_skill_review");
        assert!(derived.starts_with("ctx_agent-result_skill_review_"), "{derived}");
    }

    /// Above the cut, the branch sends the model what the source did: the
    /// host's notices stay host notices, an appended instruction stays one, and
    /// a tool addition stays where it joined.
    #[test]
    fn a_branch_projects_the_same_history_as_its_source() {
        let state = AppState::default();
        let appended = crate::system_append::card("plan-mode", "Plan first.".into(), "2026-10-03T00:00:00Z".into());
        let notice = ContextItem::Tool {
            id: format!("ctx_agent-result_{}", uuid::Uuid::new_v4().simple()),
            tool_name: crate::wire_history::HOST_CARD_TOOL.into(),
            round: Some(1),
            model_turn_id: None,
            provider_call_id: None,
            requested_input: None,
            input: Default::default(),
            result: ToolResult {
                success: true,
                output: crate::wire_history::system_reminder("Body."),
                images: Vec::new(),
                diff: None,
                executed_at: "2026-10-03T00:00:00Z".into(),
                duration_ms: 0,
            },
            subagent: None,
            notice: Some(crate::wire_history::notice_kind::HOOK_CONTEXT.into()),
            attestation: String::new(),
            created_at: "2026-10-03T00:00:00Z".into(),
        };
        let addition = ContextItem::System {
            id: format!("ctx_tool-append_{}", uuid::Uuid::new_v4().simple()),
            content: String::new(),
            local_only: true,
            hook_execution: None,
            tools_added: vec!["handoff".into()],
            native_compaction: None,
            created_at: "2026-10-03T00:00:00Z".into(),
        };
        let question = ContextItem::User {
            id: "ctx_question".into(),
            content: "Go on.".into(),
            images: Vec::new(),
            files: Vec::new(),
            created_at: "2026-10-03T00:00:01Z".into(),
        };
        let follow_up = ContextItem::User {
            id: "ctx_follow_up".into(),
            content: "And then?".into(),
            images: Vec::new(),
            files: Vec::new(),
            created_at: "2026-10-03T00:00:02Z".into(),
        };
        let source = vec![question, appended, notice, addition, follow_up];
        let forked = fork_contexts(
            &state,
            &ForkRequest {
                workspace_path: "/w",
                target_conversation_id: "conv_branch",
                through_context_id: source.last().unwrap().id(),
            },
            &source,
            fork_id,
        )
        .unwrap();
        assert_eq!(
            format!("{:?}", crate::wire_history::canonical_history(&forked))
                .replace(forked[2].id(), source[2].id())
                .replace(forked[1].id(), source[1].id()),
            format!("{:?}", crate::wire_history::canonical_history(&source))
        );
        assert!(crate::system_append::is_appended(&forked[1]));
        assert!(crate::wire_history::host_delivery(&forked[2]).is_some());
    }

    #[test]
    fn forked_tool_results_are_attested_for_the_branch_and_persist() {
        let previous = default_document();
        let mut next = previous.clone();
        let workspace_path = previous.workspaces[0].path.clone();
        let state = AppState::default();

        let forked = fork_contexts(
            &state,
            &ForkRequest {
                workspace_path: &workspace_path,
                target_conversation_id: "conv_branch",
                through_context_id: "ctx_welcome_tool",
            },
            &previous.workspaces[0].conversations[0].contexts,
            sequential_ids(),
        )
        .unwrap();

        let mut branch = previous.workspaces[0].conversations[0].clone();
        branch.id = "conv_branch".into();
        branch.contexts = forked;
        branch.branches.clear();
        next.workspaces[0].conversations.push(branch);

        // The copied tool result carries a host receipt bound to the branch, so
        // the whole branch saves. Without `fork_contexts` this is exactly the
        // transition `tool_context_and_receipt_cannot_move_between_conversations`
        // rejects.
        validate_save_transition(&previous, &next, &state).expect("forked branch must persist");
    }

    #[test]
    fn fork_target_may_already_exist_when_the_renderer_flushed_it_empty() {
        let document = default_document();
        let mut target = document.workspaces[0].conversations[0].clone();
        target.id = "conv_branch".into();
        target.contexts.clear();
        target.branches.clear();

        // This is exactly the state `branchFromUserContext` puts on disk before
        // it calls the host: the branch exists and is empty. Rejecting it made
        // every fork past the first message fail deterministically.
        validate_fork_target(Some(&target), "conv_branch")
            .expect("an empty flushed target is the expected state, not a duplicate");
        // A target the host has never seen is equally fine.
        validate_fork_target(None, "conv_branch").expect("a missing target must be accepted");
    }

    #[test]
    fn fork_target_that_already_holds_history_is_rejected() {
        let document = default_document();
        let mut target = document.workspaces[0].conversations[0].clone();
        target.id = "conv_branch".into();
        assert!(
            !target.contexts.is_empty(),
            "fixture must carry history for this guard to mean anything"
        );

        // The copy replaces `contexts` wholesale, so a target with history would
        // silently lose it. That is what the guard is actually for.
        let error = validate_fork_target(Some(&target), "conv_branch")
            .expect_err("a target holding history must be rejected");
        assert!(
            error.contains("conv_branch"),
            "error must name the target: {error}"
        );
    }

    #[test]
    fn fork_rejects_a_cut_point_outside_the_source() {
        let document = default_document();
        let workspace = &document.workspaces[0];
        let state = AppState::default();

        let error = fork_contexts(
            &state,
            &ForkRequest {
                workspace_path: &workspace.path,
                target_conversation_id: "conv_branch",
                through_context_id: "ctx_not_in_this_conversation",
            },
            &workspace.conversations[0].contexts,
            sequential_ids(),
        )
        .unwrap_err();

        assert!(error.contains("ctx_not_in_this_conversation"));
    }

    #[test]
    fn fork_remaps_model_turn_ids_consistently_without_reusing_the_source_identity() {
        let document = default_document();
        let workspace = &document.workspaces[0];
        let state = AppState::default();
        let mut contexts = workspace.conversations[0].contexts.clone();
        for context in &mut contexts {
            if let ContextItem::Reasoning { model_turn_id, .. }
            | ContextItem::Tool { model_turn_id, .. } = context
            {
                *model_turn_id = Some("turn_source".into());
            }
        }

        let forked = fork_contexts(
            &state,
            &ForkRequest {
                workspace_path: &workspace.path,
                target_conversation_id: "conv_branch",
                through_context_id: "ctx_welcome_tool",
            },
            &contexts,
            sequential_ids(),
        )
        .unwrap();

        let turn_ids = forked
            .iter()
            .filter_map(|context| match context {
                ContextItem::Reasoning { model_turn_id, .. }
                | ContextItem::Tool { model_turn_id, .. } => model_turn_id.clone(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(turn_ids.len(), 2);
        // One round stays one round in the branch, under a new local identity.
        assert_eq!(turn_ids[0], turn_ids[1]);
        assert_ne!(turn_ids[0], "turn_source");
    }
}
