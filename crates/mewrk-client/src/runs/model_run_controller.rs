//! Port of `lib/modelRunController.ts`: the live runs, their coarse summaries, and the run-ownership guards.
//!
//! The subscriptions upstream's view tree hangs on are revision counters here: a view that cares compares them per
//! frame, and nothing re-renders for a commit it does not read.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc;
use std::sync::Arc;

use super::model_stream::ModelRunState;
use super::run_contexts::browser_automation_tool_for_run;

/// Coarse-grained run summary containing fields that remain stable or change
/// infrequently during a run. Reading this instead of the full runs keeps
/// streaming text, reasoning, and tool-output deltas out of whoever only asks whether a run exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRunSummary {
    pub request_id: String,
    pub provider_name: String,
    pub model_name: String,
    pub workspace_id: String,
    /// Model profile used for the request; the steer guard uses it to allow images.
    pub supports_vision: bool,
    /// Queued message ids steered into this request, in delivery order.
    pub steered_message_ids: Vec<String>,
    /// Most recent tool driving browser automation, or `None`.
    pub browser_automation_tool: Option<String>,
}

fn derive_summary(run: &ModelRunState) -> ModelRunSummary {
    ModelRunSummary {
        request_id: run.request_id.clone(),
        provider_name: run.provider_name.clone(),
        model_name: run.model_name.clone(),
        workspace_id: run.workspace_id.clone(),
        supports_vision: run.request.model.supports_vision(),
        steered_message_ids: run.steered_inputs_by_round.values().flatten().map(|message| message.id().to_owned()).collect(),
        browser_automation_tool: browser_automation_tool_for_run(Some(run)),
    }
}

#[derive(Default)]
pub struct ModelRunController {
    runs: BTreeMap<String, ModelRunState>,
    summaries: BTreeMap<String, Arc<ModelRunSummary>>,
    /// Bumped by every run-state commit, including streaming flushes.
    revision: u64,
    revisions_by_conversation: HashMap<String, u64>,
    /// Bumped only when a conversation's summary changes.
    summary_revision: u64,
    run_tokens: HashMap<String, String>,
    preparing_runs: HashSet<String>,
    performing_runs: HashSet<String>,
    exit_waiters: Vec<(String, String, mpsc::Sender<()>)>,
}

impl ModelRunController {
    pub fn current(&self) -> &BTreeMap<String, ModelRunState> {
        &self.runs
    }

    pub fn run(&self, conversation_id: &str) -> Option<&ModelRunState> {
        self.runs.get(conversation_id)
    }

    /// The sole run-state commit point for one conversation. `f` answers whether it changed anything; nothing is
    /// published for a `false`.
    pub fn update_run(&mut self, conversation_id: &str, f: impl FnOnce(&mut ModelRunState) -> bool) {
        let Some(run) = self.runs.get_mut(conversation_id) else { return };
        if f(run) {
            self.published(conversation_id);
        }
    }

    /// Registers a run.
    pub fn insert(&mut self, conversation_id: &str, run: ModelRunState) {
        self.runs.insert(conversation_id.to_owned(), run);
        self.published(conversation_id);
    }

    /// Removes a run; `false` when there was none.
    pub fn remove(&mut self, conversation_id: &str) -> bool {
        if self.runs.remove(conversation_id).is_none() {
            return false;
        }
        self.published(conversation_id);
        true
    }

    fn published(&mut self, conversation_id: &str) {
        self.revision += 1;
        *self.revisions_by_conversation.entry(conversation_id.to_owned()).or_default() = self.revision;
        // Recompute only this conversation's summary, retaining every other entry as it was.
        match self.runs.get(conversation_id).map(derive_summary) {
            Some(derived) => {
                if self.summaries.get(conversation_id).map(|previous| &**previous) != Some(&derived) {
                    self.summaries.insert(conversation_id.to_owned(), Arc::new(derived));
                    self.summary_revision += 1;
                }
            }
            None => {
                if self.summaries.remove(conversation_id).is_some() {
                    self.summary_revision += 1;
                }
            }
        }
        self.settle_exit_waiters();
    }

    /// Changes with every commit, so a view can tell whether anything streamed since it last looked.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The same for one conversation.
    pub fn revision_of(&self, conversation_id: &str) -> u64 {
        self.revisions_by_conversation.get(conversation_id).copied().unwrap_or(0)
    }

    /// Changes only when a summary does.
    pub fn summary_revision(&self) -> u64 {
        self.summary_revision
    }

    pub fn summaries(&self) -> &BTreeMap<String, Arc<ModelRunSummary>> {
        &self.summaries
    }

    /// Non-reactive run-ownership token used by stream callbacks and cleanup.
    pub fn run_token(&self, conversation_id: &str) -> Option<&str> {
        self.run_tokens.get(conversation_id).map(String::as_str)
    }

    pub fn set_run_token(&mut self, conversation_id: &str, request_id: &str) {
        self.run_tokens.insert(conversation_id.to_owned(), request_id.to_owned());
    }

    pub fn delete_run_token(&mut self, conversation_id: &str) {
        self.run_tokens.remove(conversation_id);
        self.settle_exit_waiters();
    }

    pub fn has_run_token(&self, conversation_id: &str) -> bool {
        self.run_tokens.contains_key(conversation_id)
    }

    fn has_generation(&self, conversation_id: &str, request_id: &str) -> bool {
        self.runs.get(conversation_id).is_some_and(|run| run.request_id == request_id) || self.run_tokens.get(conversation_id).is_some_and(|token| token == request_id)
    }

    /// A receiver that hears once this exact run generation has left both live stores.
    pub fn wait_for_run_exit(&mut self, conversation_id: &str, request_id: &str) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        if self.has_generation(conversation_id, request_id) {
            self.exit_waiters.push((conversation_id.to_owned(), request_id.to_owned(), tx));
        } else {
            let _ = tx.send(());
        }
        rx
    }

    fn settle_exit_waiters(&mut self) {
        if self.exit_waiters.is_empty() {
            return;
        }
        let waiters = std::mem::take(&mut self.exit_waiters);
        for (conversation_id, request_id, tx) in waiters {
            if self.has_generation(&conversation_id, &request_id) {
                self.exit_waiters.push((conversation_id, request_id, tx));
            } else {
                let _ = tx.send(());
            }
        }
    }

    /// Non-reactive prepare/perform guards for the send pipeline.
    pub fn add_preparing_run(&mut self, conversation_id: &str) {
        self.preparing_runs.insert(conversation_id.to_owned());
    }

    pub fn delete_preparing_run(&mut self, conversation_id: &str) {
        self.preparing_runs.remove(conversation_id);
    }

    pub fn has_preparing_run(&self, conversation_id: &str) -> bool {
        self.preparing_runs.contains(conversation_id)
    }

    pub fn add_performing_run(&mut self, conversation_id: &str) {
        self.performing_runs.insert(conversation_id.to_owned());
    }

    pub fn delete_performing_run(&mut self, conversation_id: &str) {
        self.performing_runs.remove(conversation_id);
    }

    pub fn has_performing_run(&self, conversation_id: &str) -> bool {
        self.performing_runs.contains(conversation_id)
    }
}
