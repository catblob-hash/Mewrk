//! Port of `lib/sendPipeline.ts`: everything that starts, follows and settles a model run.
//!
//! Upstream's `async` functions run on task threads here (`Cx`), and each stretch between two `await`s is one
//! `Cx::update`, so state is touched exactly where it was. What a function does before its first `await` stays
//! synchronous on the UI thread: guards like the preparing flag must be up before anything else can run.

use std::collections::HashSet;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::attachments::{self, AttachmentRejection, AttachmentRejectionReason, ComposerFile, Intake, MessageAttachmentRoom};
use super::bridge::{AttachRunResult, Cx, RunSettlementPayload};
use super::capabilities::{contexts_contain_projected_images, grants_web_fetch, has_usable_base_url, native_search_ran, supports_vision};
use super::composer::{expand_pasted_texts, with_selected_elements, Deferred, UploadBatch, UploadTicket};
use super::context_tokens::estimate_contexts_tokens;
use super::conversation_queue::queue_is_paused;
use super::conversation_turns::{self, annotate_turn_failure, clear_turn_failures, ConversationTurnError, ConversationTurnStatus};
use super::image_short_ids::{
    image_placeholder_ids, image_short_ids_in_use, next_image_short_id, renumber_images_for_send, reserve_queued_message_ids, strip_image_placeholder, text_without_image_placeholders, with_image_placeholders,
};
use super::model_stream::{cumulative_model_run_usage, reduce_model_stream_event, trim_live_context_budget, ModelRunState, ModelStreamEffect, MODEL_STREAM_COMMIT_INTERVAL_MS};
use super::run_contexts::{
    apply_streamed_run_contexts, backfill_requested_tool_input, contexts_from_interrupted_run, contexts_from_model_run, merge_finalized_interrupted_run_contexts, merge_unique_contexts, run_stream_context_prefixes,
};
use super::seams::{self, find_conversation, model_choice, workspace_of};
use super::state::{RunsState, StreamBuffer};
use super::types::*;
use super::util::{create_id, exceeds_chars, now_iso, slice_utf16};
use crate::i18n::{t, tf};
use crate::model::{ApiProvider, ContextItem, Conversation, FileAttachment, ImageAttachment, ModelProfile, ModelStreamEvent, QueuedMessage, RunModelResponse, ToolDescriptor};
use crate::tool_lock::{refreshed_tool_lock, with_run_tool_lock, ToolLockRequestContext};
use crate::App;

/// Which part of a failure to forget (`clearModelRunError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClearErrorScope {
    /// Retracts only the composer notice — correct when reattaching a surviving host run, which is the *same* attempt
    /// continuing rather than a new one.
    Notice,
    /// Also retracts the turn-level notices and the header-only turns that existed to carry them, which is what makes
    /// sending again the dismissal gesture. Adoption must never use this: it would delete the record of the very
    /// failure the user reloaded to read.
    Failure,
}

fn runs(app: &mut App) -> &mut RunsState {
    app.local::<RunsState>()
}

/// Whether a run may start, from the configuration alone. Upstream spells this at every entry point with
/// `hasUsableBaseUrl`, because providers whose chat endpoint is derived keep an empty address on purpose.
fn usable_choice(app: &App) -> Option<(ApiProvider, ModelProfile)> {
    let (provider, model) = model_choice(&app.doc);
    let provider = provider.filter(|provider| provider.enabled && has_usable_base_url(provider))?;
    let model = model.filter(|model| !model.id.trim().is_empty())?;
    Some((provider.clone(), model.clone()))
}

fn enabled_tools_within(conversation: &Conversation, tools: &[ToolDescriptor]) -> Vec<String> {
    conversation.settings.enabled_tools.iter().filter(|name| tools.iter().any(|tool| &tool.name == *name)).cloned().collect()
}

// ---------------------------------------------------------------- App's turn seams

/// `lockConversationTools`: records the surface and model a run's request goes out with as the conversation's lock.
/// The composer's own send already folds this into its durable write; this covers the runs that start without a new
/// user message.
fn lock_conversation_tools(app: &mut App, workspace_id: &str, conversation_id: &str, tools: &[String], provider_id: &str, model_id: &str) {
    let family = model_choice(&app.doc).0.map(|provider| provider.family);
    let Some(current) = find_conversation(&app.doc, workspace_id, conversation_id) else { return };
    // The same question the composer asks before its own send: did this run
    // put `web_fetch` in front of the model, or only `web_search`?
    let web_fetch = grants_web_fetch(current.settings.web_search_enabled, &current.settings.web_search, family);
    let at = now_iso();
    app.update_conversation_if(workspace_id, conversation_id, |conversation| {
        let context = ToolLockRequestContext {
            web_fetch,
            native_searched: native_search_ran(&conversation.contexts),
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
            at,
        };
        let next = with_run_tool_lock(&conversation.settings, tools, &context).into_owned();
        conversation.settings = next;
        true
    });
}

/// `refreshConversationToolLock`: moves the lock's last-request time to now: the run's last request just went out.
fn refresh_conversation_tool_lock(app: &mut App, workspace_id: &str, conversation_id: &str) {
    app.update_conversation_if(workspace_id, conversation_id, |conversation| {
        let has_last = conversation.settings.tool_lock.as_ref().is_some_and(|lock| lock.last_request.is_some());
        if !has_last {
            return false;
        }
        // A native search that ran during the run pins native now.
        let lock = refreshed_tool_lock(&conversation.settings, &now_iso(), native_search_ran(&conversation.contexts));
        conversation.settings.tool_lock = Some(lock);
        true
    });
}

/// `clearModelRunError`.
pub(super) fn clear_model_run_error(app: &mut App, conversation_id: &str, scope: ClearErrorScope) {
    runs(app).model_run_errors.remove(conversation_id);
    if scope == ClearErrorScope::Notice {
        return;
    }
    if runs(app).turns.contains_key(conversation_id) {
        app.update_turns_of(conversation_id, true, |turns| {
            clear_turn_failures(turns);
        });
    }
}

/// `failConversationTurn`: records on the turn itself why its run stopped, so the timeline explains the gap and stays
/// readable after a reload — unlike the composer notice, which only lives in memory.
fn fail_conversation_turn(app: &mut App, conversation_id: &str, request_id: &str, message: &str, provider_name: &str, model_name: &str) {
    let error = ConversationTurnError { message: message.to_owned(), provider_name: provider_name.to_owned(), model_name: model_name.to_owned(), at: now_iso() };
    app.update_turns_of(conversation_id, true, |turns| {
        annotate_turn_failure(turns, request_id, error);
    });
}

fn finish_conversation_turns(app: &mut App, conversation_id: &str, request_id: &str, contexts: &[ContextItem], status: ConversationTurnStatus, meta: &TurnCompletionMeta) {
    let ended_at = now_iso();
    app.update_turns_of(conversation_id, true, |turns| {
        conversation_turns::finish_conversation_turns(turns, request_id, contexts, status, meta, &ended_at);
    });
}

/// `splitConversationTurn`.
fn split_conversation_turn(app: &mut App, workspace_id: &str, conversation_id: &str, run: &ModelRunState, next_anchor: &ContextItem) {
    let persisted = find_conversation(&app.doc, workspace_id, conversation_id).map_or(&[][..], |conversation| conversation.contexts.as_slice());
    let live: Vec<ContextItem> = contexts_from_model_run(run, true).into_iter().map(|context| context.item).collect();
    let projected = merge_unique_contexts([run.request.contexts.as_slice(), persisted, live.as_slice()]);
    let ended_at = now_iso();
    let cumulative = cumulative_model_run_usage(run);
    let anchor_id = next_anchor.id().to_owned();
    let (request_id, model_name, revision) = (run.request_id.clone(), run.model_name.clone(), run.usage_revision);
    app.update_turns_of(conversation_id, true, |turns| {
        conversation_turns::split_conversation_turn(turns, &projected, &request_id, &model_name, revision, &cumulative, &anchor_id, &ended_at);
    });
}

/// `persistInterruptedRun`.
fn persist_interrupted_run(app: &mut App, workspace_id: &str, conversation_id: &str, run: &ModelRunState) {
    // Callers capture their snapshot before awaiting the backend's cancel
    // acknowledgement, and stream state publishes on a fixed 100 ms cadence, so
    // that snapshot can be a commit behind — or the last events may still be
    // buffered. Flush, then re-read: losing the final tenth of a second of a
    // turn is losing exactly the content the user was looking at when they
    // chose to stop it.
    flush_pending_events(app, conversation_id);
    let latest = runs(app).controller.run(conversation_id).filter(|current| current.request_id == run.request_id).cloned().unwrap_or_else(|| run.clone());
    let interrupted = contexts_from_interrupted_run(&latest);
    let persisted: Vec<ContextItem> = find_conversation(&app.doc, workspace_id, conversation_id).map(|conversation| conversation.contexts.clone()).unwrap_or_default();
    let mut known: HashSet<String> = persisted.iter().map(|context| context.id().to_owned()).collect();
    let generated: Vec<ContextItem> = interrupted.iter().filter(|context| known.insert(context.id().to_owned())).cloned().collect();
    let merged = merge_unique_contexts([latest.request.contexts.as_slice(), persisted.as_slice(), generated.as_slice()]);
    finish_conversation_turns(app, conversation_id, &latest.request_id, &merged, ConversationTurnStatus::Interrupted, &TurnCompletionMeta::default());
    if interrupted.is_empty() {
        return;
    }
    let request_id = latest.request_id.clone();
    app.update_conversation_local(workspace_id, conversation_id, |conversation| apply_streamed_run_contexts(conversation, &interrupted, &request_id));
    seams::refresh_conversation(app, workspace_id, conversation_id);
}

// ---------------------------------------------------------------- tool prompts

/// `openToolPrompt`.
pub(super) fn open_tool_prompt(app: &mut App, conversation_id: &str, prompt: PendingToolPrompt) {
    let queue = runs(app).tool_prompts.entry(conversation_id.to_owned()).or_default();
    if !queue.iter().any(|pending| pending.prompt_id == prompt.prompt_id) {
        queue.push(prompt);
    }
}

/// `closeToolPrompt`.
pub(super) fn close_tool_prompt(app: &mut App, conversation_id: &str, prompt_id: &str) {
    if let Some(waiter) = runs(app).manual_tool_prompts.remove(prompt_id) {
        // The card went away without this renderer answering it — a cancelled
        // run, a backend timeout. The caller is still awaiting the nonce, so it
        // has to be told rather than left hanging.
        waiter.resolve(Err(t("工具确认已取消", "The approval was cancelled").to_owned()));
    }
    let prompts = &mut runs(app).tool_prompts;
    let Some(queue) = prompts.get_mut(conversation_id) else { return };
    queue.retain(|pending| pending.prompt_id != prompt_id);
    if queue.is_empty() {
        prompts.remove(conversation_id);
    }
}

