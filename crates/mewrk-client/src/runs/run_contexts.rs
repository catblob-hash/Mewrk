//! Port of `lib/runContexts.ts`: what a run's streamed state looks like as timeline contexts.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::capabilities::{is_encrypted_reasoning, is_preview_page_tool_name};
use super::context_tokens::{canonical_object, estimate_tokens};
use super::model_stream::{HostContextState, LiveContext, ModelRunState, StreamingHookState, StreamingHookStatus, StreamingToolPhase, StreamingToolState};
use super::util::now_iso;
use crate::i18n::t;
use crate::model::{ContextItem, Conversation, HookContextMetadata, ReasoningContent, ReasoningForm};

/// Derives the timeline identity for one round's prose (answer or reasoning).
/// The renderer and host use the same request-id-and-round inputs so streaming and
/// persisted contexts refer to the same timeline entry.
pub fn round_prose_context_id(request_id: &str, round: usize, kind: &str) -> String {
    format!("ctx_{kind}_{request_id}_{round}")
}

/// Shared grouping identity for one round's output. The host derives the same value.
pub fn round_model_turn_id(request_id: &str, round: usize) -> String {
    format!("model-turn-{request_id}-{round}")
}

/// Derives the identity for a round's `segment`-th reasoning item. It must match
/// the host's `reasoning_segment_identity`: segment zero uses the round base id
/// and later segments append `_<segment>`, allowing live rows to replace settled cards.
pub fn reasoning_segment_context_id(request_id: &str, round: usize, segment: usize) -> String {
    let base = round_prose_context_id(request_id, round, "reasoning");
    if segment == 0 { base } else { format!("{base}_{segment}") }
}

fn hook_context_id(request_id: &str, execution_id: &str, purpose: &str, index: usize) -> String {
    format!("ctx_hook_{request_id}_{execution_id}_{purpose}_{index}")
}

/// Context-id prefixes that may have been produced exclusively by this run.
/// Prose and hook contexts share host-derived ids.
pub fn run_stream_context_prefixes(request_id: &str) -> [String; 3] {
    [format!("ctx_assistant_{request_id}_"), format!("ctx_reasoning_{request_id}_"), format!("ctx_hook_{request_id}_")]
}

fn hook_stream_context(run: &ModelRunState, hook: &StreamingHookState, streaming: bool) -> LiveContext {
    let headline = match hook.status {
        StreamingHookStatus::Running => hook.status_message.as_deref().unwrap_or(t("正在执行", "Running")),
        StreamingHookStatus::Blocked => t("已阻止当前操作", "Blocked the current operation"),
        StreamingHookStatus::Failed => t("执行错误（未阻断）", "Failed without blocking"),
        StreamingHookStatus::Succeeded => t("执行完成", "Completed"),
    };
    let details = if hook.result.output.is_empty() { headline } else { hook.result.output.as_str() };
    let reason = hook.reason.as_deref().map(|reason| format!("\n\n{reason}")).unwrap_or_default();
    let detail_text = if details == headline { String::new() } else { format!("\n\n{details}") };
    let mut context = LiveContext::settled(ContextItem::System {
        id: hook_context_id(&run.request_id, &hook.execution_id, "display", 0),
        content: format!("[{} · {}]\n{}\n{headline}{reason}{detail_text}", t("生命周期钩子", "Lifecycle hook"), hook.event, hook.hook_name),
        local_only: true,
        hook_execution: Some(HookContextMetadata {
            execution_id: hook.execution_id.clone(),
            hook_id: hook.hook_id.clone(),
            hook_name: hook.hook_name.clone(),
            event: hook.event.clone(),
            status: hook.status.wire().to_owned(),
            context_injected: hook.context_injected,
        }),
        tools_added: Vec::new(),
        native_compaction: None,
        created_at: hook.created_at.clone(),
    });
    if streaming && hook.status == StreamingHookStatus::Running {
        context.streaming = Some(true);
    }
    context
}

/// The preview tool the model is currently driving the page with, for the task
/// card's "Model is driving: …" line and the stop-automation affordance.
///
/// Only the tools that own the page count. `preview_stop`, `preview_list` and
/// `preview_logs` read or kill a dev-server process without touching the page, so
/// a run doing nothing but reading logs must not present a page the user cannot
/// close. Each preview tool is its own wire tool now, so the name is the whole
/// answer — there is no `action` discriminant left to unwrap.
pub fn browser_automation_tool_for_run(run: Option<&ModelRunState>) -> Option<String> {
    let run = run?;
    let mut latest = None;
    for tools in run.streamed_tools_by_round.values() {
        for tool in tools {
            if is_preview_page_tool_name(&tool.tool_name) && matches!(tool.stream_status, StreamingToolPhase::Running | StreamingToolPhase::Completed) {
                latest = Some(tool.tool_name.clone());
            }
            for context in &tool.live.contexts {
                if let ContextItem::Tool { tool_name, .. } = &context.item {
                    if is_preview_page_tool_name(tool_name) && matches!(context.stream_status, Some(StreamingToolPhase::Running | StreamingToolPhase::Completed)) {
                        latest = Some(tool_name.clone());
                    }
                }
            }
        }
    }
    latest
}

