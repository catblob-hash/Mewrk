//! The public face of the run code: methods on [`App`] that views call. Each maps plainly onto its upstream name.

use std::sync::Arc;

use super::attachments::{AttachmentRejection, ComposerFile};
use super::bridge::Cx;
use super::composer::{selected_elements_from_text, ComposerState, ElementPick, PastedText};
use super::events::{close_fork_request, list_pending_fork_starts};
use super::image_short_ids::text_without_appended_image_placeholders;
use super::model_run_controller::ModelRunSummary;
use super::pipeline::{self, ClearErrorScope, RunGate};
use super::seams::{self, RunSignals};
use super::state::{RunBackend, RunsState};
use super::types::*;
use crate::i18n::{t, tf};
use crate::model::{ConversationPlan, ContextItem, ImageAttachment, SubagentRunRecord};
use crate::App;

fn runs(app: &mut App) -> &mut RunsState {
    app.init_runs();
    app.local::<RunsState>()
}

fn background(app: &mut App, f: impl FnOnce(Cx) + Send + 'static) {
    app.init_runs();
    let cx = Cx::of(app);
    app.spawn(move |_| f(cx));
}

fn launch_gate(app: &mut App, gate: RunGate, done: Option<Box<dyn FnOnce(&mut App, bool) + Send>>) {
    let result = match gate {
        RunGate::Consumed => true,
        RunGate::Blocked => false,
        RunGate::Start(prepared) => {
            background(app, move |cx| {
                let started = pipeline::launch_prepared(&cx, *prepared);
                if let Some(done) = done {
                    cx.post(move |app| done(app, started));
                }
            });
            return;
        }
    };
    if let Some(done) = done {
        done(app, result);
    }
}

/// Sending.
impl App {
    /// `sendActiveComposer`: sends what the active conversation's composer holds, queuing it if a run is busy.
    pub fn send_message(&mut self) {
        self.init_runs();
        let Some(draft) = self.active_conversation_id().filter(|id| seams::is_draft_conversation_id(id)) else {
            return pipeline::send_composer(self);
        };
        // Materialize a draft before sending, which requires a real conversation. Not while images are uploading,
        // because a send rejection must not leave an empty conversation.
        if self.local::<RunsState>().composer.image_loading_ids.contains(&draft) {
            return;
        }
        // A requested worktree is created before the first message so every tool call uses its isolated checkout;
        // never silently fall back to the workspace root.
        self.redeem_draft(false, |app, redeemed| match redeemed {
            Ok(Some(_)) => pipeline::send_composer(app),
            Ok(None) => {}
            Err(reason) => {
                let reason = crate::doc::failure_message(&reason, t("原因不明", "unknown reason"));
                seams::signals(app).branch_chip_error = Some(tf(
                    "无法建立隔离工作树，消息还没有发出：{reason}。取消勾选「工作树」可以直接在工作区根上开始。",
                    "The isolated worktree could not be created, so nothing was sent: {reason}. Clear the worktree checkbox to start on the workspace root instead.",
                    &[("reason", &reason)],
                ));
                crate::prelude::repaint();
            }
        });
    }

    /// `deleteQueuedMessage`.
    pub fn delete_queued_message(&mut self, workspace_id: &str, conversation_id: &str, message_id: &str) {
        self.init_runs();
        pipeline::delete_queued_message(self, workspace_id, conversation_id, message_id);
        crate::prelude::repaint();
    }

    /// `steerQueuedMessage`: puts a queued message into the running turn. `done` hears whether it reached the mailbox.
    pub fn steer_queued_message(&mut self, conversation_id: &str, message_id: &str, done: impl FnOnce(&mut App, bool) + Send + 'static) {
        self.init_runs();
        let message = seams::workspace_of(&self.doc, conversation_id)
            .and_then(|workspace| workspace.conversations.iter().find(|conversation| conversation.id == conversation_id))
            .and_then(|conversation| conversation.queued_messages.iter().find(|message| message.id == message_id))
            .cloned();
        let Some(message) = message else { return done(self, false) };
        let conversation_id = conversation_id.to_owned();
        background(self, move |cx| {
            let steered = pipeline::steer_queued_message_blocking(&cx, &conversation_id, &message);
            cx.post(move |app| {
                done(app, steered);
                crate::prelude::repaint();
            });
        });
    }