/// `reconcileToolPrompts`: at run end, reconcile host-held pending approvals instead of clearing them; background cards
/// outlive turns. Preserve renderer-manual cards by reference.
fn reconcile_tool_prompts(app: &mut App, conversation_id: &str) {
    let cx = Cx::of(app);
    let conversation_id = conversation_id.to_owned();
    app.spawn(move |_| {
        let pending = cx.list_pending_tool_prompts();
        cx.post(move |app| {
            let state = runs(app);
            let manual: HashSet<String> = state.manual_tool_prompts.keys().cloned().collect();
            let Some(queue) = state.tool_prompts.get_mut(&conversation_id) else { return };
            match pending {
                Ok(pending) => {
                    let alive: HashSet<String> = pending.into_iter().filter(|prompt| prompt.conversation_id == conversation_id).map(|prompt| prompt.prompt.prompt_id).collect();
                    queue.retain(|prompt| alive.contains(&prompt.prompt_id) || manual.contains(&prompt.prompt_id));
                }
                Err(_) => queue.retain(|prompt| manual.contains(&prompt.prompt_id)),
            }
            if queue.is_empty() {
                state.tool_prompts.remove(&conversation_id);
            }
        });
    });
}

/// `pendingQuestionFor`: the `ask_user` card this conversation's run is blocked on, with what it would hand back if a
/// composer message were sent now.
fn pending_question_for(app: &mut App, conversation_id: &str) -> Option<(String, QuestionResponse)> {
    let state = runs(app);
    let card = state.tool_prompts.get(conversation_id)?.iter().find(|prompt| prompt.kind == PromptKind::Question)?;
    let response = state.question_drafts.get(&card.prompt_id).cloned().unwrap_or_else(QuestionResponse::close);
    Some((card.prompt_id.clone(), response))
}

/// `answerQuestionPrompt`, blocking. The card comes down only once the host took the answer; the host's own
/// `tool_approval_resolved` would close it anyway.
pub(super) fn answer_question_blocking(cx: &Cx, conversation_id: &str, prompt_id: &str, response: &QuestionResponse) -> bool {
    if cx.resolve_tool_prompt(prompt_id, ToolPromptDecision::Deny, None, Some(response)).is_err() {
        return false;
    }
    let (conversation_id, prompt_id) = (conversation_id.to_owned(), prompt_id.to_owned());
    cx.update(move |app| {
        runs(app).question_drafts.remove(&prompt_id);
        close_tool_prompt(app, &conversation_id, &prompt_id);
    });
    true
}

// ---------------------------------------------------------------- following a run

/// `registerRunState`: registers a run in the controller using the shared shape for started and adopted runs.
fn register_run_state(app: &mut App, workspace_id: &str, conversation_id: &str, request_id: &str, request: Arc<ModelRunRequest>, started_at: String) {
    // From here the run's own stream carries the conversation's tasks.
    app.forget_task_overlay(conversation_id);
    runs(app).controller.insert(conversation_id, ModelRunState::new(workspace_id, request_id, request, started_at));
}

/// `modelRunController.flushPendingEvents`: immediately folds this conversation's uncommitted buffered stream events
/// into run state. Events commit at 10 FPS, so an interrupted run must flush before persistence: the final visible
/// prose is the most likely data still buffered.
pub(super) fn flush_pending_events(app: &mut App, conversation_id: &str) {
    let Some(buffer) = runs(app).streams.get_mut(conversation_id) else { return };
    let timer = buffer.timer.take();
    let (workspace_id, request_id) = (buffer.workspace_id.clone(), buffer.request_id.clone());
    let batch = buffer.coalescer.take();
    if let Some(timer) = timer {
        app.clear_timer(timer);
    }
    if batch.is_empty() {
        return;
    }
    commit_batch(app, &workspace_id, conversation_id, &request_id, batch);
}

/// One commit: folds events individually in order, then performs effects in order.
fn commit_batch(app: &mut App, workspace_id: &str, conversation_id: &str, request_id: &str, batch: Vec<super::model_stream::QueuedModelStreamEvent>) {
    let mut effects: Vec<ModelStreamEffect> = Vec::new();
    runs(app).controller.update_run(conversation_id, |running| {
        if running.request_id != request_id {
            return false;
        }
        for entry in &batch {
            effects.extend(reduce_model_stream_event(running, &entry.event, &entry.at));
        }
        // Trim once per batch: the budget only needs to hold before a commit, and
        // per-event trimming would turn one traversal into dozens.
        trim_live_context_budget(running);
        true
    });
    for effect in effects {
        match effect {
            ModelStreamEffect::TurnSplit { item, run_before_input } => split_conversation_turn(app, workspace_id, conversation_id, &run_before_input, &item),
            ModelStreamEffect::SteeringDelivered { context_id } => {
                runs(app).composer.steering_message_ids.remove(&context_id);
            }
            ModelStreamEffect::TurnUsage { usage, revision } => {
                app.update_turns_of(conversation_id, false, |turns| {
                    conversation_turns::update_running_turn_usage(turns, request_id, &usage, revision);
                });
            }
            ModelStreamEffect::ToolCompleted { tool_name, success, .. } => {
                // A preview tool that reached completion means this conversation now owns a
                // preview page, so the pane learns about it from the same event that reports
                // the call instead of a separate poll. The tools that only read or kill a
                // dev-server process are excluded — they never create a page for the
                // conversation to own.
                if success && tool_name.as_deref().is_some_and(super::capabilities::is_preview_page_tool_name) {
                    seams::signals(app).register_preview_sessions.push(conversation_id.to_owned());
                }
            }
            ModelStreamEffect::ToolContextSettled { context } => seams::apply_settled_tool_context(app, conversation_id, context),
        }
    }
    // The host owns persistence: it writes each canonical context when produced
    // and throttles streaming prose into `streaming` rows. The renderer has no
    // write path and cannot overwrite end-of-turn output with a stale read model.
}

/// `handleEvent`: what a run's channel does with each message, on the UI thread.
fn handle_run_event(app: &mut App, conversation_id: &str, request_id: &str, value: Value) {
    if runs(app).controller.run_token(conversation_id) != Some(request_id) {
        return;
    }
    // The channel layer intercepts `run_concluded` for adopted runs; started runs
    // receive their result from the invoke promise, so ignore it with pings here.
    match value.get("type").and_then(Value::as_str) {
        Some("ping" | "run_concluded" | "debug_request_body") => return,
        _ => {}
    }
    match value.get("type").and_then(Value::as_str) {
        // Approval cards are raised by the tool executor, which does not know
        // the surrounding round, so they are handled before any run-state fold.
        Some("tool_approval_requested") => {
            if let Ok(prompt) = serde_json::from_value::<PendingToolPrompt>(value) {
                open_tool_prompt(app, conversation_id, prompt);
                crate::prelude::repaint();
            }
            return;
        }
        Some("tool_approval_resolved") => {
            if let Some(prompt_id) = value.get("promptId").and_then(Value::as_str) {
                close_tool_prompt(app, conversation_id, prompt_id);
                crate::prelude::repaint();
            }
            return;
        }
        _ => {}
    }
    let event: ModelStreamEvent = match serde_json::from_value(value) {
        Ok(event) => event,
        Err(error) => {
            eprintln!("model stream event: {error}");
            return;
        }
    };
    let needs_timer = match runs(app).streams.get_mut(conversation_id) {
        Some(buffer) if buffer.request_id == request_id => buffer.coalescer.push(event),
        _ => return,
    };
    if needs_timer {
        let id = conversation_id.to_owned();
        let timer = app.set_timer(Duration::from_millis(MODEL_STREAM_COMMIT_INTERVAL_MS), move |app| {
            if let Some(buffer) = runs(app).streams.get_mut(&id) {
                buffer.timer = None;
            }
            flush_pending_events(app, &id);
            crate::prelude::repaint();
        });
        if let Some(buffer) = runs(app).streams.get_mut(conversation_id) {
            buffer.timer = Some(timer);
        }
    }
}

/// What the lifecycle's `drive` gets to hand its events to.
#[derive(Clone)]
pub(super) struct RunKey {
    pub workspace_id: Arc<str>,
    pub conversation_id: Arc<str>,
    pub request_id: Arc<str>,
}

impl RunKey {
    fn new(workspace_id: &str, conversation_id: &str, request_id: &str) -> Self {
        Self { workspace_id: workspace_id.into(), conversation_id: conversation_id.into(), request_id: request_id.into() }
    }

    fn deliver(&self, app: &mut App, value: Value) {
        handle_run_event(app, &self.conversation_id, &self.request_id, value);
    }
}

/// `runTurnLifecycle`: `drive` is the event source. Started runs invoke `run_model`; adopted runs use the attach
/// channel and settlement slot. Both use the same lifecycle below.
fn run_turn_lifecycle(cx: &Cx, key: RunKey, request: Arc<ModelRunRequest>, drive: impl FnOnce(RunKey) -> RunOutcome) {
    // Each run gets its own commit buffer. It commits batches at 10 FPS while retaining every event.
    {
        let key = key.clone();
        cx.update(move |app| {
            runs(app).streams.insert(
                key.conversation_id.to_string(),
                StreamBuffer { workspace_id: key.workspace_id.to_string(), request_id: key.request_id.to_string(), coalescer: Default::default(), timer: None },
            );
        });
    }
    // Starting a turn used to release credential protection here, which silently answered the
    // takeover question on the user's behalf before it was ever asked. Control is now released
    // only by an explicit user action: the trusted handoff button, or the per-tab authorization
    // prompt the backend raises when a browser tool actually targets a signed-in user tab.
    let outcome = drive(key.clone());
    cx.update(move |app| settle_run(app, &key, &request, outcome));
}

/// Everything after a run's `await`, which upstream runs in one stretch.
fn settle_run(app: &mut App, key: &RunKey, request: &Arc<ModelRunRequest>, outcome: RunOutcome) {
    let (workspace_id, conversation_id, request_id) = (&*key.workspace_id, &*key.conversation_id, &*key.request_id);
    // The stream ended: commit the final buffered delta before reading run state.
    // On failure too, so an interrupted snapshot includes the final content before the stream broke.
    flush_pending_events(app, conversation_id);
    let still_ours = runs(app).controller.run_token(conversation_id) == Some(request_id);
    if still_ours {
        match outcome {
            Ok(response) => settle_response(app, workspace_id, conversation_id, request_id, request, response),
            Err(error) => {
                let message = if error.trim().is_empty() { t("未知错误", "Unknown error").to_owned() } else { error };
                // A first round that never connected produces no context at all, so the
                // turn is the only place the failure can be read — and a round with
                // neither output nor a notice is dropped when it settles, so the notice
                // has to land before the interrupted run is persisted.
                fail_conversation_turn(app, conversation_id, request_id, &message, &request.provider.name, &request.model.id);
                let running = runs(app).controller.run(conversation_id).filter(|running| running.request_id == request_id).cloned();
                if let Some(running) = running {
                    persist_interrupted_run(app, workspace_id, conversation_id, &running);
                }
                runs(app).model_run_errors.insert(
                    conversation_id.to_owned(),
                    ModelRunErrorState {
                        message,
                        retryable: true,
                        request_id: Some(request_id.to_owned()),
                        provider_name: request.provider.name.clone(),
                        model_name: request.model.id.clone(),
                        workspace_id: workspace_id.to_owned(),
                        request: request.clone(),
                    },
                );
            }
        }
    }
    // `finally`.
    let state = runs(app);
    state.streams.remove(conversation_id);
    let remaining_queued: HashSet<String> =
        find_conversation(&app.doc, workspace_id, conversation_id).map(|conversation| conversation.queued_messages.iter().map(|message| message.id.clone()).collect()).unwrap_or_default();
    let state = runs(app);
    state.composer.steering_message_ids.retain(|id| !remaining_queued.contains(id));
    if state.controller.run_token(conversation_id) == Some(request_id) {
        state.controller.delete_run_token(conversation_id);
        state.controller.remove(conversation_id);
    }
    // A run that ends without retracting its cards — a dropped channel, a
    // crashed worker — must not leave a card the user can still click. But a
    // background task's card is deliberately still alive host-side, so this
    // reconciles against the host's pending list instead of clearing.
    reconcile_tool_prompts(app, conversation_id);
    runs(app).controller.delete_performing_run(conversation_id);
    app.note_runs_changed();
}

