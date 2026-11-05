//! The push events runs care about, and upstream's effects that followed from them.

use std::time::{Duration, Instant};

use serde_json::Value;

use super::bridge::Cx;
use super::composer::Deferred;
use super::context_tokens::{estimate_wire_tokens, wire_view};
use super::conversation_queue::queue_is_paused;
use super::pipeline::{self, close_tool_prompt, open_tool_prompt, RunGate};
use super::seams::{self, model_choice, workspace_of};
use super::state::RunsState;
use super::types::*;
use crate::i18n::t;
use crate::model::{ConversationPlan, Conversation};
use crate::App;

fn runs(app: &mut App) -> &mut RunsState {
    app.local::<RunsState>()
}

fn call_key(conversation_id: &str, call_id: &str) -> String {
    format!("{conversation_id}\0{call_id}")
}

fn text(event: &Value, key: &str) -> String {
    event.get(key).and_then(Value::as_str).unwrap_or("").to_owned()
}

/// `onAppPushEvent`, for what runs own. Everything else passes by.
pub(super) fn handle_push_event(app: &mut App, event: &Value) {
    let conversation_id = text(event, "conversationId");
    match event.get("type").and_then(Value::as_str) {
        // Record wake notifications here; the reconcile performs the wake once adoption is done.
        Some("taskSettled") => {
            runs(app).pending_wake_conversations.insert(conversation_id);
        }
        // Background task approvals use push delivery because no running stream can carry their card; they outlive turn boundaries.
        Some("toolApprovalRequested") => {
            if let Ok(prompt) = serde_json::from_value::<PendingToolPrompt>(event.clone()) {
                open_tool_prompt(app, &conversation_id, prompt);
            }
        }
        Some("toolApprovalResolved") => close_tool_prompt(app, &conversation_id, &text(event, "promptId")),
        Some("conversationTitleChanged") => {
            // The host wrote it (placeholder, then the local helper model's title);
            // mirrored without writing back.
            let title = text(event, "title");
            if let Some(workspace_id) = workspace_of(&app.doc, &conversation_id).map(|workspace| workspace.id.clone()) {
                app.update_conversation_local(&workspace_id, &conversation_id, |conversation| {
                    if conversation.title == title {
                        return false;
                    }
                    conversation.title = title;
                    true
                });
            }
        }
        Some("conversationPlanUpdated") => {
            let plan: Option<ConversationPlan> = event.get("plan").cloned().and_then(|plan| serde_json::from_value(plan).ok());
            let state = runs(app);
            state.plan_push_count += 1;
            state.plans.insert(conversation_id, plan);
        }
        Some("conversationPlanModeChanged") => {
            // The host already wrote it: an approved plan turned plan mode off. It is
            // written back all the same, because a settings write still waiting in the
            // commit queue was built before this and carries the old value, and the
            // latest write for a conversation is the one that lands.
            let enabled = event.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            let Some(workspace_id) = workspace_of(&app.doc, &conversation_id).map(|workspace| workspace.id.clone()) else { return };
            app.update_conversation_if(&workspace_id, &conversation_id, |conversation| {
                if conversation.settings.plan_mode_enabled == enabled {
                    return false;
                }
                conversation.settings.plan_mode_enabled = enabled;
                true
            });
        }
        // Fork cards are global: the tool call that raised one has already returned, so the
        // card belongs to no run and the tray draws it whichever conversation is open.
        Some("forkRequested") => {
            if let Ok(request) = serde_json::from_value::<PendingForkRequest>(event.clone()) {
                open_fork_request(app, request);
            }
        }
        Some("forkResolved") => {
            close_fork_request(app, &text(event, "forkId"));
            // A retraction carries no decision; an answered card carries the row the task bar shows.
            if let Some(decision) = event.get("decision").cloned().and_then(|decision| serde_json::from_value::<ForkDecisionRecord>(decision).ok()) {
                append_fork_decision(app, decision);
            }
            // The child's first run is started from the reconcile rather than here, so it
            // waits for run adoption exactly as a wake does and cannot race it.
            if let Some(child) = event.get("childConversationId").and_then(Value::as_str) {
                runs(app).pending_fork_starts.push(PendingForkStart { workspace_id: text(event, "workspaceId"), conversation_id: child.to_owned(), prompt_context_id: String::new(), starts_run: true });
            }
        }
        // The model handed its conversation off and the host committed the
        // continuation, armed on its opening message. It is not a fork, but its
        // first run starts the way a host-made fork's does.
        Some("conversationHandedOff") => {
            let child = text(event, "childConversationId");
            let state = runs(app);
            state.handoff_jumps.insert(child.clone(), text(event, "sourceConversationId"));
            state.pending_fork_starts.push(PendingForkStart {
                workspace_id: text(event, "workspaceId"),
                conversation_id: child,
                prompt_context_id: String::new(),
                // A compaction the user asked for leaves its continuation waiting for them.
                starts_run: event.get("startsRun").and_then(Value::as_bool) != Some(false),
            });
        }
        Some("toolExplained") => {
            let is_error = event.get("error").and_then(Value::as_bool).unwrap_or(false);
            let (context_id, call_id, explanation) = (text(event, "contextId"), text(event, "callId"), text(event, "text"));
            let state = runs(app);
            let map = if is_error { &mut state.explanation_errors } else { &mut state.explanations };
            map.insert(context_id, explanation.clone());
            map.insert(call_key(&conversation_id, &call_id), explanation);
            state.explanation_version += 1;
        }
        _ => return,
    }
    app.note_runs_changed();
}