    /// `dispatchNextQueuedMessage`.
    pub fn dispatch_next_queued_message(&mut self, workspace_id: &str, conversation_id: &str) {
        self.init_runs();
        pipeline::dispatch_next_queued_message(self, workspace_id, conversation_id);
    }

    /// `retryFailedQueuedPromotion`.
    pub fn retry_failed_queued_promotion(&mut self, workspace_id: &str, conversation_id: &str, message_id: &str) {
        self.init_runs();
        self.local::<RunsState>().reconcile_retry_after.remove(conversation_id);
        pipeline::retry_failed_queued_promotion(self, workspace_id, conversation_id, message_id);
    }

    /// Whether a conversation's queue is held by a Stop.
    pub fn queue_is_paused(&mut self, conversation_id: &str) -> bool {
        seams::workspace_of(&self.doc, conversation_id).and_then(|workspace| workspace.conversations.iter().find(|conversation| conversation.id == conversation_id)).is_some_and(super::conversation_queue::queue_is_paused)
    }
}

/// The composer.
impl App {
    /// `composerController.current()`.
    pub fn composer(&mut self) -> &mut ComposerState {
        &mut runs(self).composer
    }

    /// The conversation's draft text.
    pub fn composer_draft(&mut self, conversation_id: &str) -> String {
        runs(self).composer.drafts.get(conversation_id).cloned().unwrap_or_default()
    }

    /// `updateDrafts`.
    pub fn set_composer_draft(&mut self, conversation_id: &str, text: impl Into<String>) {
        runs(self).composer.drafts.insert(conversation_id.to_owned(), text.into());
        crate::prelude::repaint();
    }

    /// `updatePastedTexts`.
    pub fn set_composer_pasted_texts(&mut self, conversation_id: &str, pastes: Vec<PastedText>) {
        runs(self).composer.pasted_texts.insert(conversation_id.to_owned(), pastes);
    }

    /// `updateElementPicks`.
    pub fn set_composer_element_picks(&mut self, conversation_id: &str, picks: Vec<ElementPick>) {
        runs(self).composer.element_picks.insert(conversation_id.to_owned(), picks);
        crate::prelude::repaint();
    }

    /// `addComposerImages`: images the host hands the composer on its own. `done` hears which landed.
    pub fn add_composer_images(&mut self, conversation_id: &str, files: Vec<ComposerFile>, done: impl FnOnce(&mut App, Vec<ImageAttachment>) + Send + 'static) {
        self.init_runs();
        pipeline::add_composer_images(self, conversation_id, files, done);
        crate::prelude::repaint();
    }

    /// `addComposerAttachments`: whatever the user picked, pasted or dropped.
    pub fn add_composer_attachments(&mut self, conversation_id: &str, files: Vec<ComposerFile>, pre_rejected: Vec<AttachmentRejection>) {
        self.init_runs();
        pipeline::add_composer_attachments(self, conversation_id, files, pre_rejected);
        crate::prelude::repaint();
    }

    /// `removeComposerImage`.
    pub fn remove_composer_image(&mut self, conversation_id: &str, image_id: &str) {
        self.init_runs();
        pipeline::remove_composer_image(self, conversation_id, image_id);
        crate::prelude::repaint();
    }

    /// `removeComposerFile`.
    pub fn remove_composer_file(&mut self, conversation_id: &str, file_id: &str) {
        self.init_runs();
        pipeline::remove_composer_file(self, conversation_id, file_id);
        crate::prelude::repaint();
    }

    /// `invalidateImages`: for conversations going away. In-flight uploads stop mattering; attachments and picks go.
    pub fn invalidate_composer_images(&mut self, conversation_ids: &[String]) {
        let stale = runs(self).composer.invalidate_images(conversation_ids);
        for started in stale {
            pipeline::start_stale_upload(self, started);
        }
        crate::prelude::repaint();
    }