/// `settlementToResponse`'s happy path: replace this run's document span with the canonical sequence.
fn settle_response(app: &mut App, workspace_id: &str, conversation_id: &str, request_id: &str, request: &Arc<ModelRunRequest>, response: RunModelResponse) {
    let completed_run = runs(app).controller.run(conversation_id).filter(|run| run.request_id == request_id).cloned();
    let stop_reason = response.stop_reason.as_deref().unwrap_or("");
    let user_cancelled = matches!(stop_reason, "cancelled" | "canceled" | "stopped");
    let hook_stopped = stop_reason.starts_with("hook_");
    let finalized = match &completed_run {
        Some(run) => backfill_requested_tool_input(&response.contexts, run),
        None => response.contexts.clone(),
    };
    let response_contexts = match (&completed_run, user_cancelled) {
        (Some(run), true) => merge_finalized_interrupted_run_contexts(run, &finalized),
        _ => finalized,
    };
    let persisted: Vec<ContextItem> = find_conversation(&app.doc, workspace_id, conversation_id).map(|conversation| conversation.contexts.clone()).unwrap_or_default();
    let completed_contexts = merge_unique_contexts([request.contexts.as_slice(), persisted.as_slice(), response_contexts.as_slice()]);
    runs(app).controller.update_run(conversation_id, |running| {
        if running.request_id != request_id {
            return false;
        }
        running.clear_streamed();
        true
    });
    let streamed_run_context_ids: HashSet<String> = completed_run
        .as_ref()
        .map(|run| contexts_from_interrupted_run(run).into_iter().filter(|context| !matches!(context, ContextItem::User { .. })).map(|context| context.id().to_owned()).collect())
        .unwrap_or_default();
    app.update_conversation_local(workspace_id, conversation_id, |conversation| {
        // The host already wrote canonical contexts. Align the read model immediately
        // without waiting for IPC, so do not persist this projection.
        // Replace same-id streamed contexts with final canonical contexts and discard
        // unmatched streamed residue. User contexts always survive even if absent
        // from settlement.
        let canonical: HashSet<&str> = response_contexts.iter().map(ContextItem::id).collect();
        let prefixes = run_stream_context_prefixes(request_id);
        let superseded = |id: &str| !canonical.contains(id) && (streamed_run_context_ids.contains(id) || prefixes.iter().any(|prefix| id.starts_with(prefix.as_str())));
        let delivered: HashSet<&str> = response_contexts.iter().filter(|context| matches!(context, ContextItem::User { .. })).map(ContextItem::id).collect();
        // Replace this run's document span with the complete canonical sequence at
        // the first matching context, preserving authoritative ordering.
        let belongs_to_run = |id: &str| canonical.contains(id) || superseded(id);
        let first_run_index = conversation.contexts.iter().position(|context| belongs_to_run(context.id()));
        let insert_at = first_run_index.map(|first| conversation.contexts[..first].iter().filter(|context| !belongs_to_run(context.id())).count());
        let mut kept: Vec<ContextItem> = std::mem::take(&mut conversation.contexts).into_iter().filter(|context| !belongs_to_run(context.id())).collect();
        let insert_at = insert_at.unwrap_or(kept.len());
        let mut seen: HashSet<&str> = HashSet::new();
        let settled_run: Vec<ContextItem> = response_contexts.iter().filter(|context| seen.insert(context.id())).cloned().collect();
        let tail = kept.split_off(insert_at);
        kept.extend(settled_run);
        kept.extend(tail);
        conversation.contexts = kept;
        conversation.queued_messages.retain(|message| !delivered.contains(message.id.as_str()));
        conversation.updated_at = now_iso();
        true
    });
    // The host conversation store is authoritative: restore omitted host contexts
    // and remove local residue from the read model.
    seams::refresh_conversation(app, workspace_id, conversation_id);
    let mut failure: Option<String> = None;
    if stop_reason == "error" {
        let attempts = response.error.as_ref().map_or(1, |error| error.attempts);
        let detail = response.error.as_ref().map_or_else(|| t("模型请求失败；可以检查上下文后重试。", "The model request failed. Check the context and try again.").to_owned(), |error| error.message.clone());
        let message = if attempts > 1 {
            tf("{detail}（已自动重试 {count} 次）", "{detail} (automatically retried {count} times)", &[("detail", &detail), ("count", &(attempts - 1))])
        } else {
            detail
        };
        // Why it stopped is recorded before that it stopped: settlement discards
        // a round that produced nothing, and for one that broke before its first
        // message this notice is the entire record of it.
        fail_conversation_turn(app, conversation_id, request_id, &message, &request.provider.name, &request.model.id);
        failure = Some(message);
    }
    let status = if stop_reason == "error" || hook_stopped || user_cancelled { ConversationTurnStatus::Interrupted } else { ConversationTurnStatus::Completed };
    let meta = TurnCompletionMeta { model_id: Some(response.model.clone()), usage: Some(response.usage.clone()), duration_ms: Some(response.duration_ms) };
    finish_conversation_turns(app, conversation_id, request_id, &completed_contexts, status, &meta);
    let fallback = estimate_contexts_tokens(request.contexts.iter().chain(response_contexts.iter()));
    runs(app).context_usage.insert(
        conversation_id.to_owned(),
        ContextUsage {
            tokens: response.context_tokens.unwrap_or(fallback),
            estimated: response.context_tokens.is_none(),
            unprojectable: false,
            provider_id: Some(request.provider.id.clone()),
            family: Some(request.provider.family),
        },
    );
    if let Some(message) = failure {
        runs(app).model_run_errors.insert(
            conversation_id.to_owned(),
            ModelRunErrorState {
                message,
                retryable: true,
                request_id: Some(request_id.to_owned()),
                provider_name: request.provider.name.clone(),
                model_name: request.model.id.clone(),
                workspace_id: workspace_id.to_owned(),
                request: request.clone(),
            },
        );
    }
}

/// `performModelRun` up to its first `await`, which upstream runs straight after the preparing flag comes down: nothing
/// may see the conversation with neither.
pub(super) fn begin_model_run(app: &mut App, workspace_id: &str, conversation_id: &str, request: Arc<ModelRunRequest>, turn_anchor: Option<(String, String)>) -> Option<RunKey> {
    if app.context_mutation_is_blocked(conversation_id) {
        return None;
    }
    // What this run shows the model, and which model it is, becomes the lock.
    lock_conversation_tools(app, workspace_id, conversation_id, &request.enabled_tools, &request.provider.id, &request.model.id);
    if seams::active_ids(app).1.as_deref() == Some(conversation_id) {
        app.close_editors();
    }
    let request_id = create_id("run");
    let controller = &mut runs(app).controller;
    controller.add_performing_run(conversation_id);
    controller.set_run_token(conversation_id, &request_id);
    // A new attempt retires the previous failure from every surface it reached.
    clear_model_run_error(app, conversation_id, ClearErrorScope::Failure);
    let started_at = turn_anchor.as_ref().map_or_else(now_iso, |(_, started_at)| started_at.clone());
    register_run_state(app, workspace_id, conversation_id, &request_id, request.clone(), started_at);
    let model_id = &request.model.id;
    let contexts = &request.contexts;
    match turn_anchor {
        Some((anchor_id, started_at)) => app.update_turns_of(conversation_id, true, |turns| {
            conversation_turns::start_conversation_turn(turns, &request_id, &anchor_id, model_id, &started_at, contexts);
        }),
        // No new user message of its own: this run continues the round it follows,
        // or opens one anchored at the timeline tail when that round is finished.
        None => {
            let resumed_at = now_iso();
            app.update_turns_of(conversation_id, true, |turns| {
                conversation_turns::continue_conversation_turn(turns, &request_id, model_id, contexts, &resumed_at);
            });
        }
    }
    app.note_runs_changed();
    Some(RunKey::new(workspace_id, conversation_id, &request_id))
}

/// The rest of `performModelRun`: blocks its task thread for the whole run.
pub(super) fn follow_model_run(cx: &Cx, key: RunKey, request: Arc<ModelRunRequest>) {
    let driver = cx.clone();
    let lifecycle_request = request.clone();
    let (workspace_id, conversation_id) = (key.workspace_id.clone(), key.conversation_id.clone());
    run_turn_lifecycle(cx, key, request, move |key| {
        let request_id = key.request_id.clone();
        driver.run_model(&lifecycle_request, &request_id, move |app, value| key.deliver(app, value))
    });
    // The prompt cache is counted from the last request, which a long run sent
    // just now rather than when it started.
    cx.update(move |app| refresh_conversation_tool_lock(app, &workspace_id, &conversation_id));
}

/// The preparing flag comes down and the run starts, in one stretch.
fn start_prepared(cx: &Cx, workspace_id: &str, conversation_id: &str, request: ModelRunRequest, anchor: Option<(String, String)>, before: impl FnOnce(&mut App) + Send + 'static) {
    let request = Arc::new(request);
    let key = {
        let (workspace_id, conversation_id, request) = (workspace_id.to_owned(), conversation_id.to_owned(), request.clone());
        cx.update(move |app| {
            runs(app).controller.delete_preparing_run(&conversation_id);
            before(app);
            begin_model_run(app, &workspace_id, &conversation_id, request, anchor)
        })
    };
    if let Some(key) = key {
        follow_model_run(cx, key, request);
    }
}

// ---------------------------------------------------------------- adoption

/// What an attach channel holds until its lifecycle is ready for events.
#[derive(Default)]
struct AdoptChannel {
    deliver: Option<RunKey>,
    early: Vec<Value>,
    concluded: bool,
    conclude: Option<mpsc::Sender<()>>,
}

