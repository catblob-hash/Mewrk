//! What the run code keeps for the life of the app: upstream's `App.tsx` state for runs, in one place.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::composer::{ComposerState, Deferred};
use super::conversation_turns::{drop_empty_turns, load_conversation_turns, ConversationTurn, ConversationTurns, CONVERSATION_TURNS_STORAGE_KEY};
use super::model_run_controller::ModelRunController;
use super::model_stream::ModelStreamCoalescer;
use super::timeline_history::TimelineHistory;
use super::types::{ContextUsage, ForkDecisionRecord, ModelRunErrorState, PendingForkRequest, PendingForkStart, PendingToolPrompt, QuestionResponse, ToolApprovalGrant};
use super::util::now_ms;
use super::view::RunViewCache;
use crate::model::ConversationPlan;
use crate::timers::TimerId;
use crate::App;

/// The commit buffer of one followed run: `createModelStreamCoalescer` and its flusher.
pub(super) struct StreamBuffer {
    pub workspace_id: String,
    pub request_id: String,
    pub coalescer: ModelStreamCoalescer,
    pub timer: Option<TimerId>,
}

/// A way for tests to stand in for the host, as upstream's tests mock `lib/runtime.ts`.
pub trait RunBackend: Send + Sync {
    /// `invoke(command, args)`. `events` is the channel of a command that streams.
    fn invoke(&self, command: &str, args: serde_json::Value, events: Option<EventSink>) -> Result<serde_json::Value, String>;
}

/// What a backend sends a command's stream events to.
pub type EventSink = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

#[derive(Default)]
pub struct RunsState {
    pub(super) started: bool,
    pub(super) backend: Option<Arc<dyn RunBackend>>,
    pub controller: ModelRunController,
    pub composer: ComposerState,
    /// `conversationTurns`. Write through [`App::update_conversation_turns`].
    pub turns: ConversationTurns,
    pub(super) turns_revision: u64,
    pub(super) turns_save: Option<TimerId>,
    pub model_run_errors: HashMap<String, ModelRunErrorState>,
    pub model_stopping_ids: HashSet<String>,
    pub context_usage: HashMap<String, ContextUsage>,
    pub tool_prompts: HashMap<String, Vec<PendingToolPrompt>>,
    pub tool_prompt_cursors: HashMap<String, usize>,
    /// What each open question card would hand back if a composer message were sent now.
    pub(super) question_drafts: HashMap<String, QuestionResponse>,
    /// Cards the renderer raised itself: the caller is waiting for the grant.
    pub(super) manual_tool_prompts: HashMap<String, Deferred<Result<ToolApprovalGrant, String>>>,
    /// A conversation with no plan is present with `None` once asked.
    pub plans: HashMap<String, Option<ConversationPlan>>,
    pub(super) plan_push_count: u64,
    pub fork_requests: Vec<PendingForkRequest>,
    pub fork_decisions: HashMap<String, Vec<ForkDecisionRecord>>,
    pub fork_start_error: Option<String>,
    pub(super) pending_wake_conversations: HashSet<String>,
    pub(super) pending_fork_starts: Vec<PendingForkStart>,
    pub(super) fork_starts_attempted: HashSet<String>,
    /// Handoff continuations, child → source. When the source is still the
    /// conversation on screen once the child is loaded, the page follows it there.
    pub(super) handoff_jumps: HashMap<String, String>,
    pub resumable_runs_adopted: bool,
    pub(super) adoption_started: bool,
    pub(super) adoptions_in_flight: HashSet<String>,
    pub(super) streams: HashMap<String, StreamBuffer>,
    /// Conversations whose run finished while another one was open. Their mark turns blue until they are opened;
    /// this is the renderer's own notice, so a restart forgets it.
    pub unseen_completions: HashSet<String>,
    pub(super) running_conversation_ids: HashSet<String>,
    /// Explanations by tool card id, plus by `<conversation>\0<provider call id>` for a call still running.
    pub(super) explanations: HashMap<String, String>,
    pub(super) explanation_errors: HashMap<String, String>,
    pub(super) explanations_loaded: HashSet<String>,
    pub(super) explanation_version: u64,
    pub timeline_history: TimelineHistory,
    pub(super) view_cache: RunViewCache,
    pub(super) reconcile_posted: bool,
    /// When a conversation's automatic wake or dispatch may try again after its flush failed. Upstream's effects only
    /// rerun when state changes, and a failed flush changes none; the per-frame reconcile needs telling.
    pub(super) reconcile_retry_after: HashMap<String, std::time::Instant>,
    pub signals: super::seams::RunSignals,
    pub(super) timeline_notice: Option<super::timeline_edits::TimelineNotice>,
    pub(super) timeline_notice_timer: Option<TimerId>,
    /// `navigatedToolPromptIdsRef`: cards already taken to where they are answered, or that need no taking.
    pub(super) navigated_tool_prompt_ids: HashSet<String>,
}

impl App {
    /// The run state, set up on first use.
    pub fn runs(&mut self) -> &mut RunsState {
        self.init_runs();
        self.local::<RunsState>()
    }

    /// `updateConversationTurns`: every writer funnels through here, so this is where a round that produced nothing
    /// stops being a record. Sweeping it centrally is what keeps the next run from continuing a round the user never
    /// saw and inheriting its elapsed time and token counters. `settled` writes the record now, as a turn ending is
    /// not something a quit should lose; a usage tick waits for the debounce.
    pub(super) fn update_turns_of(&mut self, conversation_id: &str, settled: bool, f: impl FnOnce(&mut Vec<ConversationTurn>)) {
        let runs = self.local::<RunsState>();
        let turns = runs.turns.entry(conversation_id.to_owned()).or_default();
        f(turns);
        drop_empty_turns(turns);
        runs.turns_revision += 1;
        self.schedule_turns_save(settled);
    }

    fn schedule_turns_save(&mut self, now: bool) {
        if now {
            if let Some(timer) = self.local::<RunsState>().turns_save.take() {
                self.clear_timer(timer);
            }
            return self.save_conversation_turns();
        }
        // Not a debounce: usage can tick for as long as a run lasts, and the record must not wait for it to stop.
        if self.local::<RunsState>().turns_save.is_some() {
            return;
        }
        let timer = self.set_timer(std::time::Duration::from_millis(500), |app| {
            app.local::<RunsState>().turns_save = None;
            app.save_conversation_turns();
        });
        self.local::<RunsState>().turns_save = Some(timer);
    }

    /// `saveConversationTurns`: presentation metadata must never make the conversation itself unsavable, so a failed
    /// write only costs the metadata. Quitting calls this before `flush_ui_store`, as a pending tick wouldn't get to run.
    pub fn save_conversation_turns(&mut self) {
        let turns = std::mem::take(&mut self.local::<RunsState>().turns);
        let text = serde_json::to_string(&turns);
        self.local::<RunsState>().turns = turns;
        if let Ok(text) = text {
            self.ui_store_update(|store| store.set(CONVERSATION_TURNS_STORAGE_KEY, text));
        }
    }

    pub(super) fn load_turns(&mut self) {
        let raw = self.ui_store().get(CONVERSATION_TURNS_STORAGE_KEY).map(str::to_owned);
        self.local::<RunsState>().turns = load_conversation_turns(raw.as_deref(), now_ms());
    }
}