/// The reasoning a run is doing right now, for the stream indicator beside the
/// cat. Derived from run state rather than from the projected contexts because
/// encrypted reasoning with no summary has no context at all until it finishes —
/// this line is the only surface it ever gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveReasoningView {
    /// When this round's reasoning opened.
    pub started_at: String,
    /// Tokens the round has thought so far. Every provider gets a figure: the
    /// provider's own reasoning usage when it reports one, otherwise an estimate —
    /// the reasoning text streamed so far plus whatever the sidecar estimated for
    /// thinking that streamed no text. Zero until there is anything to count.
    pub tokens: u64,
}

pub fn live_reasoning_from_model_run(run: Option<&ModelRunState>) -> Option<LiveReasoningView> {
    let run = run?;
    // The newest open round wins: an earlier round's reasoning closed when its
    // text or its tool calls began, whether or not a `reasoning_done` said so.
    let (&round, started_at) = run.reasoning_started_at_by_round.iter().rev().find(|(round, _)| run.completed_reasoning_by_round.get(round) != Some(&true))?;
    let reported = run.usage_by_round.get(&round).and_then(|usage| usage.reasoning_tokens).unwrap_or(0);
    let hidden: u64 = run.hidden_reasoning_tokens_by_round.get(&round).map_or(0, |items| items.values().sum());
    let streamed = estimate_tokens(&run.streamed_reasoning_by_round.get(&round).map(|items| items.concat()).unwrap_or_default());
    Some(LiveReasoningView { started_at: started_at.clone(), tokens: reported.max(hidden + streamed) })
}

/// What joined the transcript ahead of a round, in the host's order: the user's
/// steering and the host's own additions, each host context placed after the
/// steered inputs that had arrived before it.
fn joined_ahead_of_round(steered_inputs: &[ContextItem], host_contexts: &[HostContextState], out: &mut Vec<LiveContext>) {
    let mut steered = 0;
    for entry in host_contexts {
        while steered < entry.after_steered.min(steered_inputs.len()) {
            out.push(LiveContext::settled(steered_inputs[steered].clone()));
            steered += 1;
        }
        out.push(LiveContext::settled(entry.context.clone()));
    }
    out.extend(steered_inputs[steered..].iter().cloned().map(LiveContext::settled));
}