/// `adoptConversationRun`: adopts a surviving host run after reload. Attach replays buffered live events;
/// `run_concluded` and the settlement slot provide its result. Returns after the run is registered and advancing in
/// the background, not after it finishes.
fn adopt_conversation_run(cx: &Cx, workspace_id: &str, conversation_id: &str) {
    let proceed = {
        let conversation_id = conversation_id.to_owned();
        cx.update(move |app| {
            let state = runs(app);
            if state.adoptions_in_flight.contains(&conversation_id)
                || state.controller.has_run_token(&conversation_id)
                || state.controller.has_performing_run(&conversation_id)
                || state.controller.has_preparing_run(&conversation_id)
            {
                return false;
            }
            state.adoptions_in_flight.insert(conversation_id);
            true
        })
    };
    if !proceed {
        return;
    }
    let shared = Arc::new(Mutex::new(AdoptChannel::default()));
    let channel_state = shared.clone();
    let (attach, stream) = cx.attach_model_run(conversation_id, move |app, value| {
        let mut channel = channel_state.lock().unwrap_or_else(|p| p.into_inner());
        if value.get("type").and_then(Value::as_str) == Some("run_concluded") {
            channel.concluded = true;
            if let Some(conclude) = channel.conclude.take() {
                let _ = conclude.send(());
            }
            return;
        }
        match channel.deliver.clone() {
            Some(key) => {
                drop(channel);
                key.deliver(app, value);
            }
            None => channel.early.push(value),
        }
    });
    let finish_adoption = |cx: &Cx, conversation_id: &str| {
        let conversation_id = conversation_id.to_owned();
        cx.update(move |app| {
            runs(app).adoptions_in_flight.remove(&conversation_id);
        });
    };
    let attach = match attach {
        Ok(attach) => attach,
        Err(_) => return finish_adoption(cx, conversation_id),
    };
    let (request_id, request, settlement) = match attach {
        AttachRunResult::None => {
            stream.close();
            return finish_adoption(cx, conversation_id);
        }
        AttachRunResult::Running { request_id, request } => (request_id, request, None),
        AttachRunResult::Finished { request_id, request, settlement } => (request_id, request, Some(settlement)),
    };
    let request = Arc::new(*request);
    let key = RunKey::new(workspace_id, conversation_id, &request_id);
    let (tx, rx) = mpsc::channel::<()>();
    let registered = {
        let (key, request, shared) = (key.clone(), request.clone(), shared.clone());
        cx.update(move |app| {
            let conversation_id = &*key.conversation_id;
            // If a local run started while awaiting attach, abandon adoption. The host
            // permits one run per conversation; this is local race defense.
            if runs(app).controller.has_run_token(conversation_id) || runs(app).controller.has_performing_run(conversation_id) {
                return false;
            }
            let controller = &mut runs(app).controller;
            controller.add_performing_run(conversation_id);
            controller.set_run_token(conversation_id, &key.request_id);
            // Reattaching is the same attempt continuing, so the turn keeps its
            // failure record until the settlement either replaces or clears it.
            clear_model_run_error(app, conversation_id, ClearErrorScope::Notice);
            register_run_state(app, &key.workspace_id, conversation_id, &key.request_id, request.clone(), now_iso());
            // Restore a same-generation turn marked interrupted during loading because
            // the host is still running it or awaiting its settlement.
            let resumed_at = now_iso();
            let (request_id, contexts, model_id) = (key.request_id.clone(), request.contexts.clone(), request.model.id.clone());
            app.update_turns_of(conversation_id, true, |turns| {
                conversation_turns::resume_adopted_conversation_turn(turns, &request_id, &contexts, &model_id, &resumed_at);
            });
            // If the marker arrived before anything waits for it, settlement starts at once.
            let mut channel = shared.lock().unwrap_or_else(|p| p.into_inner());
            if channel.concluded {
                let _ = tx.send(());
            } else {
                channel.conclude = Some(tx);
            }
            app.note_runs_changed();
            true
        })
    };
    if !registered {
        stream.close();
        return finish_adoption(cx, conversation_id);
    }
    let missing_message = t("运行已结束，但结算结果不可用", "The run has ended, but its settlement is unavailable");
    let driver = cx.clone();
    let conversation = conversation_id.to_owned();
    let lifecycle_cx = cx.clone();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("mewrk-adopted-run".to_owned())
        .spawn(move || {
            run_turn_lifecycle(&lifecycle_cx, key, request, move |key| {
                // Now that the lifecycle's buffer exists, what arrived early goes through it, in order.
                let channel_key = key.clone();
                let drain = shared.clone();
                driver.update(move |app| {
                    let early = {
                        let mut channel = drain.lock().unwrap_or_else(|p| p.into_inner());
                        channel.deliver = Some(channel_key.clone());
                        std::mem::take(&mut channel.early)
                    };
                    for value in early {
                        channel_key.deliver(app, value);
                    }
                });
                let _ = ready_tx.send(());
                match settlement {
                    Some(settlement) => RunSettlementPayload::into_outcome(Some(*settlement), missing_message),
                    None => {
                        let _ = rx.recv();
                        driver.take_run_settlement(&conversation).and_then(|settlement| RunSettlementPayload::into_outcome(settlement, missing_message))
                    }
                }
            });
            stream.close();
        })
        .expect("thread");
    let _ = ready_rx.recv();
    finish_adoption(cx, conversation_id);
}

/// `adoptResumableRuns`: reattaches every host run that is live or awaiting settlement after startup/load. After it
/// returns they are visible in the controller, so guards requiring no run cannot race adoption. Propagates enumeration
/// failure so the adoption latch distinguishes completed adoption from retryable failure: otherwise wake and queue
/// dispatch could proceed without attaching any host runs.
pub(super) fn adopt_resumable_runs(cx: &Cx) -> Result<(), String> {
    let rows = cx.list_resumable_runs()?;
    let mut adoptions = Vec::new();
    for row in rows {
        let conversation_id = row.conversation_id;
        let workspace_id = {
            let conversation_id = conversation_id.clone();
            cx.update(move |app| workspace_of(&app.doc, &conversation_id).map(|workspace| workspace.id.clone()))
        };
        let Some(workspace_id) = workspace_id else {
            // The conversation no longer exists, so no UI can adopt this run. Stop it
            // and discard any settlement.
            let cleanup = cx.clone();
            std::thread::spawn(move || {
                let _ = cleanup.cancel_conversation_run(&conversation_id);
                let _ = cleanup.take_run_settlement(&conversation_id);
            });
            continue;
        };
        let adopting = cx.clone();
        adoptions.push(std::thread::spawn(move || adopt_conversation_run(&adopting, &workspace_id, &conversation_id)));
    }
    for adoption in adoptions {
        let _ = adoption.join();
    }
    Ok(())
}

// ---------------------------------------------------------------- the queue

/// `queueComposerMessage`.
fn queue_composer_message(app: &mut App, workspace_id: &str, conversation_id: &str, content: &str, images: Vec<ImageAttachment>, files: Vec<FileAttachment>, clear_composer: bool) -> Option<QueuedMessage> {
    let conversation = find_conversation(&app.doc, workspace_id, conversation_id)?;
    let trimmed = content.trim();
    if trimmed.is_empty() && images.is_empty() && files.is_empty() {
        return None;
    }
    if exceeds_chars(trimmed, 100_000) {
        return None;
    }
    if conversation.queued_messages.len() >= 100 {
        return None;
    }
    // Queued numbers must be unique against the transcript *and* the queue
    // ahead of this message; a second repair happens at delivery (steer goes
    // through the Rust mailbox renumber, promotion re-checks below).
    let taken = reserve_queued_message_ids(image_short_ids_in_use(&conversation.contexts), &conversation.queued_messages);
    let (queued_content, queued_images) = renumber_images_for_send(taken, trimmed, &images);
    let message = QueuedMessage { id: create_id("queued"), content: queued_content, images: queued_images, files, created_at: now_iso() };
    let mut next = conversation.clone();
    next.queued_messages.push(message.clone());
    next.updated_at = message.created_at.clone();
    runs(app).composer.mark_queued_message_unsaved(&message.id);
    // A live steer can make this queued message part of an in-flight provider request. Do not
    // acknowledge that ordering barrier until the exact message (including image sidecar refs) is
    // durable across a process crash.
    let save = seams::publish_conversation(app, workspace_id, next, true);
    runs(app).composer.track_queued_message_save(&message.id, save.clone());
    // The unsaved marker and the title-bar save state handle failures.
    let cx = Cx::of(app);
    let message_id = message.id.clone();
    app.spawn(move |_| {
        let result = save.wait();
        cx.post(move |app| runs(app).composer.queued_message_save_settled(&message_id, &save, &result));
    });
    if clear_composer {
        runs(app).composer.clear_after_send(conversation_id);
    }
    Some(message)
}

/// `steerQueuedMessage`, blocking: `true` once the message is in the run's steer mailbox.
pub(super) fn steer_queued_message_blocking(cx: &Cx, conversation_id: &str, message: &QueuedMessage) -> bool {
    enum Barrier {
        NoRun,
        Pending(Deferred<Result<(), String>>, String),
        NeedsFlush(String),
        Clear(String),
    }
    let barrier = {
        let (conversation_id, message_id) = (conversation_id.to_owned(), message.id.clone());
        cx.update(move |app| {
            let state = runs(app);
            let Some(running) = state.controller.run(&conversation_id) else { return Barrier::NoRun };
            if state.composer.steering_message_ids.contains(&message_id) {
                return Barrier::NoRun;
            }
            let request_id = running.request_id.clone();
            match state.composer.queued_message_save(&message_id) {
                Some(save) => Barrier::Pending(save, request_id),
                None if state.composer.queued_message_needs_save(&message_id) => Barrier::NeedsFlush(request_id),
                None => Barrier::Clear(request_id),
            }
        })
    };
    let running_at_click = match barrier {
        Barrier::NoRun => return false,
        Barrier::Pending(save, request_id) => {
            if save.wait().is_err() {
                let message_id = message.id.clone();
                cx.update(move |app| runs(app).composer.drop_queued_message_save_if_current(&message_id, &save));
                return false;
            }
            request_id
        }
        Barrier::NeedsFlush(request_id) => {
            // Do not steer before durable persistence. Leave the message queued for an explicit retry.
            if cx.task.flush_latest_document(true).is_err() {
                return false;
            }
            let message_id = message.id.clone();
            cx.update(move |app| runs(app).composer.clear_queued_message_needs_save(&message_id));
            request_id
        }
        Barrier::Clear(request_id) => request_id,
    };
    let current = {
        let (conversation_id, message_id) = (conversation_id.to_owned(), message.id.clone());
        cx.update(move |app| {
            let state = runs(app);
            let running = state.controller.run(&conversation_id)?;
            let (workspace_id, request_id, vision) = (running.workspace_id.clone(), running.request_id.clone(), supports_vision(&running.request.model));
            if request_id != running_at_click || state.composer.steering_message_ids.contains(&message_id) {
                return None;
            }
            let current = find_conversation(&app.doc, &workspace_id, &conversation_id)?.queued_messages.iter().find(|queued| queued.id == message_id)?.clone();
            if !current.images.is_empty() && !vision {
                return None;
            }
            runs(app).composer.steering_message_ids.insert(current.id.clone());
            Some((request_id, current))
        })
    };
    let Some((request_id, current)) = current else { return false };
    if cx.steer_model_run(&request_id, &current).is_ok() {
        return true;
    }
    // The turn already ended: retract the steering marker and keep the message queued.
    cx.update(move |app| {
        runs(app).composer.steering_message_ids.remove(&current.id);
    });
    false
}

/// `commitUserTurn`: confirms the host accepted the prose append; otherwise replays it once on top of authoritative
/// prose.
///
/// `update_conversation` silently preserves host prose when the render read model differs item-by-item from the store,
/// then writes authoritative prose back to the read model. Presence confirms acceptance only if no explicit write
/// refusal remains: reloading authority after refusal is best-effort and may fail.
fn commit_user_turn(cx: &Cx, workspace_id: &str, conversation_id: &str, user_context: &ContextItem, title_for: impl Fn(&Conversation) -> String + Send + 'static) -> Option<Conversation> {
    fn committed(app: &mut App, workspace_id: &str, conversation_id: &str, id: &str) -> Option<Conversation> {
        if app.conversation_write_failure(conversation_id).is_some() {
            return None;
        }
        let conversation = find_conversation(&app.doc, workspace_id, conversation_id)?;
        conversation.contexts.iter().any(|context| context.id() == id).then(|| conversation.clone())
    }
    let (workspace, conversation, item) = (workspace_id.to_owned(), conversation_id.to_owned(), user_context.clone());
    let replay = cx.update(move |app| -> Result<Conversation, Option<Deferred<Result<(), String>>>> {
        if let Some(accepted) = committed(app, &workspace, &conversation, item.id()) {
            return Ok(accepted);
        }
        if app.conversation_write_failure(&conversation).is_some() {
            return Err(None);
        }
        let Some(located) = find_conversation(&app.doc, &workspace, &conversation) else { return Err(None) };
        let mut retried = located.clone();
        retried.title = title_for(located);
        retried.updated_at = match &item {
            ContextItem::User { created_at, .. } => created_at.clone(),
            _ => now_iso(),
        };
        retried.contexts.push(item);
        Err(Some(seams::publish_conversation(app, &workspace, retried, true)))
    });
    match replay {
        Ok(accepted) => Some(accepted),
        Err(None) => None,
        Err(Some(save)) => {
            save.wait().ok()?;
            let (workspace, conversation, id) = (workspace_id.to_owned(), conversation_id.to_owned(), user_context.id().to_owned());
            cx.update(move |app| committed(app, &workspace, &conversation, &id))
        }
    }
}