/// `openForkRequest`.
pub(super) fn open_fork_request(app: &mut App, request: PendingForkRequest) {
    let requests = &mut runs(app).fork_requests;
    if !requests.iter().any(|pending| pending.fork_id == request.fork_id) {
        requests.push(request);
    }
}

/// `closeForkRequest`.
pub(super) fn close_fork_request(app: &mut App, fork_id: &str) {
    runs(app).fork_requests.retain(|pending| pending.fork_id != fork_id);
}

/// `appendForkDecision`.
fn append_fork_decision(app: &mut App, decision: ForkDecisionRecord) {
    let rows = runs(app).fork_decisions.entry(decision.source_conversation_id.clone()).or_default();
    if !rows.iter().any(|row| row.fork_id == decision.fork_id) {
        rows.push(decision);
    }
}

impl App {
    /// Sets up the run code: reads the stored turns and listens to the host. Called by every entry point, and safe to call early.
    pub fn init_runs(&mut self) {
        if std::mem::replace(&mut self.local::<RunsState>().started, true) {
            return;
        }
        self.load_turns();
        self.on_push_event(handle_push_event);
        self.doc_hooks().timeline_notice.push(std::rc::Rc::new(|app, message, duration_ms| app.show_timeline_notice(None, message, None, duration_ms)));
        self.runs_reconcile();
    }

    /// Something a run's effects depend on changed: reconcile soon, and redraw.
    pub(super) fn note_runs_changed(&mut self) {
        crate::prelude::repaint();
        let state = self.local::<RunsState>();
        if std::mem::replace(&mut state.reconcile_posted, true) {
            return;
        }
        self.post(|app| {
            app.local::<RunsState>().reconcile_posted = false;
            app.runs_reconcile();
        });
    }

    pub(super) fn back_off_reconcile(&mut self, conversation_id: &str) {
        self.local::<RunsState>().reconcile_retry_after.insert(conversation_id.to_owned(), Instant::now() + Duration::from_secs(3));
    }

    /// What upstream's effects do when the document or the runs change: adopts surviving runs, then starts forks, wakes
    /// and queued messages that have become possible. Idempotent and cheap, so call it once a frame, or whenever the
    /// document changes (a model picked, a provider enabled, a queue resumed): nothing here runs on a timer.
    pub fn runs_reconcile(&mut self) {
        self.init_runs();
        self.begin_adoption();
        if !runs(self).resumable_runs_adopted {
            return;
        }
        self.track_unseen_completions();
        self.start_pending_forks();
        self.start_pending_wakes();
        self.dispatch_queue_heads();
    }

    /// `unseenCompletions`: conversations whose run finished while another one was open.
    fn track_unseen_completions(&mut self) {
        let active = self.active_conversation_id();
        let state = runs(self);
        let running: std::collections::HashSet<String> = state.controller.summaries().keys().cloned().collect();
        let finished: Vec<String> = state.running_conversation_ids.iter().filter(|id| !running.contains(*id) && Some(*id) != active.as_ref()).cloned().collect();
        state.running_conversation_ids = running;
        state.unseen_completions.extend(finished);
        if let Some(active) = active {
            state.unseen_completions.remove(&active);
        }
    }