/// Projects a run onto the timeline. `streaming` keeps the live extras; `false` is the shape a settled run leaves.
pub fn contexts_from_model_run(run: &ModelRunState, streaming: bool) -> Vec<LiveContext> {
    // Use the request's actual model so live and settled reasoning cards use the same form.
    let reasoning_form: ReasoningForm = match run.request.model.reasoning_content {
        ReasoningContent::Plaintext => ReasoningForm::Plaintext,
        ReasoningContent::Encrypted => ReasoningForm::Encrypted,
    };
    let mut rounds: BTreeSet<usize> = BTreeSet::new();
    rounds.extend(run.streamed_reasoning_by_round.keys());
    // Encrypted-only reasoning has no streamed text, so its start timestamp must contribute a timeline round.
    rounds.extend(run.reasoning_started_at_by_round.keys());
    // Some providers report only reasoning-token usage; include those rounds so
    // their settled metadata card does not appear only after the run ends.
    rounds.extend(run.usage_by_round.iter().filter(|(_, usage)| usage.reasoning_tokens.unwrap_or(0) > 0).map(|(round, _)| round));
    rounds.extend(run.streamed_text_by_round.keys());
    rounds.extend(run.streamed_tools_by_round.keys());
    rounds.extend(run.streamed_hooks_by_round.keys());
    rounds.extend(run.steered_inputs_by_round.keys());
    rounds.extend(run.host_contexts_by_round.keys());

    let mut contexts: Vec<LiveContext> = Vec::new();
    for round in rounds {
        let model_turn_id = round_model_turn_id(&run.request_id, round);
        let reasoning_items = run.streamed_reasoning_by_round.get(&round).map_or(&[][..], Vec::as_slice);
        let reasoning_started_at = run.reasoning_started_at_by_round.get(&round);
        let text = run.streamed_text_by_round.get(&round);
        let tools = run.streamed_tools_by_round.get(&round).map_or(&[][..], Vec::as_slice);
        let hooks = run.streamed_hooks_by_round.get(&round).map_or(&[][..], Vec::as_slice);
        let steered_inputs = run.steered_inputs_by_round.get(&round).map_or(&[][..], Vec::as_slice);
        joined_ahead_of_round(steered_inputs, run.host_contexts_by_round.get(&round).map_or(&[][..], Vec::as_slice), &mut contexts);
        let hook_events = |events: &[&str]| hooks.iter().filter(move |hook| events.contains(&hook.event.as_str())).collect::<Vec<_>>();
        contexts.extend(hook_events(&["SessionStart", "UserPromptSubmit"]).into_iter().map(|hook| hook_stream_context(run, hook, streaming)));
        // Reasoning text is not the only evidence that a reasoning card exists:
        // encrypted-only reasoning has a start timestamp, and token-only reasoning
        // has usage metadata.
        let reasoning_tokens = run.usage_by_round.get(&round).and_then(|usage| usage.reasoning_tokens).filter(|&tokens| tokens > 0);
        // A token-only card has neither a start nor a delta, so it is not live.
        let reasoning_opened = !reasoning_items.is_empty() || reasoning_started_at.is_some();
        let reasoning_completed = run.completed_reasoning_by_round.get(&round) == Some(&true);
        let reasoning_body = reasoning_items.iter().any(|text| !text.is_empty());
        // Encrypted reasoning this round has not finished and has no summary yet.
        //
        // While it streams there is nothing a card could show — the stream
        // indicator narrates it beside the cat, and the card arrives once the
        // round's own duration does. If the round *ends* this way, there is still
        // nothing to show: no summary, and no ciphertext either, since that only
        // arrives with the `done` frame. A summary that did stream stays in either
        // case, live or interrupted, just like plaintext reasoning does: it is
        // readable text the user already saw. What an interrupted card never
        // carries is a replayable trace, and the host's wire projection drops every
        // interrupted reasoning card when the next turn is built.
        let withhold_reasoning = reasoning_form == ReasoningForm::Encrypted && reasoning_opened && !reasoning_completed && !reasoning_body;
        if !withhold_reasoning && (reasoning_opened || reasoning_tokens.is_some()) {
            // Derive one identity per reasoning item. Duration and token counts belong
            // only to the first row, and only the last row remains streaming.
            let row_count = reasoning_items.len().max(1);
            for item in 0..row_count {
                let last_row = item == row_count - 1;
                let mut card = LiveContext::settled(ContextItem::Reasoning {
                    id: reasoning_segment_context_id(&run.request_id, round, item),
                    content: Some(reasoning_items.get(item).cloned().unwrap_or_default()),
                    form: Some(run.reasoning_forms_by_round.get(&round).and_then(|forms| forms.get(&item)).copied().unwrap_or(reasoning_form)),
                    round: Some(round),
                    model_turn_id: Some(model_turn_id.clone()),
                    interrupted: false,
                    duration_ms: if item == 0 { run.reasoning_duration_by_round.get(&round).copied() } else { None },
                    // Responses reports zero rather than omitting tokens; omit zero to
                    // avoid displaying a meaningless `· 0`.
                    tokens: if item == 0 { reasoning_tokens } else { None },
                    replay: None,
                    created_at: run.started_at.clone(),
                });
                if streaming {
                    let active = run.active_reasoning_items_by_round.get(&round).and_then(|items| items.get(&item)).copied().unwrap_or(last_row);
                    card.streaming = Some(reasoning_opened && !reasoning_completed && active);
                }
                if item == 0 {
                    card.started_at = reasoning_started_at.filter(|at| !at.is_empty()).cloned();
                }
                contexts.push(card);
            }
        }
        if text.is_some_and(|text| !text.is_empty()) || !tools.is_empty() {
            let mut card = LiveContext::settled(ContextItem::Assistant {
                id: round_prose_context_id(&run.request_id, round, "assistant"),
                content: text.cloned().unwrap_or_default(),
                round: Some(round),
                model_turn_id: Some(model_turn_id.clone()),
                interrupted: false,
                sources: Vec::new(),
                created_at: run.started_at.clone(),
            });
            if streaming {
                card.streaming = Some(true);
            }
            contexts.push(card);
        }
        contexts.extend(hook_events(&["PreToolUse", "PermissionRequest"]).into_iter().map(|hook| hook_stream_context(run, hook, streaming)));
        // Only the backend mints the receipt that authorizes a tool result into
        // the document, and it mints it when the call actually finishes. A call
        // the stream left unfinished has no attested result, so persisting a
        // synthesized one gets the whole document rejected — and because every
        // later save carries that same context forward, the conversation stops
        // persisting entirely. Settling an interrupted run therefore keeps the
        // calls that finished and drops the rest.
        //
        // The child record is dropped for the same reason: it is renderer-built,
        // and only a backend-minted subagent receipt authorizes one. The host
        // mints those from the finished child transcript, which an interrupted
        // run never delivers.
        for tool in tools.iter().filter(|tool| streaming || tool.stream_status == StreamingToolPhase::Completed) {
            let requested_input = tool.requested_input.as_ref().filter(|requested| canonical_object(requested) != canonical_object(&tool.input)).cloned();
            let mut card = LiveContext::settled(ContextItem::Tool {
                id: tool.id.clone(),
                tool_name: tool.tool_name.clone(),
                round: Some(round),
                model_turn_id: Some(model_turn_id.clone()),
                provider_call_id: None,
                requested_input,
                input: tool.input.clone(),
                result: tool.result.clone(),
                subagent: None,
                notice: None,
                attestation: String::new(),
                created_at: tool.created_at.clone(),
            });
            if streaming {
                card.streaming = Some(true);
                card.stream_status = Some(tool.stream_status);
                card.live = Some(tool.live.clone());
            }
            contexts.push(card);
        }
        contexts.extend(hook_events(&["PostToolUse"]).into_iter().map(|hook| hook_stream_context(run, hook, streaming)));
        contexts.extend(hook_events(&["Stop"]).into_iter().map(|hook| hook_stream_context(run, hook, streaming)));
    }
    contexts
}