/// A title for a conversation still carrying the default one (`titleFor` / the promotion's inline rule).
fn title_from_message(title: &str, content: &str, files: &[FileAttachment], images: &[ImageAttachment]) -> String {
    // Recognizes both localized default-title markers.
    if title != "新任务" && title != "New task" {
        return title.to_owned();
    }
    let fragment = slice_utf16(&text_without_image_placeholders(content), 32);
    if !fragment.is_empty() {
        return fragment;
    }
    files.first().map(|file| file.name.clone()).or_else(|| images.first().map(|image| image.name.clone())).unwrap_or_else(|| t("图片", "Image").to_owned())
}

/// `sendComposer`.
pub(super) fn send_composer(app: &mut App) {
    let (workspace_id, conversation_id) = seams::active_ids(app);
    // Snapshot the tool catalog at send time so an active-conversation switch during
    // a durable wait cannot mix another conversation's tools into this request.
    let enabled_tools_at_send = seams::active_enabled_tools(app);
    let tools_at_send = seams::active_conversation_tools(app);
    let (Some(workspace_id), Some(conversation_id)) = (workspace_id, conversation_id) else { return };
    let Some(active_conversation) = find_conversation(&app.doc, &workspace_id, &conversation_id) else { return };
    if app.is_body_unloaded(&conversation_id) {
        // The body is still on its way; send once it is here, and only if the user is still on
        // this conversation — the composer they typed into belongs to it.
        let cx = Cx::of(app);
        app.spawn(move |_| {
            if cx.task.ensure_conversation_loaded(&conversation_id).is_err() {
                return;
            }
            cx.post(move |app| {
                if seams::active_ids(app).1.as_deref() == Some(&conversation_id) && !app.is_body_unloaded(&conversation_id) {
                    send_composer(app);
                }
            });
        });
        return;
    }
    let active_contexts = active_conversation.contexts.clone();
    let workspace_path = seams::workspace_path(app, &workspace_id);
    let queued_count = active_conversation.queued_messages.len();
    let paused = queue_is_paused(active_conversation);
    let composer = &runs(app).composer;
    // A long paste waited in the box as a tag; it is sent as the text it stands for.
    let typed = expand_pasted_texts(composer.drafts.get(&conversation_id).map_or("", String::as_str), composer.pasted_texts.get(&conversation_id).map_or(&[][..], Vec::as_slice));
    let typed = typed.trim();
    let element_picks = composer.element_picks.get(&conversation_id).cloned().unwrap_or_default();
    let images = composer.image_drafts.get(&conversation_id).cloned().unwrap_or_default();
    let files = composer.file_drafts.get(&conversation_id).cloned().unwrap_or_default();
    let uploading = composer.image_loading_ids.contains(&conversation_id);
    // The send boundary is where a draft stops being something the user is
    // still arranging: the picked elements expand into the prompt text they
    // stand for, and every attached image gets the `[Image #N]` the model
    // needs to cite it. Neither was ever shown in the composer.
    let draft = with_image_placeholders(&with_selected_elements(typed, &element_picks), &images);
    let has_content = !draft.is_empty() || !images.is_empty() || !files.is_empty();
    let request_contains_images = !images.is_empty() || contexts_contain_projected_images(&active_contexts);
    if uploading {
        return;
    }
    let Some((provider, model)) = usable_choice(app) else {
        seams::signals(app).open_provider_settings = true;
        crate::prelude::repaint();
        return;
    };
    if request_contains_images && !supports_vision(&model) {
        return;
    }
    // Queuing means waiting for a busy model.
    let queue_is_blocking = runs(app).controller.run(&conversation_id).is_some() || queued_count > 0;
    if has_content && queue_is_blocking {
        let queued = queue_composer_message(app, &workspace_id, &conversation_id, &draft, images, files, true);
        // Sending another message is what resumes a queue a Stop paused: the
        // messages already waiting go first, one round each, and this one after.
        set_conversation_queue_paused(app, &conversation_id, false);
        // A run blocked on an `ask_user` card takes a composer message as the
        // user's way out of the card: the message is steered in first, then the
        // card is answered with whatever was filled in (a close when nothing
        // was). The host settles the call with that result and the message
        // follows it into the next round. Steering first is what keeps a close
        // from ending the turn: the host only ends it when no message is waiting.
        let question = pending_question_for(app, &conversation_id);
        if let (Some(queued), Some((prompt_id, response)), true) = (queued, question, runs(app).controller.run(&conversation_id).is_some()) {
            let cx = Cx::of(app);
            app.spawn(move |_| {
                if steer_queued_message_blocking(&cx, &conversation_id, &queued) {
                    answer_question_blocking(&cx, &conversation_id, &prompt_id, &response);
                }
            });
        }
        app.note_runs_changed();
        return;
    }
    if queued_count > 0 {
        // An empty send with messages waiting resumes a queue a Stop paused; the
        // queue sends its head, so nothing else goes out from here.
        if paused {
            set_conversation_queue_paused(app, &conversation_id, false);
            dispatch_next_queued_message(app, &workspace_id, &conversation_id);
        }
        return;
    }
    // Nothing is waiting, so there is no pause left to keep either.
    set_conversation_queue_paused(app, &conversation_id, false);
    if app.context_mutation_is_blocked(&conversation_id) {
        return;
    }

    // Draft numbers were allocated when the images were added; the transcript
    // may have grown since (tool screenshots take numbers too), so repair any
    // collision at the send boundary — the last point before numbers freeze.
    let (send_content, send_images) = renumber_images_for_send(image_short_ids_in_use(&active_contexts), &draft, &images);
    let pending_user_context = has_content.then(|| ContextItem::User {
        id: create_id("ctx"),
        content: send_content.clone(),
        images: send_images.clone(),
        files: files.clone(),
        created_at: now_iso(),
    });
    let mut request_contexts = active_contexts;
    runs(app).controller.add_preparing_run(&conversation_id);
    let mut launch_settings = find_conversation(&app.doc, &workspace_id, &conversation_id).map(|conversation| conversation.settings.clone());
    let mut scalar_snapshot = None;
    let mut first_save = None;
    if let Some(pending) = &pending_user_context {
        let Some(located) = find_conversation(&app.doc, &workspace_id, &conversation_id) else {
            runs(app).controller.delete_preparing_run(&conversation_id);
            return;
        };
        request_contexts = located.contexts.clone();
        request_contexts.push(pending.clone());
        let original = (located.title.clone(), located.updated_at.clone());
        let mut next = located.clone();
        next.title = title_from_message(&located.title, &send_content, &files, &send_images);
        next.contexts = request_contexts.clone();
        // The tool surface this turn goes out with, and the model it goes
        // to, become durable alongside the turn itself.
        let exposure = ToolLockRequestContext {
            web_fetch: grants_web_fetch(located.settings.web_search_enabled, &located.settings.web_search, Some(provider.family)),
            native_searched: native_search_ran(&located.contexts),
            provider_id: provider.id.clone(),
            model_id: model.id.clone(),
            at: now_iso(),
        };
        next.settings = with_run_tool_lock(&located.settings, &enabled_tools_at_send, &exposure).into_owned();
        if let ContextItem::User { created_at, .. } = pending {
            next.updated_at = created_at.clone();
        }
        launch_settings = Some(next.settings.clone());
        scalar_snapshot = Some((original, next.clone()));
        // The user turn and provider snapshot must be durable before Rust is
        // allowed to construct or send the network request. Published before the
        // first await, so a second Send or a run's refresh builds on it.
        first_save = Some(seams::publish_conversation(app, &workspace_id, next, true));
    }
    let cx = Cx::of(app);
    let reasoning_effort = launch_settings.as_ref().map(|settings| settings.reasoning_effort).unwrap_or_default();
    app.spawn(move |_| {
        let done_preparing = |cx: &Cx, conversation_id: &str| {
            let conversation_id = conversation_id.to_owned();
            cx.update(move |app| runs(app).controller.delete_preparing_run(&conversation_id));
        };
        let mut reasoning_effort = reasoning_effort;
        let mut request_contexts = request_contexts;
        if let Some(pending) = &pending_user_context {
            if first_save.as_ref().is_some_and(|save| save.wait().is_err()) {
                let (workspace, conversation, id, snapshot) = (workspace_id.clone(), conversation_id.clone(), pending.id().to_owned(), scalar_snapshot.clone());
                let rollback = cx.update(move |app| roll_back_user_turn(app, &workspace, &conversation, &id, snapshot));
                if let Some(rollback) = rollback {
                    // The durable command reports failure only before commit or
                    // when the disk barrier itself failed. Persist the inverse
                    // transition too, so a restart cannot reveal an unsent turn.
                    // If rollback cannot persist, the optimistic state remains rolled
                    // back and the store continues retrying.
                    let _ = rollback.wait();
                }
                return done_preparing(&cx, &conversation_id);
            }
            // The host is the sole prose writer. Its `expected_context_ids` gate
            // silently retains host prose when the read model is stale, so a durable
            // write may succeed without storing this user turn. Confirm it reached
            // the store before running; otherwise the model would receive history
            // missing the message.
            let (title_content, title_files, title_images) = (send_content.clone(), files.clone(), send_images.clone());
            let committed = commit_user_turn(&cx, &workspace_id, &conversation_id, pending, move |conversation| title_from_message(&conversation.title, &title_content, &title_files, &title_images));
            let Some(committed) = committed else {
                let (provider, model, workspace, conversation, path, tools, enabled) = (provider.clone(), model.clone(), workspace_id.clone(), conversation_id.clone(), workspace_path.clone(), tools_at_send.clone(), enabled_tools_at_send.clone());
                let contexts = request_contexts.clone();
                cx.update(move |app| {
                    let refusal = app.conversation_write_failure(&conversation);
                    let suffix = refusal.map(|reason| tf("（宿主拒收原因：{reason}）", " Host refused it: {reason}", &[("reason", &reason)])).unwrap_or_default();
                    let message = format!(
                        "{}{suffix}",
                        t(
                            "这条消息没能写进对话库，已取消发送——请重新发送，不要让模型收到一份缺了它的历史。",
                            "This message could not be written to the conversation store, so the request was not sent. Send it again rather than letting the model receive a history without it."
                        )
                    );
                    let request = ModelRunRequest::new(&provider, &model, reasoning_effort, &conversation, &path, enabled, contexts, tools);
                    if let Ok(request) = request {
                        runs(app).model_run_errors.insert(
                            conversation.clone(),
                            ModelRunErrorState { message, retryable: false, request_id: None, provider_name: provider.name.clone(), model_name: model.id.clone(), workspace_id: workspace, request: Arc::new(request) },
                        );
                    }
                    crate::prelude::repaint();
                });
                return done_preparing(&cx, &conversation_id);
            };
            reasoning_effort = committed.settings.reasoning_effort;
            request_contexts = committed.contexts;
        } else if cx.task.flush_latest_document(true).is_err() {
            // Persist the provider/model snapshot before invoking Rust, where network destinations are
            // checked against the trusted document to prevent credential redirection from the WebView.
            // Do not start a request until API configuration is durably persisted.
            return done_preparing(&cx, &conversation_id);
        }
        let anchor = pending_user_context.as_ref().and_then(|pending| match pending {
            ContextItem::User { id, created_at, .. } => Some((id.clone(), created_at.clone())),
            _ => None,
        });
        match ModelRunRequest::new(&provider, &model, reasoning_effort, &conversation_id, &workspace_path, enabled_tools_at_send, request_contexts, tools_at_send) {
            Ok(request) => {
                let cleared = conversation_id.clone();
                start_prepared(&cx, &workspace_id, &conversation_id, request, anchor, move |app| {
                    runs(app).composer.clear_after_send(&cleared);
                    seams::signals(app).scroll_timeline_to_bottom = true;
                    crate::prelude::repaint();
                });
            }
            Err(error) => {
                eprintln!("{error}");
                done_preparing(&cx, &conversation_id);
            }
        }
    });
}