    /// After loading, adopt host runs that are still live or awaiting settlement. Queue dispatch waits for adoption so
    /// both paths cannot race for one conversation. A latch allows only one adoption; failed adoption reopens it.
    fn begin_adoption(&mut self) {
        if std::mem::replace(&mut runs(self).adoption_started, true) {
            return;
        }
        let cx = Cx::of(self);
        self.spawn(move |_| match pipeline::adopt_resumable_runs(&cx) {
            Ok(()) => {
                cx.update(|app| {
                    runs(app).resumable_runs_adopted = true;
                    app.note_runs_changed();
                });
                rescan_after_adoption(&cx);
            }
            Err(_) => cx.post(|app| {
                app.set_timer(Duration::from_millis(3000), |app| {
                    runs(app).adoption_started = false;
                    app.note_runs_changed();
                });
            }),
        });
    }

    /// Start the first run of every child the host forked. The host wrote the child (history copy plus the prompt as
    /// its last user message) and published `forkResolved`; the renderer is the only run starter, so it loads the
    /// authoritative body, files it under its workspace — without a create command, the host already owns it — and runs
    /// it. Waits for adoption like a wake does.
    fn start_pending_forks(&mut self) {
        let starts = std::mem::take(&mut runs(self).pending_fork_starts);
        for start in starts {
            if !runs(self).fork_starts_attempted.insert(start.conversation_id.clone()) {
                continue;
            }
            let cx = Cx::of(self);
            self.spawn(move |_| {
                if let Err(error) = start_fork(&cx, &start) {
                    let message = if error.trim().is_empty() { t("分叉出的会话未能开始运行", "The forked conversation could not start its run").to_owned() } else { error };
                    cx.post(move |app| {
                        runs(app).fork_start_error = Some(message);
                        crate::prelude::repaint();
                    });
                }
            });
        }
    }

    /// Start a no-user-message wake run for every conversation with pending wake state — on screen or not, as queued
    /// messages and forks already do — but only after adoption completes to avoid competing for a conversation. Its
    /// sidebar row shows it working.
    fn start_pending_wakes(&mut self) {
        let pending: Vec<String> = runs(self).pending_wake_conversations.iter().cloned().collect();
        let now = Instant::now();
        for conversation_id in pending {
            if runs(self).reconcile_retry_after.get(&conversation_id).is_some_and(|until| *until > now) {
                continue;
            }
            // A conversation the document no longer has (deleted meanwhile) has nothing to wake.
            let Some(workspace_id) = workspace_of(&self.doc, &conversation_id).map(|workspace| workspace.id.clone()) else {
                runs(self).pending_wake_conversations.remove(&conversation_id);
                continue;
            };
            // Consume the record before the run starts, not after. A wake only finishes once
            // the run it starts has settled, and that run's own state updates reopen this
            // reconcile — with the record still in place, the rerun that lands after the run
            // is gone starts a second, duplicate wake run. A wake that could not proceed puts
            // the record back without signalling, so the next natural rerun retries it.
            match pipeline::wake_gate(self, &workspace_id, &conversation_id) {
                RunGate::Consumed => {
                    runs(self).pending_wake_conversations.remove(&conversation_id);
                }
                RunGate::Blocked => {}
                RunGate::Start(prepared) => {
                    runs(self).pending_wake_conversations.remove(&conversation_id);
                    let cx = Cx::of(self);
                    self.spawn(move |_| {
                        if !pipeline::launch_prepared(&cx, *prepared) {
                            cx.post(move |app| {
                                runs(app).pending_wake_conversations.insert(conversation_id);
                            });
                        }
                    });
                }
            }
        }
    }

    /// A round that ends on its own hands the queue its next message, one round each. A Stop paused the conversation's
    /// queue instead: it waits for the user's next send.
    fn dispatch_queue_heads(&mut self) {
        let now = Instant::now();
        let mut heads: Vec<(String, String)> = Vec::new();
        for workspace in &self.doc.workspaces {
            for conversation in &workspace.conversations {
                if !conversation.queued_messages.is_empty() && !queue_is_paused(conversation) {
                    heads.push((workspace.id.clone(), conversation.id.clone()));
                }
            }
        }
        for (workspace_id, conversation_id) in heads {
            let head = workspace_of(&self.doc, &conversation_id).and_then(|workspace| workspace.conversations.iter().find(|conversation| conversation.id == conversation_id)).and_then(|conversation| conversation.queued_messages.first()).map(|message| message.id.clone());
            let Some(head) = head else { continue };
            let state = runs(self);
            if state.composer.failed_queued_promotion_ids.contains(&head)
                || state.controller.run(&conversation_id).is_some()
                || state.controller.has_performing_run(&conversation_id)
                || state.controller.has_preparing_run(&conversation_id)
                || state.reconcile_retry_after.get(&conversation_id).is_some_and(|until| *until > now)
            {
                continue;
            }
            pipeline::dispatch_next_queued_message(self, &workspace_id, &conversation_id);
        }
    }