    /// `putMessageInComposer`: hands a sent user message back as it was written: its picked elements as chips, its images and
    /// files as attachments, and its text without the `[Image #N]` the send added for the model. A draft the composer already
    /// holds is kept, after the message. The caret goes to the box.
    pub fn put_message_in_composer(&mut self, conversation_id: &str, item: &ContextItem) {
        let ContextItem::User { content, images, files, .. } = item else { return };
        let (written, picks) = selected_elements_from_text(content, images);
        let text = text_without_appended_image_placeholders(&written, images);
        let composer = &mut runs(self).composer;
        let draft = composer.drafts.entry(conversation_id.to_owned()).or_default();
        *draft = if draft.trim().is_empty() { text } else { format!("{text}\n\n{draft}") };
        composer.image_drafts.entry(conversation_id.to_owned()).or_default().extend(images.iter().cloned());
        composer.file_drafts.entry(conversation_id.to_owned()).or_default().extend(files.iter().cloned());
        composer.element_picks.entry(conversation_id.to_owned()).or_default().extend(picks);
        composer.focus_pending = true;
        crate::prelude::repaint();
    }
}

/// Runs.
impl App {
    /// `modelRunSummaries`: every live run, coarsely.
    pub fn model_run_summaries(&mut self) -> Vec<(String, Arc<ModelRunSummary>)> {
        runs(self).controller.summaries().iter().map(|(id, summary)| (id.clone(), summary.clone())).collect()
    }

    /// `activeModelRunBusy`: a run exists, or is being prepared or torn down.
    pub fn model_run_is_active(&mut self, conversation_id: &str) -> bool {
        let controller = &runs(self).controller;
        controller.run(conversation_id).is_some() || controller.has_run_token(conversation_id) || controller.has_performing_run(conversation_id) || controller.has_preparing_run(conversation_id)
    }

    /// `activeModelStopping`: a Stop is on its way.
    pub fn model_is_stopping(&mut self, conversation_id: &str) -> bool {
        runs(self).model_stopping_ids.contains(conversation_id)
    }

    /// `modelRunErrors[conversation]`: the composer's notice shows when its `request_id` is `None`; a turn renders the rest.
    pub fn model_run_error(&mut self, conversation_id: &str) -> Option<ModelRunErrorState> {
        runs(self).model_run_errors.get(conversation_id).cloned()
    }

    /// `clearModelRunError(conversation, "failure")`: dismisses a failure from every surface it reached.
    pub fn dismiss_model_run_error(&mut self, conversation_id: &str) {
        self.init_runs();
        pipeline::clear_model_run_error(self, conversation_id, ClearErrorScope::Failure);
        crate::prelude::repaint();
    }

    /// `retryActiveModelRun`: runs the active conversation's failed request again.
    pub fn retry_model_run(&mut self) {
        self.init_runs();
        pipeline::retry_active_model_run(self);
    }

    /// `stopModelRun`: asks the host to cancel the run. `done` hears `true` once it has settled, `false` on no run, on a
    /// refusal, or if it has not settled after 15 seconds (the run keeps showing and another stop may follow).
    pub fn cancel_run(&mut self, conversation_id: &str, done: impl FnOnce(&mut App, bool) + Send + 'static) {
        self.init_runs();
        pipeline::stop_model_run(self, conversation_id, None, Some(Box::new(done)));
    }

    /// `stopBrowserAutomation`: stops the run driving the preview page, then the browser, which the in-flight call may
    /// itself need in order to unwind. TODO: the status it returns belongs to the browser controller, once ported.
    pub fn stop_browser_automation(&mut self, conversation_id: &str, done: impl FnOnce(&mut App, bool) + Send + 'static) {
        self.init_runs();
        let driving = self.local::<RunsState>().controller.run(conversation_id).is_some_and(|run| super::run_contexts::browser_automation_tool_for_run(Some(run)).is_some());
        if !driving {
            return done(self, false);
        }
        let id = conversation_id.to_owned();
        let on_ack: Box<dyn FnOnce(&Cx) + Send> = Box::new(move |cx| {
            // Browser cleanup failed after the preview tool stopped; the next toggle reconciles the surface.
            let _ = seams::call_unit(cx, "browser_action", serde_json::json!({ "sessionId": id, "action": "stop", "value": null }));
        });
        pipeline::stop_model_run(self, conversation_id, Some(on_ack), Some(Box::new(done)));
    }