/// The optimistic turn comes back out after a failed durable write. `None` when there was nothing to roll back.
fn roll_back_user_turn(app: &mut App, workspace_id: &str, conversation_id: &str, id: &str, snapshot: Option<((String, String), Conversation)>) -> Option<Deferred<Result<(), String>>> {
    let optimistic = find_conversation(&app.doc, workspace_id, conversation_id)?;
    if !optimistic.contexts.iter().any(|context| context.id() == id) {
        return None;
    }
    let mut rolled_back = optimistic.clone();
    if let Some((original, next)) = snapshot {
        if *optimistic == next {
            rolled_back.title = original.0;
            rolled_back.updated_at = original.1;
        }
    }
    rolled_back.contexts.retain(|context| context.id() != id);
    Some(seams::publish_conversation(app, workspace_id, rolled_back, true))
}

/// `setQueuePaused`: pauses or resumes a conversation's queue; the pause is saved with it.
///
/// Upstream only updates its document store here, whose saves carry no conversations, so the host hears of the pause
/// when something else next writes this conversation; meanwhile a settlement's refresh reads the host's copy, without
/// the pause, back over the read model. Writing it to the host right away is what "saved with it" requires.
pub(super) fn set_conversation_queue_paused(app: &mut App, conversation_id: &str, paused: bool) {
    let Some(workspace_id) = workspace_of(&app.doc, conversation_id).map(|workspace| workspace.id.clone()) else { return };
    app.update_conversation_if(&workspace_id, conversation_id, |conversation| {
        if conversation.queue_paused == paused {
            return false;
        }
        conversation.queue_paused = paused;
        true
    });
}

/// `deleteQueuedMessage`.
pub(super) fn delete_queued_message(app: &mut App, workspace_id: &str, conversation_id: &str, message_id: &str) {
    if runs(app).composer.steering_message_ids.contains(message_id) {
        return;
    }
    let Some(conversation) = find_conversation(&app.doc, workspace_id, conversation_id) else { return };
    if !conversation.queued_messages.iter().any(|message| message.id == message_id) {
        return;
    }
    let mut next = conversation.clone();
    next.queued_messages.retain(|message| message.id != message_id);
    next.updated_at = now_iso();
    runs(app).composer.forget_queued_messages(&[message_id.to_owned()]);
    seams::publish_conversation(app, workspace_id, next, false);
}

/// `dispatchNextQueuedMessage`.
pub(super) fn dispatch_next_queued_message(app: &mut App, workspace_id: &str, conversation_id: &str) {
    // Snapshot the tool catalog at dispatch time so an active-conversation switch
    // during flush or persistence cannot contaminate this request.
    let tools_at_dispatch = seams::active_conversation_tools(app);
    let controller = &runs(app).controller;
    if controller.has_run_token(conversation_id) || controller.has_performing_run(conversation_id) || controller.has_preparing_run(conversation_id) {
        return;
    }
    let Some(conversation) = find_conversation(&app.doc, workspace_id, conversation_id) else { return };
    let Some(message) = conversation.queued_messages.first() else { return };
    let message_id = message.id.clone();
    // The queue effect dispatches again once the body is loaded.
    if app.is_body_unloaded(conversation_id) {
        seams::ensure_conversation_body_detached(app, conversation_id);
        return;
    }
    if runs(app).composer.failed_queued_promotion_ids.contains(&message_id) {
        return;
    }
    let Some((provider, model)) = usable_choice(app) else { return };
    let conversation = find_conversation(&app.doc, workspace_id, conversation_id).expect("found above");
    let message = conversation.queued_messages.first().expect("found above");
    if !supports_vision(&model) && (!message.images.is_empty() || contexts_contain_projected_images(&conversation.contexts)) {
        return;
    }
    let workspace_path = seams::workspace_path(app, workspace_id);
    runs(app).controller.add_preparing_run(conversation_id);
    let cx = Cx::of(app);
    let (workspace_id, conversation_id) = (workspace_id.to_owned(), conversation_id.to_owned());
    app.spawn(move |_| {
        let done_preparing = |cx: &Cx, conversation_id: &str| {
            let conversation_id = conversation_id.to_owned();
            cx.update(move |app| runs(app).controller.delete_preparing_run(&conversation_id));
        };
        if cx.task.flush_latest_document(false).is_err() {
            let conversation = conversation_id.clone();
            cx.update(move |app| app.back_off_reconcile(&conversation));
            return done_preparing(&cx, &conversation_id);
        }
        // Promotion, in one stretch on the UI thread.
        struct Promotion {
            queued: Conversation,
            original: Conversation,
            message: QueuedMessage,
            started_at: String,
            save: Deferred<Result<(), String>>,
        }
        let promotion = {
            let (workspace_id, conversation_id, message_id) = (workspace_id.clone(), conversation_id.clone(), message_id.clone());
            cx.update(move |app| {
                let located = find_conversation(&app.doc, &workspace_id, &conversation_id)?;
                let current_message = located.queued_messages.first()?;
                if current_message.id != message_id {
                    return None;
                }
                let current_message = current_message.clone();
                // The transcript may have grown while this message sat in the queue;
                // repair number collisions now, against both the contexts and the
                // messages still queued behind this one.
                let taken = reserve_queued_message_ids(image_short_ids_in_use(&located.contexts), located.queued_messages.iter().filter(|candidate| candidate.id != current_message.id));
                let (content, images) = renumber_images_for_send(taken, &current_message.content, &current_message.images);
                let user_context = ContextItem::User { id: current_message.id.clone(), content, images, files: current_message.files.clone(), created_at: current_message.created_at.clone() };
                let original = located.clone();
                let mut queued = located.clone();
                queued.title = title_from_message(&located.title, &current_message.content, &current_message.files, &current_message.images);
                queued.contexts.push(user_context);
                queued.queued_messages.retain(|candidate| candidate.id != current_message.id);
                queued.updated_at = now_iso();
                // This promotion is transactional: if the durable write fails, the
                // queued message must remain a queued intent rather than becoming an
                // unsent user turn. Suppress the generic optimistic echo and perform
                // an ID-scoped rollback below so concurrent edits are preserved.
                let save = seams::publish_conversation(app, &workspace_id, queued.clone(), true);
                Some(Promotion { queued, original, message: current_message, started_at: now_iso(), save })
            })
        };
        let Some(promotion) = promotion else { return done_preparing(&cx, &conversation_id) };
        let saved = promotion.save.wait();
        let promoted_ok = saved.is_ok() && {
            // The host may silently preserve its prose when read-model and store
            // contexts differ. Treat an unpromoted queued message as a write failure
            // and use the same ID-scoped rollback, rather than send history without it.
            let (workspace_id, conversation_id, id) = (workspace_id.clone(), conversation_id.clone(), promotion.message.id.clone());
            cx.update(move |app| find_conversation(&app.doc, &workspace_id, &conversation_id).is_some_and(|conversation| conversation.contexts.iter().any(|context| context.id() == id)))
        };
        if !promoted_ok {
            let (workspace, conversation) = (workspace_id.clone(), conversation_id.clone());
            let Promotion { queued, original, message, .. } = promotion;
            let rolled: Option<Deferred<Result<(), String>>> = cx.update(move |app| {
                // A promptly rejected save can be rolled back before anything consumes
                // the skip marker. Do not retain the abandoned snapshot indefinitely.
                runs(app).composer.failed_queued_promotion_ids.insert(message.id.clone());
                let optimistic = find_conversation(&app.doc, &workspace, &conversation)?;
                let still_owns_scalars = *optimistic == queued;
                let mut rolled_back = optimistic.clone();
                if still_owns_scalars {
                    rolled_back.title = original.title;
                    rolled_back.updated_at = original.updated_at;
                }
                rolled_back.contexts.retain(|context| context.id() != message.id);
                rolled_back.queued_messages.retain(|candidate| candidate.id != message.id);
                rolled_back.queued_messages.insert(0, message);
                Some(seams::publish_conversation(app, &workspace, rolled_back, true))
            });
            if let Some(rolled) = rolled {
                // A save rejection can still mean the host installed the promotion
                // before reporting an older writer failure. Publish and durably
                // acknowledge the inverse transition before exposing retry. On failure,
                // the store permits one ordinary debounced retry of the safe queued state,
                // and the failed-message latch still prevents auto promotion.
                let _ = rolled.wait();
            }
            // Promotion failure already completed the ID-scoped rollback. Keep the message
            // queued and require an explicit retry.
            return done_preparing(&cx, &conversation_id);
        }
        let Promotion { queued, started_at, .. } = promotion;
        let enabled_tools = enabled_tools_within(&queued, &tools_at_dispatch);
        match ModelRunRequest::new(&provider, &model, queued.settings.reasoning_effort, &conversation_id, &workspace_path, enabled_tools, Vec::new(), tools_at_dispatch) {
            Ok(mut request) => {
                request.run.contexts = queued.contexts;
                start_prepared(&cx, &workspace_id, &conversation_id, request, Some((message_id, started_at)), |_| {});
            }
            Err(error) => {
                eprintln!("{error}");
                done_preparing(&cx, &conversation_id);
            }
        }
    });
}

/// `retryFailedQueuedPromotion`.
pub(super) fn retry_failed_queued_promotion(app: &mut App, workspace_id: &str, conversation_id: &str, message_id: &str) {
    let still_exists = find_conversation(&app.doc, workspace_id, conversation_id).is_some_and(|conversation| conversation.queued_messages.iter().any(|message| message.id == message_id));
    runs(app).composer.failed_queued_promotion_ids.remove(message_id);
    if still_exists {
        dispatch_next_queued_message(app, workspace_id, conversation_id);
    }
}

// ---------------------------------------------------------------- runs with no new user message

/// How a run without a new message went at its synchronous gates.
pub(super) enum RunGate {
    /// An existing run or queue will consume this (`true` upstream).
    Consumed,
    /// Configuration or mutual exclusion prevents it for now (`false` upstream).
    Blocked,
    /// Preparing is flagged and this request is ready to go, once the flush succeeds.
    Start(Box<PreparedRun>),
}