    /// `estimateActiveContextUsage`.
    pub(super) fn estimate_context_usage(&mut self, contexts: &[crate::model::ContextItem]) -> ContextUsage {
        let (provider, model) = model_choice(&self.doc);
        let target = provider.filter(|provider| provider.enabled).zip(model);
        ContextUsage {
            tokens: estimate_wire_tokens(wire_view(contexts, target)),
            estimated: true,
            unprojectable: false,
            provider_id: target.map(|(provider, _)| provider.id.clone()),
            family: target.map(|(provider, _)| provider.family),
        }
    }

    /// `selectConversation`'s run-side effects: fetches the plan and fork rows and explanations a conversation shows.
    /// Call when a conversation becomes active.
    pub fn note_conversation_activated(&mut self, conversation_id: &str) {
        self.init_runs();
        runs(self).unseen_completions.remove(conversation_id);
        if seams::is_draft_conversation_id(conversation_id) {
            return;
        }
        self.load_conversation_plan(conversation_id);
        self.load_fork_decisions(conversation_id);
        self.load_tool_explanations(conversation_id);
    }

    /// The plan lives at the host, so it has to be fetched when a conversation becomes active; `conversationPlanUpdated`
    /// keeps it current from then on.
    pub fn load_conversation_plan(&mut self, conversation_id: &str) {
        let requested_at = runs(self).plan_push_count;
        let cx = Cx::of(self);
        let conversation_id = conversation_id.to_owned();
        self.spawn(move |_| match cx.load_conversation_plan(&conversation_id) {
            Ok(plan) => cx.post(move |app| {
                // A push that landed while this was in flight is newer than the answer.
                if runs(app).plan_push_count == requested_at {
                    runs(app).plans.insert(conversation_id, plan);
                    crate::prelude::repaint();
                }
            }),
            Err(error) => eprintln!("Failed to load the conversation plan: {error}"),
        });
    }

    /// Fork decisions are host rows: re-read them whenever a conversation becomes active, so a card answered while
    /// another conversation was open still shows up in this one's task bar.
    pub fn load_fork_decisions(&mut self, conversation_id: &str) {
        let cx = Cx::of(self);
        let conversation_id = conversation_id.to_owned();
        self.spawn(move |_| {
            // The rows are advisory; a failed read keeps whatever was already shown.
            if let Ok(records) = cx.list_fork_decisions(&conversation_id) {
                cx.post(move |app| {
                    runs(app).fork_decisions.insert(conversation_id, records);
                    crate::prelude::repaint();
                });
            }
        });
    }

    /// `loadToolExplanations`: fills what the helper model already wrote, once per conversation.
    pub fn load_tool_explanations(&mut self, conversation_id: &str) {
        if !runs(self).explanations_loaded.insert(conversation_id.to_owned()) {
            return;
        }
        let cx = Cx::of(self);
        let conversation_id = conversation_id.to_owned();
        self.spawn(move |_| {
            let Ok(stored) = cx.get_tool_explanations(&conversation_id) else {
                return cx.post(move |app| {
                    runs(app).explanations_loaded.remove(&conversation_id);
                });
            };
            cx.post(move |app| {
                let state = runs(app);
                state.explanations.extend(stored.explanations);
                state.explanation_errors.extend(stored.errors);
                state.explanation_version += 1;
                crate::prelude::repaint();
            });
        });
    }

    /// `requestToolApproval`, `awaitManualToolApproval`, `executeTool`: runs a tool call the user assembled by hand,
    /// drawing the card first when the backend wants one. `done` hears the result on the UI thread.
    pub fn run_manual_tool(&mut self, conversation_id: &str, request: crate::model::ToolExecutionRequest, done: impl FnOnce(&mut App, Result<crate::model::ToolResult, String>) + Send + 'static) {
        let cx = Cx::of(self);
        let conversation_id = conversation_id.to_owned();
        self.spawn(move |_| {
            let result = (|| {
                let approval = cx.request_tool_approval(&request)?;
                let granted = match approval.prompt.clone() {
                    Some(prompt) => {
                        let waiter: Deferred<Result<ToolApprovalGrant, String>> = Deferred::new();
                        let registered = waiter.clone();
                        cx.update(move |app| {
                            runs(app).manual_tool_prompts.insert(prompt.prompt_id.clone(), registered);
                            open_tool_prompt(app, &conversation_id, prompt);
                            crate::prelude::repaint();
                        });
                        waiter.wait()?
                    }
                    None => approval,
                };
                cx.execute_tool(&request, granted.nonce.as_deref())
            })();
            cx.post(move |app| done(app, result));
        });
    }
}