    /// `compactConversationNow`. `done` hears `false` when configuration or an existing run blocked it.
    pub fn compact_conversation_now(&mut self, workspace_id: &str, conversation_id: &str, done: impl FnOnce(&mut App, bool) + Send + 'static) {
        self.init_runs();
        let gate = pipeline::compact_now_gate(self, workspace_id, conversation_id);
        launch_gate(self, gate, Some(Box::new(done)));
    }

    /// `wakeConversation`: a run with no new message, to deliver a settled task. `done` hears `true` when an existing
    /// or started run will consume it. The host's wake events do this on their own; this is for a caller that wants it now.
    pub fn wake_conversation(&mut self, workspace_id: &str, conversation_id: &str, done: impl FnOnce(&mut App, bool) + Send + 'static) {
        self.init_runs();
        let gate = pipeline::wake_gate(self, workspace_id, conversation_id);
        launch_gate(self, gate, Some(Box::new(done)));
    }

    /// `startForkedConversationRun`.
    pub fn start_forked_conversation_run(&mut self, workspace_id: &str, conversation_id: &str, done: impl FnOnce(&mut App, bool) + Send + 'static) {
        self.init_runs();
        let gate = pipeline::forked_run_gate(self, workspace_id, conversation_id);
        launch_gate(self, gate, Some(Box::new(done)));
    }

    /// `controlWorkflowStep`: a refused control is non-fatal, as the workflow may already have ended.
    pub fn control_workflow_step(&mut self, conversation_id: &str, run_id: &str, step_index: u64, action: WorkflowStepAction) {
        let (conversation_id, run_id) = (conversation_id.to_owned(), run_id.to_owned());
        background(self, move |cx| {
            let _ = cx.control_workflow_step(&conversation_id, &run_id, step_index, action);
        });
    }

    /// `workflowStepRecord`: one workflow step's full record from the run directory; `None` when it no longer exists.
    pub fn workflow_step_record(&mut self, conversation_id: &str, run_id: &str, step_index: u64, done: impl FnOnce(&mut App, Option<SubagentRunRecord>) + Send + 'static) {
        let (conversation_id, run_id) = (conversation_id.to_owned(), run_id.to_owned());
        background(self, move |cx| {
            let record = cx.workflow_step_record(&conversation_id, &run_id, step_index).ok().flatten();
            cx.post(move |app| done(app, record));
        });
    }

    /// `settleConversationTitle`: the user named the conversation, so the local helper model must not rename it.
    pub fn settle_conversation_title(&mut self, conversation_id: &str) {
        let conversation_id = conversation_id.to_owned();
        background(self, move |cx| {
            if let Err(error) = cx.call::<serde_json::Value>("settle_conversation_title", serde_json::json!({ "conversationId": conversation_id })) {
                eprintln!("Could not settle the conversation title: {error}");
            }
        });
    }

    /// One-shot requests to views (`RunSignals`). Whoever handles one takes it.
    pub fn run_signals(&mut self) -> &mut RunSignals {
        &mut runs(self).signals
    }

    /// Where `undo` and `redo` of the timeline record (`TimelineHistory`). Runs append to the same list between an edit
    /// and its undo, which is why edits are patches.
    pub fn timeline_history(&mut self) -> &mut super::timeline_history::TimelineHistory {
        &mut runs(self).timeline_history
    }

    /// Stands in for the host, as upstream's tests mock `lib/runtime.ts`. Only for tests and tools.
    pub fn set_run_backend(&mut self, backend: Arc<dyn RunBackend>) {
        self.local::<RunsState>().backend = Some(backend);
    }

    /// Nothing to do now the run code reads `crate::selection` itself. TODO: drop with the call in `App::set_active`.
    pub fn set_runs_active_conversation(&mut self, _workspace_id: Option<&str>, _conversation_id: Option<&str>) {}
}