pub(super) struct PreparedRun {
    workspace_id: String,
    conversation_id: String,
    request: ModelRunRequest,
    anchor: Option<(String, String)>,
}

/// Flushes the provider snapshot, as on the send path, then runs. `false` when the flush failed.
pub(super) fn launch_prepared(cx: &Cx, prepared: PreparedRun) -> bool {
    if cx.task.flush_latest_document(true).is_err() {
        let conversation_id = prepared.conversation_id;
        cx.update(move |app| {
            runs(app).controller.delete_preparing_run(&conversation_id);
            app.back_off_reconcile(&conversation_id);
        });
        return false;
    }
    start_prepared(cx, &prepared.workspace_id, &prepared.conversation_id, prepared.request, prepared.anchor, |_| {});
    true
}

fn prepare(app: &mut App, workspace_id: &str, conversation_id: &str, provider: &ApiProvider, model: &ModelProfile, tools: Vec<ToolDescriptor>, build: impl FnOnce(&mut ModelRunRequest), anchor: Option<(String, String)>) -> RunGate {
    let conversation = find_conversation(&app.doc, workspace_id, conversation_id).expect("located by the caller");
    let enabled_tools = enabled_tools_within(conversation, &tools);
    let contexts = conversation.contexts.clone();
    let reasoning_effort = conversation.settings.reasoning_effort;
    let workspace_path = seams::workspace_path(app, workspace_id);
    let mut request = match ModelRunRequest::new(provider, model, reasoning_effort, conversation_id, &workspace_path, enabled_tools, contexts, tools) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("{error}");
            return RunGate::Blocked;
        }
    };
    build(&mut request);
    runs(app).controller.add_preparing_run(conversation_id);
    RunGate::Start(Box::new(PreparedRun { workspace_id: workspace_id.to_owned(), conversation_id: conversation_id.to_owned(), request, anchor }))
}

/// `wakeConversation`'s gates: a run with no new user message when a settled task can be delivered — whichever
/// conversation it is, on screen or not, like a queued message or a fork's first run. An existing run or queue consumes
/// it at its turn boundary; a wake never impersonates an answer or revives an awaiting turn.
pub(super) fn wake_gate(app: &mut App, workspace_id: &str, conversation_id: &str) -> RunGate {
    // Snapshot the catalog now so a conversation switch during the flush cannot
    // change what this run is offered. The conversation's own settings pick from
    // it, as for a queued message: it need not be the one on screen.
    let tools = seams::active_conversation_tools(app);
    let Some(conversation) = find_conversation(&app.doc, workspace_id, conversation_id) else { return RunGate::Blocked };
    // The wake effect runs again once the body is loaded.
    if app.is_body_unloaded(conversation_id) {
        seams::ensure_conversation_body_detached(app, conversation_id);
        return RunGate::Blocked;
    }
    // A queue the user paused with Stop sends nothing until they send again, so it
    // cannot carry the result: the wake runs on its own.
    let queue_will_run = !conversation.queued_messages.is_empty() && !queue_is_paused(conversation);
    let controller = &runs(app).controller;
    if controller.run(conversation_id).is_some() || controller.has_run_token(conversation_id) || controller.has_performing_run(conversation_id) || queue_will_run {
        return RunGate::Consumed;
    }
    // A concurrent wake or send that is merely preparing is not consumption: it may
    // fail during persistence. Preserve wake accounting until a run actually appears,
    // then consume it when the wake effect reruns.
    if controller.has_preparing_run(conversation_id) || app.context_mutation_is_blocked(conversation_id) {
        return RunGate::Blocked;
    }
    let Some((provider, model)) = usable_choice(app) else { return RunGate::Blocked };
    prepare(app, workspace_id, conversation_id, &provider, &model, tools, |_| {}, None)
}

fn no_run_in_the_way(app: &mut App, conversation_id: &str) -> bool {
    let controller = &runs(app).controller;
    !(controller.run(conversation_id).is_some() || controller.has_run_token(conversation_id) || controller.has_performing_run(conversation_id) || controller.has_preparing_run(conversation_id))
}

/// `startForkedConversationRun`'s gates: the first run of a conversation the host forked on the model's behalf. The
/// prompt is already persisted as the child's last user message — or, in a native compaction's continuation, the
/// compaction card the work carries on from — so this is a run over persisted contexts anchored at it. `Blocked` means
/// configuration or an existing run blocked it; the child then simply waits for the user.
pub(super) fn forked_run_gate(app: &mut App, workspace_id: &str, conversation_id: &str) -> RunGate {
    let Some(conversation) = find_conversation(&app.doc, workspace_id, conversation_id) else { return RunGate::Blocked };
    // The host appended the prompt as the last user context — or opened a native
    // compaction's continuation on its card, which the work carries on from; without
    // either there is nothing to answer and the run would open an empty round.
    let anchor = match conversation.contexts.last() {
        Some(last @ ContextItem::System { native_compaction: Some(_), .. }) => Some(last.id().to_owned()),
        _ => conversation.contexts.iter().rev().find(|context| matches!(context, ContextItem::User { .. })).map(|context| context.id().to_owned()),
    };
    if !no_run_in_the_way(app, conversation_id) || app.context_mutation_is_blocked(conversation_id) {
        return RunGate::Blocked;
    }
    let Some((provider, model)) = usable_choice(app) else { return RunGate::Blocked };
    let Some(anchor) = anchor else { return RunGate::Blocked };
    // The child is not the active conversation, so its tool set comes from its own
    // settings rather than the active-conversation seams. The catalog itself is
    // document-wide; the host re-derives the enabled list from the store anyway.
    let tools = seams::active_conversation_tools(app);
    let fork_prompt = anchor.clone();
    prepare(app, workspace_id, conversation_id, &provider, &model, tools, move |request| request.fork_prompt_context_id = Some(fork_prompt), Some((anchor, now_iso())))
}

/// `compactConversationNow`'s gates: compacts a conversation natively at once (`compactNow`): a run without a new user
/// message that the host ends at its first boundary, once the continuation exists.
pub(super) fn compact_now_gate(app: &mut App, workspace_id: &str, conversation_id: &str) -> RunGate {
    // Snapshot the catalog now, as a wake does, so a conversation switch during the
    // flush below cannot change what the compacted request offers.
    let tools = seams::active_conversation_tools(app);
    if find_conversation(&app.doc, workspace_id, conversation_id).is_none() || app.is_body_unloaded(conversation_id) {
        return RunGate::Blocked;
    }
    if !no_run_in_the_way(app, conversation_id) || app.context_mutation_is_blocked(conversation_id) {
        return RunGate::Blocked;
    }
    let Some((provider, model)) = usable_choice(app) else { return RunGate::Blocked };
    prepare(app, workspace_id, conversation_id, &provider, &model, tools, |request| request.compact_now = true, None)
}

/// `retryActiveModelRun`.
pub(super) fn retry_active_model_run(app: &mut App) {
    let (workspace_id, conversation_id) = seams::active_ids(app);
    let (Some(workspace_id), Some(conversation_id)) = (workspace_id, conversation_id) else { return };
    let Some(conversation) = find_conversation(&app.doc, &workspace_id, &conversation_id) else { return };
    let contexts = conversation.contexts.clone();
    let reasoning_effort = conversation.settings.reasoning_effort;
    let Some(failed) = runs(app).model_run_errors.get(&conversation_id).cloned() else { return };
    if app.context_mutation_is_blocked(&conversation_id) {
        return;
    }
    let Some((provider, model)) = usable_choice(app) else {
        seams::signals(app).open_provider_settings = true;
        crate::prelude::repaint();
        return;
    };
    if !supports_vision(&model) && contexts_contain_projected_images(&contexts) {
        return;
    }
    let workspace_path = seams::workspace_path(app, &workspace_id);
    let enabled_tools = seams::active_enabled_tools(app);
    let tools = seams::active_conversation_tools(app);
    let mut request = (*failed.request).clone();
    let built = ModelRunRequest::new(&provider, &model, reasoning_effort, &conversation_id, &workspace_path, enabled_tools, contexts, tools);
    let Ok(built) = built else { return };
    request.run = built.run;
    runs(app).controller.add_preparing_run(&conversation_id);
    let cx = Cx::of(app);
    app.spawn(move |_| {
        // Do not start a request until API configuration has persisted safely.
        if cx.task.flush_latest_document(false).is_ok() {
            start_prepared(&cx, &workspace_id, &conversation_id, request, None, |_| {});
        } else {
            cx.update(move |app| runs(app).controller.delete_preparing_run(&conversation_id));
        }
    });
}

/// How long to wait after a stop is confirmed. A timeout does not mean cancellation failed; the run card stays visible
/// and another idempotent stop request is allowed.
const STOP_SETTLE_TIMEOUT: Duration = Duration::from_millis(15_000);

/// `stopModelRun`.
pub(super) fn stop_model_run(app: &mut App, conversation_id: &str, on_cancellation_acknowledged: Option<Box<dyn FnOnce(&Cx) + Send>>, done: Option<Box<dyn FnOnce(&mut App, bool) + Send>>) {
    let finish_now = |app: &mut App, done: Option<Box<dyn FnOnce(&mut App, bool) + Send>>| {
        if let Some(done) = done {
            done(app, false);
        }
    };
    if runs(app).model_stopping_ids.contains(conversation_id) {
        return finish_now(app, done);
    }
    let Some(request_id) = runs(app).controller.run(conversation_id).map(|running| running.request_id.clone()) else { return finish_now(app, done) };
    if runs(app).controller.run_token(conversation_id) != Some(&request_id) {
        return finish_now(app, done);
    }
    runs(app).model_stopping_ids.insert(conversation_id.to_owned());
    // Stop pauses the queue too, before the run can end and hand it the next
    // message: what the user stopped should not be followed by more of the same.
    set_conversation_queue_paused(app, conversation_id, true);
    crate::prelude::repaint();
    let cx = Cx::of(app);
    let conversation_id = conversation_id.to_owned();
    app.spawn(move |_| {
        // Conversation id is the stable stop key across reloads; previews lack a conversation-level hub and use request id.
        let acknowledged = match cx.cancel_conversation_run(&conversation_id) {
            Ok(true) => true,
            Ok(false) => cx.cancel_model_run(&request_id).unwrap_or(false),
            Err(_) => false,
        };
        let mut settled = false;
        if acknowledged {
            if let Some(on_ack) = on_cancellation_acknowledged {
                on_ack(&cx);
            }
            // The cancellation command only publishes the stop flag. Keep the run and
            // its task cards visible until the run promise has joined every scoped
            // worker and the send pipeline has persisted the backend's terminal
            // records — but bounded: an uncooperative teardown must not hold the
            // stop button hostage forever.
            let exit = {
                let (conversation_id, request_id) = (conversation_id.clone(), request_id.clone());
                cx.update(move |app| runs(app).controller.wait_for_run_exit(&conversation_id, &request_id))
            };
            settled = exit.recv_timeout(STOP_SETTLE_TIMEOUT).is_ok();
        }
        cx.update(move |app| {
            if settled {
                runs(app).model_run_errors.remove(&conversation_id);
            }
            // A stop the host never took leaves the run going; its natural end must
            // still hand the queue on.
            if !acknowledged {
                set_conversation_queue_paused(app, &conversation_id, false);
            }
            runs(app).model_stopping_ids.remove(&conversation_id);
            if let Some(done) = done {
                done(app, settled);
            }
            app.note_runs_changed();
        });
    });
}