/// Applies a run's settled projection to a conversation. Replaceable ids are
/// limited to this run's prefixes, preserving contexts from other runs and user edits.
/// `false` when nothing materially changed.
pub fn apply_streamed_run_contexts(conversation: &mut Conversation, streamed: &[ContextItem], request_id: &str) -> bool {
    let prefixes = run_stream_context_prefixes(request_id);
    let is_replaceable = |id: &str| prefixes.iter().any(|prefix| id.starts_with(prefix.as_str()));
    fn text_of(context: &ContextItem) -> Option<&str> {
        match context {
            ContextItem::Tool { .. } => None,
            ContextItem::Reasoning { content, .. } => content.as_deref(),
            ContextItem::System { content, .. } | ContextItem::User { content, .. } | ContextItem::Assistant { content, .. } => Some(content),
        }
    }
    fn interrupted_of(context: &ContextItem) -> Option<bool> {
        match context {
            ContextItem::Assistant { interrupted, .. } | ContextItem::Reasoning { interrupted, .. } => Some(*interrupted),
            _ => None,
        }
    }
    let streamed_by_id: HashMap<&str, &ContextItem> = streamed.iter().rev().map(|context| (context.id(), context)).collect();
    let mut known_ids: HashSet<String> = conversation.contexts.iter().map(|context| context.id().to_owned()).collect();
    let delivered_queue_ids: HashSet<&str> = streamed.iter().filter(|context| matches!(context, ContextItem::User { .. })).map(ContextItem::id).collect();
    let mut changed = false;
    for context in &mut conversation.contexts {
        if !is_replaceable(context.id()) {
            continue;
        }
        let Some(fresh) = streamed_by_id.get(context.id()) else { continue };
        if text_of(fresh) == text_of(context) && interrupted_of(fresh) == interrupted_of(context) {
            continue;
        }
        *context = (*fresh).clone();
        changed = true;
    }
    let generated: Vec<ContextItem> = streamed.iter().filter(|context| known_ids.insert(context.id().to_owned())).cloned().collect();
    let queue_before = conversation.queued_messages.len();
    conversation.queued_messages.retain(|message| !delivered_queue_ids.contains(message.id.as_str()));
    if !changed && generated.is_empty() && conversation.queued_messages.len() == queue_before {
        return false;
    }
    conversation.contexts.extend(generated);
    conversation.updated_at = now_iso();
    true
}