/// Tool prompts.
impl App {
    /// `toolPrompts[conversation]`: the outstanding cards, oldest first.
    pub fn tool_prompts(&mut self, conversation_id: &str) -> Vec<PendingToolPrompt> {
        runs(self).tool_prompts.get(conversation_id).cloned().unwrap_or_default()
    }

    /// `activeToolPrompt`: the card the composer shows, with its index and how many there are.
    pub fn active_tool_prompt(&mut self, conversation_id: &str) -> Option<(PendingToolPrompt, usize, usize)> {
        let state = runs(self);
        let queue = state.tool_prompts.get(conversation_id)?;
        let cursor = state.tool_prompt_cursors.get(conversation_id).copied().unwrap_or(0).min(queue.len().saturating_sub(1));
        Some((queue.get(cursor)?.clone(), cursor, queue.len()))
    }

    /// `activeToolPromptStack.onNavigate`: flips through the stack.
    pub fn navigate_tool_prompt(&mut self, conversation_id: &str, delta: isize) {
        let Some((_, cursor, total)) = self.active_tool_prompt(conversation_id) else { return };
        let next = (cursor as isize + delta).clamp(0, total as isize - 1) as usize;
        runs(self).tool_prompt_cursors.insert(conversation_id.to_owned(), next);
        crate::prelude::repaint();
    }

    /// `setToolPromptCursors`: points the stack at a card, for a view that navigated to it.
    pub fn set_tool_prompt_cursor(&mut self, conversation_id: &str, index: usize) {
        runs(self).tool_prompt_cursors.insert(conversation_id.to_owned(), index);
        crate::prelude::repaint();
    }

    /// The effect that follows a new card to where it is answered: the newest card nobody has been taken to yet, with
    /// the stack's cursor moved to it. `find_agent` is `approvalPromptSubagentView`: the agent page a card with a
    /// `sourceAgent` belongs to, or `None` while that page does not exist yet (the card waits for it). Call each frame
    /// for the conversation on screen.
    pub fn next_tool_prompt_navigation<T>(&mut self, conversation_id: &str, mut find_agent: impl FnMut(&PendingToolPrompt) -> Option<T>) -> Option<ToolPromptNavigation<T>> {
        let state = runs(self);
        let queue = state.tool_prompts.get(conversation_id)?.clone();
        for (index, prompt) in queue.iter().enumerate().rev() {
            if state.navigated_tool_prompt_ids.contains(&prompt.prompt_id) {
                continue;
            }
            if prompt.kind == PromptKind::PlanExit {
                state.navigated_tool_prompt_ids.insert(prompt.prompt_id.clone());
                state.tool_prompt_cursors.insert(conversation_id.to_owned(), index);
                return Some(ToolPromptNavigation::PlanPane);
            }
            if prompt.source_agent.is_none() && prompt.source_call_id.is_none() {
                state.navigated_tool_prompt_ids.insert(prompt.prompt_id.clone());
                continue;
            }
            let Some(target) = find_agent(prompt) else { continue };
            state.navigated_tool_prompt_ids.insert(prompt.prompt_id.clone());
            state.tool_prompt_cursors.insert(conversation_id.to_owned(), index);
            return Some(ToolPromptNavigation::Subagent(target));
        }
        None
    }

    /// `activeQuestionPending`: the run is blocked on an `ask_user` card, and a composer message answers it first.
    pub fn question_is_pending(&mut self, conversation_id: &str) -> bool {
        runs(self).tool_prompts.get(conversation_id).is_some_and(|prompts| prompts.iter().any(|prompt| prompt.kind == PromptKind::Question))
    }

    /// `decideToolPrompt`: answers one card. A card raised inside a model run resumes the blocked worker and returns
    /// nothing; a card the renderer raised itself hands the minted grant back to whoever is awaiting it.
    pub fn decide_tool_prompt(&mut self, conversation_id: &str, prompt_id: &str, decision: ToolPromptDecision, feedback: Option<&str>) {
        self.init_runs();
        let waiter = self.local::<RunsState>().manual_tool_prompts.remove(prompt_id);
        pipeline::close_tool_prompt(self, conversation_id, prompt_id);
        let (prompt_id, feedback) = (prompt_id.to_owned(), feedback.map(str::to_owned));
        background(self, move |cx| {
            let answered = cx.resolve_tool_prompt(&prompt_id, decision, feedback.as_deref(), None);
            // The backend retracts a model card itself with `tool_approval_resolved`,
            // so a rejected submission has no renderer-side compensation left.
            if let Some(waiter) = waiter {
                waiter.resolve(answered);
            }
        });
        crate::prelude::repaint();
    }