// ---------------------------------------------------------------- attachments

/// `uploadTargetCheck`: whether uploads queued for a conversation's composer may still land. A draft that materialized
/// mid-upload has handed its attachments on already, so an upload still addressed to it is as stale as one for a
/// deleted conversation.
fn upload_target_is_current(cx: &Cx, ticket: &UploadTicket) -> bool {
    let ticket = ticket.clone();
    cx.update(move |app| {
        if !runs(app).composer.upload_still_current(&ticket) {
            return false;
        }
        let conversation_id = &ticket.conversation_id;
        if seams::is_draft_conversation_id(conversation_id) { app.draft_of(conversation_id).is_some() } else { workspace_of(&app.doc, conversation_id).is_some() }
    })
}

/// A batch from a queue that was invalidated: it runs to report it is stale, then nothing follows it.
pub(super) fn start_stale_upload(app: &mut App, started: (UploadBatch, UploadTicket)) {
    start_upload(app, Some(started));
}

/// Starts a batch on its own thread, and whatever follows it.
fn start_upload(app: &mut App, started: Option<(UploadBatch, UploadTicket)>) {
    let Some((batch, ticket)) = started else { return };
    let backend_task = app.task();
    app.spawn(move |_| {
        let finished = ticket.clone();
        batch(backend_task.clone(), ticket);
        backend_task.post(move |app| {
            let next = runs(app).composer.finish_image_upload(&finished);
            start_upload(app, next);
            crate::prelude::repaint();
        });
    });
}

fn image_input_available(app: &App) -> bool {
    let (provider, model) = model_choice(&app.doc);
    provider.is_some_and(|provider| provider.enabled) && model.is_some_and(|model| !model.id.trim().is_empty() && supports_vision(model))
}

/// `addComposerImagesNow`: the image half of an upload batch, run inside the conversation's upload queue.
fn add_composer_images_now(cx: &Cx, ticket: &UploadTicket, files: Vec<ComposerFile>) -> Vec<ImageAttachment> {
    let conversation_id = ticket.conversation_id.clone();
    if !upload_target_is_current(cx, ticket) {
        return Vec::new();
    }
    if !cx.update(|app| image_input_available(app)) {
        return Vec::new();
    }
    let accepted_files: Vec<ComposerFile> = files.into_iter().filter(|file| file.size() > 0 && file.size() <= attachments::MAX_IMAGE_ATTACHMENT_BYTES).collect();
    if accepted_files.is_empty() {
        return Vec::new();
    }
    let uploaded: Vec<ImageAttachment> = accepted_files.iter().filter_map(|file| cx.prepare_image_attachment(&file.name, &file.bytes).ok()).collect();
    if !upload_target_is_current(cx, ticket) {
        return Vec::new();
    }
    if uploaded.is_empty() {
        return Vec::new();
    }
    // The numbers already taken are in the conversation's body, so number against it loaded.
    let unloaded = {
        let conversation_id = conversation_id.clone();
        cx.update(move |app| app.is_body_unloaded(&conversation_id))
    };
    if unloaded {
        // A failure numbers against what is known; the send boundary still dedupes typed placeholders.
        let _ = cx.task.ensure_conversation_loaded(&conversation_id);
        if !upload_target_is_current(cx, ticket) {
            return Vec::new();
        }
    }
    cx.update(move |app| {
        let existing = runs(app).composer.image_drafts.get(&conversation_id).cloned().unwrap_or_default();
        let mut known: HashSet<String> = existing.iter().map(|image| image.id.clone()).collect();
        let unique: Vec<ImageAttachment> = uploaded
            .into_iter()
            .filter(|image| {
                if known.contains(&image.id) {
                    return false;
                }
                let pixels = image.width as u64 * image.height as u64;
                if pixels == 0 || pixels > attachments::MAX_IMAGE_ATTACHMENT_PIXELS {
                    return false;
                }
                known.insert(image.id.clone());
                true
            })
            .collect();
        if unique.is_empty() {
            return Vec::new();
        }
        // Number the accepted images the way Claude Code does, but keep the
        // number off the draft: the composer shows a thumbnail, and the
        // `[Image #N]` the model reads is written at the send boundary.
        // Numbers already taken anywhere visible — transcript, queued
        // messages, this draft's images and any placeholder the user typed
        // themselves — are never reissued.
        let conversation = workspace_of(&app.doc, &conversation_id).and_then(|workspace| workspace.conversations.iter().find(|candidate| candidate.id == conversation_id));
        let mut taken = match conversation {
            Some(conversation) => reserve_queued_message_ids(image_short_ids_in_use(&conversation.contexts), &conversation.queued_messages),
            None => HashSet::new(),
        };
        taken.extend(existing.iter().filter_map(|image| image.short_id));
        let composer = &runs(app).composer;
        let draft = expand_pasted_texts(composer.drafts.get(&conversation_id).map_or("", String::as_str), composer.pasted_texts.get(&conversation_id).map_or(&[][..], Vec::as_slice));
        taken.extend(image_placeholder_ids(&draft));
        let numbered: Vec<ImageAttachment> = unique
            .into_iter()
            .map(|image| {
                let short_id = next_image_short_id(&taken);
                taken.insert(short_id);
                ImageAttachment { short_id: Some(short_id), ..image }
            })
            .collect();
        runs(app).composer.image_drafts.entry(conversation_id).or_default().extend(numbered.iter().cloned());
        numbered
    })
}

/// `addComposerImages`: images the host hands the composer on its own — a preview pane's capture, an element pick's
/// crop. They count against what the message's attachments may come to like any the user adds, and one that does not
/// fit says so. `done` hears which landed.
pub(super) fn add_composer_images(app: &mut App, conversation_id: &str, files: Vec<ComposerFile>, done: impl FnOnce(&mut App, Vec<ImageAttachment>) + Send + 'static) {
    if files.is_empty() {
        return done(app, Vec::new());
    }
    let cx = Cx::of(app);
    let batch: UploadBatch = Box::new(move |_, ticket| {
        let conversation_id = ticket.conversation_id.clone();
        let fitting = cx.update(move |app| {
            let composer = &mut runs(app).composer;
            let mut room = MessageAttachmentRoom::new(composer.image_drafts.get(&conversation_id).map_or(&[][..], Vec::as_slice), composer.file_drafts.get(&conversation_id).map_or(&[][..], Vec::as_slice));
            let mut fitting = Vec::new();
            let mut rejected = Vec::new();
            for file in files {
                if room.fits(file.size()) {
                    fitting.push(file);
                } else {
                    rejected.push(AttachmentRejection { name: (!file.name.is_empty()).then(|| file.name.clone()), reason: AttachmentRejectionReason::MessageTooLarge });
                }
            }
            if !rejected.is_empty() {
                composer.attachment_notices.insert(conversation_id, rejected);
            }
            fitting
        });
        let images = add_composer_images_now(&cx, &ticket, fitting);
        cx.post(move |app| done(app, images));
    });
    let started = runs(app).composer.enqueue_image_upload(conversation_id, batch);
    start_upload(app, started);
}

/// `addComposerAttachments`: attaches whatever the user picked, pasted or dropped: images join the image drafts, PDFs
/// and text files the file drafts, and anything left out is reported on the composer. `pre_rejected` carries what a
/// drop turned away before reading (a folder).
pub(super) fn add_composer_attachments(app: &mut App, conversation_id: &str, files: Vec<ComposerFile>, pre_rejected: Vec<AttachmentRejection>) {
    if files.is_empty() && pre_rejected.is_empty() {
        return;
    }
    let cx = Cx::of(app);
    let batch: UploadBatch = Box::new(move |_, ticket| {
        if !upload_target_is_current(&cx, &ticket) {
            return;
        }
        let conversation_id = ticket.conversation_id.clone();
        let (image_input, room) = {
            let conversation_id = conversation_id.clone();
            cx.update(move |app| {
                let image_input = image_input_available(app);
                let composer = &runs(app).composer;
                (image_input, MessageAttachmentRoom::new(composer.image_drafts.get(&conversation_id).map_or(&[][..], Vec::as_slice), composer.file_drafts.get(&conversation_id).map_or(&[][..], Vec::as_slice)))
            })
        };
        let mut rejected = pre_rejected;
        let mut image_files = Vec::new();
        let mut documents = Vec::new();
        for intake in attachments::sort_attachments(files, image_input, room) {
            match intake {
                Intake::Image(file) => image_files.push(file),
                Intake::Document { file, kind } => documents.push((file, kind)),
                Intake::Rejected(rejection) => rejected.push(rejection),
            }
        }
        if !image_files.is_empty() {
            let wanted = image_files.len();
            let images = add_composer_images_now(&cx, &ticket, image_files);
            // The image path drops what it cannot take without saying which; the
            // count is what is left to report.
            rejected.extend((images.len()..wanted).map(|_| AttachmentRejection { name: None, reason: AttachmentRejectionReason::ImageRejected }));
        }
        let mut uploaded = Vec::new();
        for (file, kind) in documents {
            match attachments::read_document(file, kind) {
                Err(rejection) => rejected.push(rejection),
                Ok(document) => match cx.prepare_file_attachment(&document) {
                    Ok(file) => uploaded.push(file),
                    Err(_) => rejected.push(AttachmentRejection { name: Some(document.name), reason: AttachmentRejectionReason::Failed }),
                },
            }
        }
        if !upload_target_is_current(&cx, &ticket) {
            return;
        }
        cx.update(move |app| {
            let composer = &mut runs(app).composer;
            // Files already on the message, and files that turn out to be the same bytes
            // as one of them, are not attached twice — a duplicate is not an error worth
            // a notice, so it is dropped without one.
            if !uploaded.is_empty() {
                let current = composer.file_drafts.get(&conversation_id).cloned().unwrap_or_default();
                composer.file_drafts.insert(conversation_id.clone(), attachments::merge_file_attachments(&current, &uploaded));
            }
            // Each attempt reports for itself: a later one that took everything clears
            // what an earlier one left out.
            if rejected.is_empty() {
                composer.attachment_notices.remove(&conversation_id);
            } else {
                composer.attachment_notices.insert(conversation_id, rejected);
            }
            crate::prelude::repaint();
        });
    });
    let started = runs(app).composer.enqueue_image_upload(conversation_id, batch);
    start_upload(app, started);
}

/// `removeComposerFile`.
pub(super) fn remove_composer_file(app: &mut App, conversation_id: &str, file_id: &str) {
    if let Some(files) = runs(app).composer.file_drafts.get_mut(conversation_id) {
        files.retain(|file| file.id != file_id);
    }
}

/// `removeComposerImage`: drops a draft image, and any `[Image #N]` the user typed for it, together.
pub(super) fn remove_composer_image(app: &mut App, conversation_id: &str, image_id: &str) {
    let composer = &mut runs(app).composer;
    let Some(images) = composer.image_drafts.get_mut(conversation_id) else { return };
    let Some(at) = images.iter().position(|image| image.id == image_id) else { return };
    let removed = images.remove(at);
    // The composer no longer writes the placeholder itself, but a user who
    // typed one for this image meant it to point at the thumbnail they just
    // removed. The reverse is deliberately not true — deleting placeholder
    // text in the draft never drops the image.
    if let Some(short_id) = removed.short_id {
        let draft = composer.drafts.entry(conversation_id.to_owned()).or_default();
        *draft = strip_image_placeholder(draft, short_id);
    }
}