/// What a stopped run leaves behind as plain contexts.
pub fn contexts_from_interrupted_run(run: &ModelRunState) -> Vec<ContextItem> {
    // Rounds whose every call carried a real result through to the end. Those
    // replay as an ordinary model turn. A round that lost a call to the
    // interrupt keeps the calls that finished — they executed and are attested —
    // but its visible text is only a fragment of the turn the model intended, so
    // the text is flagged and replay rebuilds the turn from the surviving calls.
    let settled_rounds: HashSet<usize> = run
        .streamed_tools_by_round
        .iter()
        .filter(|(_, tools)| !tools.is_empty() && tools.iter().all(|tool: &StreamingToolState| tool.stream_status == StreamingToolPhase::Completed))
        .map(|(round, _)| *round)
        .collect();
    contexts_from_model_run(run, false)
        .into_iter()
        .map(|context| {
            let mut item = context.item;
            if let ContextItem::Assistant { round, interrupted, .. } | ContextItem::Reasoning { round, interrupted, .. } = &mut item {
                if round.is_none_or(|round| !settled_rounds.contains(&round)) {
                    *interrupted = true;
                }
            }
            item
        })
        .collect()
}

/// Keeps local partial text from a cancelled stream while replacing any matching
/// live tool card with the backend's receipt-bearing terminal context. Contexts
/// that only the backend learned during teardown are appended once.
pub fn merge_finalized_interrupted_run_contexts(run: &ModelRunState, finalized_contexts: &[ContextItem]) -> Vec<ContextItem> {
    // An encrypted reasoning card the host recovered from a round that never
    // finished, with no summary to show: no text, and no ciphertext either, since
    // that only arrives with the `done` frame. The local projection withholds
    // the same round for the same reason; the host never mints such a card, but
    // these contexts bypass the projection's gate, so the rule is applied here
    // too. A fragment that does carry a summary stays, as it does locally.
    let abandoned: HashSet<&str> = finalized_contexts
        .iter()
        .filter(|context| matches!(context, ContextItem::Reasoning { interrupted: true, form, content, .. } if is_encrypted_reasoning(*form, content.as_deref()) && content.as_deref().unwrap_or("").is_empty()))
        .map(ContextItem::id)
        .collect();
    let finalized: Vec<&ContextItem> = finalized_contexts.iter().filter(|context| !abandoned.contains(context.id())).collect();
    let finalized_by_id: HashMap<&str, &ContextItem> = finalized.iter().rev().map(|context| (context.id(), *context)).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut merged = Vec::new();
    for context in contexts_from_interrupted_run(run) {
        if abandoned.contains(context.id()) {
            continue;
        }
        seen.insert(context.id().to_owned());
        let Some(&terminal) = finalized_by_id.get(context.id()) else {
            merged.push(context);
            continue;
        };
        let mut terminal = terminal.clone();
        match (&context, &mut terminal) {
            (ContextItem::Assistant { interrupted: true, .. }, ContextItem::Assistant { interrupted, .. })
            | (ContextItem::Reasoning { interrupted: true, .. }, ContextItem::Reasoning { interrupted, .. }) => *interrupted = true,
            _ => {}
        }
        merged.push(terminal);
    }
    for context in finalized {
        if seen.insert(context.id().to_owned()) {
            merged.push(context.clone());
        }
    }
    merged
}

/// The contexts of every group in order, each id once.
pub fn merge_unique_contexts<'a>(groups: impl IntoIterator<Item = &'a [ContextItem]>) -> Vec<ContextItem> {
    let mut ids: HashSet<&str> = HashSet::new();
    let mut merged = Vec::new();
    for group in groups {
        for context in group {
            if ids.insert(context.id()) {
                merged.push(context.clone());
            }
        }
    }
    merged
}

/// Backfills model-request inputs available only in the stream into authoritative
/// tool cards. Match by context id and attach `requested_input` only when it
/// differs from the card's executed input; receipt-bound fields must not be
/// inferred for an unrelated card.
pub fn backfill_requested_tool_input(contexts: &[ContextItem], run: &ModelRunState) -> Vec<ContextItem> {
    let streamed_by_id: HashMap<&str, &StreamingToolState> = run.streamed_tools_by_round.values().flatten().map(|tool| (tool.id.as_str(), tool)).collect();
    contexts
        .iter()
        .map(|context| {
            let mut context = context.clone();
            if let ContextItem::Tool { id, requested_input, input, .. } = &mut context {
                if requested_input.is_none() {
                    if let Some(requested) = streamed_by_id.get(id.as_str()).and_then(|tool| tool.requested_input.as_ref()) {
                        // Compare the authoritative card's executed input. The requested input only
                        // belongs on that card when it differs from its final arguments.
                        if canonical_object(requested) != canonical_object(input) {
                            *requested_input = Some(requested.clone());
                        }
                    }
                }
            }
            context
        })
        .collect()
}