/// `listWakePendingConversations`, `listPendingToolPrompts`, `listPendingForkRequests`, `listPendingForkStarts`: what the
/// edge-triggered notifications may have lost.
fn rescan_after_adoption(cx: &Cx) {
    // `taskSettled` is an edge signal that reload and bounded host backlogs can lose. After adoption, rescan
    // deliverable idle conversations to restore the level signal. Failure is non-fatal: a later edge notification can
    // restore the signal.
    if let Ok(conversation_ids) = cx.list_wake_pending_conversations() {
        if !conversation_ids.is_empty() {
            cx.post(move |app| {
                runs(app).pending_wake_conversations.extend(conversation_ids);
                app.note_runs_changed();
            });
        }
    }
    // Unresolved background-task approvals are not replayed with a run and may predate all open streams; prompt-id
    // deduplication makes duplicate push or replay delivery safe.
    if let Ok(prompts) = cx.list_pending_tool_prompts() {
        cx.post(move |app| {
            for row in prompts {
                open_tool_prompt(app, &row.conversation_id, row.prompt);
            }
            crate::prelude::repaint();
        });
    }
    // Fork cards belong to no run and outlive every stream; re-list them after adoption so a reload cannot hide a
    // request until its 30-minute expiry. Fork-id deduplication makes a duplicate push delivery safe.
    if let Ok(requests) = cx.list_pending_fork_requests() {
        cx.post(move |app| {
            for request in requests {
                open_fork_request(app, request);
            }
            crate::prelude::repaint();
        });
    }
    list_pending_fork_starts(cx);
}

/// The durable intent is authoritative; a successfully delivered push is not an acknowledgement.
pub(super) fn list_pending_fork_starts(cx: &Cx) {
    match cx.list_pending_fork_starts() {
        Ok(starts) => cx.post(move |app| {
            runs(app).pending_fork_starts.extend(starts);
            app.note_runs_changed();
        }),
        Err(error) => {
            let message = if error.trim().is_empty() { t("无法读取分叉首轮待启动记录", "Could not load pending fork runs").to_owned() } else { error };
            cx.post(move |app| {
                runs(app).fork_start_error = Some(message);
                crate::prelude::repaint();
            });
        }
    }
}

fn start_fork(cx: &Cx, start: &PendingForkStart) -> Result<(), String> {
    let authoritative = cx.call::<Option<Conversation>>("load_conversation", serde_json::json!({ "conversationId": start.conversation_id }))?;
    let Some(authoritative) = authoritative else { return Err(t("分叉子会话不存在", "The forked conversation is unavailable").to_owned()) };
    let handed_off = authoritative.handoff_of.is_some();
    let (workspace_id, conversation_id, starts_run) = (start.workspace_id.clone(), start.conversation_id.clone(), start.starts_run);
    let gate = cx.update(move |app| {
        // A new child opens the list, where the host filed it; one already listed (a start
        // retried after a restart) keeps its place.
        let id = authoritative.id.clone();
        let usage = app.estimate_context_usage(&authoritative.contexts);
        if let Some(workspace) = app.doc.workspaces.iter_mut().find(|workspace| workspace.id == workspace_id) {
            match workspace.conversations.iter_mut().find(|candidate| candidate.id == id) {
                Some(candidate) => *candidate = authoritative,
                None => workspace.conversations.insert(0, authoritative),
            }
            app.unloaded.remove(&id);
            app.document_changed();
        }
        runs(app).context_usage.insert(id.clone(), usage);
        if let Some(handed_off_from) = runs(app).handoff_jumps.remove(&conversation_id) {
            if app.active_conversation_id().as_deref() == Some(&handed_off_from) {
                app.select_conversation(&workspace_id, &conversation_id);
            }
        }
        crate::prelude::repaint();
        if !starts_run {
            return None;
        }
        Some(pipeline::forked_run_gate(app, &workspace_id, &conversation_id))
    });
    let started = match gate {
        None => return Ok(()),
        Some(RunGate::Start(prepared)) => pipeline::launch_prepared(cx, *prepared),
        Some(_) => false,
    };
    if started {
        return Ok(());
    }
    Err(if handed_off {
        t("交接会话首轮尚未启动，请检查模型配置后重试", "The handover conversation's run has not started. Check model settings and retry.").to_owned()
    } else {
        t("分叉首轮尚未启动，请检查模型配置后重试", "The fork run has not started. Check model settings and retry.").to_owned()
    })
}