    /// `rememberQuestionDraft`: what an open question card would hand back if a composer message were sent now.
    pub fn remember_question_draft(&mut self, prompt_id: &str, response: QuestionResponse) {
        runs(self).question_drafts.insert(prompt_id.to_owned(), response);
    }

    /// `answerQuestionPrompt`: answers a question card. `done` hears `false` when the host did not take the answer.
    pub fn answer_question_prompt(&mut self, conversation_id: &str, prompt_id: &str, response: QuestionResponse, done: impl FnOnce(&mut App, bool) + Send + 'static) {
        let (conversation_id, prompt_id) = (conversation_id.to_owned(), prompt_id.to_owned());
        background(self, move |cx| {
            let taken = pipeline::answer_question_blocking(&cx, &conversation_id, &prompt_id, &response);
            cx.post(move |app| {
                done(app, taken);
                crate::prelude::repaint();
            });
        });
    }
}

/// Plans, forks, explanations.
impl App {
    /// `plans[conversation]`.
    pub fn conversation_plan(&mut self, conversation_id: &str) -> Option<ConversationPlan> {
        runs(self).plans.get(conversation_id).cloned().flatten()
    }

    /// `forkRequests`: every card waiting for the user, whichever conversation raised it.
    pub fn fork_requests(&mut self) -> Vec<PendingForkRequest> {
        runs(self).fork_requests.clone()
    }

    /// `forkDecisions[conversation]`: the task bar's rows.
    pub fn fork_decisions(&mut self, conversation_id: &str) -> Vec<ForkDecisionRecord> {
        runs(self).fork_decisions.get(conversation_id).cloned().unwrap_or_default()
    }

    /// `decideForkRequest`: answers one fork card. The card comes down at once; the host publishes `forkResolved` for
    /// the outcome, and that event — shared with the auto-approved path — is what starts the child's run.
    pub fn decide_fork_request(&mut self, fork_id: &str, approved: bool) {
        self.init_runs();
        close_fork_request(self, fork_id);
        let fork_id = fork_id.to_owned();
        background(self, move |cx| {
            if let Err(error) = cx.resolve_fork_request(&fork_id, approved) {
                eprintln!("{}: {error}", t("分叉请求未能提交给宿主", "The fork decision could not be delivered to the host"));
            }
        });
        crate::prelude::repaint();
    }

    /// `forkStartError`.
    pub fn fork_start_error(&mut self) -> Option<String> {
        runs(self).fork_start_error.clone()
    }

    /// The error's retry button: everything goes back on the list from the host's durable intents.
    pub fn retry_fork_starts(&mut self) {
        let state = runs(self);
        state.fork_start_error = None;
        state.fork_starts_attempted.clear();
        background(self, move |cx| list_pending_fork_starts(&cx));
        crate::prelude::repaint();
    }

    /// `toolExplanation`: a tool card's explanation, by its id or, while running, its call id.
    pub fn tool_explanation(&mut self, context_id: &str, conversation_id: Option<&str>, call_id: Option<&str>) -> Option<String> {
        let state = runs(self);
        state.explanations.get(context_id).or_else(|| state.explanations.get(&format!("{}\0{}", conversation_id?, call_id?))).cloned()
    }

    /// `toolErrorExplanation`: why a failed tool card failed, in the local helper model's words.
    pub fn tool_error_explanation(&mut self, context_id: &str) -> Option<String> {
        runs(self).explanation_errors.get(context_id).cloned()
    }

    /// `toolExplanationVersion`: changes whenever an explanation arrives.
    pub fn tool_explanation_version(&mut self) -> u64 {
        runs(self).explanation_version
    }
}
