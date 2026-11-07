//! What the timeline reads per frame (`StreamedConversationView.tsx`, and App's run-derived values).
//!
//! The projection of a run is built at most once per commit and handed out behind an `Arc`, so reading it never copies
//! the conversation: the persisted contexts stay where they are, and [`RunView::rendered_contexts`] walks them with
//! the live ones after, by reference.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use super::context_tokens::{estimate_wire_tokens, live_context_tokens, wire_view, LiveContextTokens};
use super::conversation_turns::{materialize_run_turn_contexts_ids, ConversationTurn};
use super::model_stream::{LiveContext, ModelRunRetryState, ModelRunState, StreamingToolPhase, SubagentLiveState};
use super::run_contexts::{browser_automation_tool_for_run, contexts_from_model_run, live_reasoning_from_model_run, LiveReasoningView};
use super::seams::{find_conversation, model_choice, workspace_of};
use super::state::RunsState;
use super::task_overlay::TaskOverlayView;
use super::types::{ContextUsage, WorkflowProgressEntry};
use crate::model::{ContextItem, ProviderFamily};
use crate::App;

/// One conversation's live run, as of its latest commit (one a second).
#[derive(Clone, Debug)]
pub struct RunView {
    pub request_id: String,
    pub provider_name: String,
    pub model_name: String,
    pub workspace_id: String,
    /// When this run was registered.
    pub started_at: String,
    /// Changes with each commit, the run's or its conversation's task overlay's: a view that caches layout keys it on this.
    pub revision: u64,
    /// The first [`Self::in_place`] are the task overlay's cards: tasks earlier runs started, still streaming, each drawn
    /// in the place of the persisted card it stands for. The rest are the run's streaming projection, which replaces
    /// same-id persisted contexts and follows them.
    pub contexts: Vec<LiveContext>,
    /// How many of [`Self::contexts`] are the overlay's, drawn in place.
    pub in_place: usize,
    streaming_ids: HashSet<String>,
    /// The overlay's cards, by id, as indexes into `contexts`.
    over: HashMap<String, usize>,
    /// `thinking`: derived from run state, not from the contexts: encrypted reasoning with no summary is deliberately
    /// absent from them while it streams.
    pub thinking: Option<LiveReasoningView>,
    /// `retryNotice`.
    pub retry: Option<ModelRunRetryState>,
    /// Progress rows per `workflow` card, by the card's context id: the run's own, and the overlay's for workflows earlier
    /// runs started. Keyed on the progress entries, not on the run: the same `Arc` until a workflow reports.
    pub workflow_progress_by_call: Arc<BTreeMap<String, Vec<WorkflowProgressEntry>>>,
    /// Live run id per `workflow` card, by context id — the address Skip/Retry commands carry.
    pub workflow_run_id_by_call: BTreeMap<String, String>,
    /// Most recent tool driving browser automation.
    pub browser_automation_tool: Option<String>,
}

/// The view by conversation, with the run's and the overlay's revisions it was built at.
pub(super) type RunViewCache = HashMap<String, (u64, u64, Arc<RunView>)>;

impl RunView {
    fn of(run: &ModelRunState, revision: u64, overlay: Option<&TaskOverlayView>) -> Self {
        let own = contexts_from_model_run(run, true);
        let streaming_ids: HashSet<String> = own.iter().map(|context| context.id().to_owned()).collect();
        let mut workflow_progress_by_call = run.workflow_progress_by_call.clone();
        let mut workflow_run_id_by_call = run.workflow_run_id_by_call.clone();
        // A card the run streams itself is the run's to draw.
        let mut contexts: Vec<LiveContext> = overlay.map_or_else(Vec::new, |overlay| overlay.contexts.iter().filter(|context| !streaming_ids.contains(context.id())).cloned().collect());
        let in_place = contexts.len();
        if let Some(overlay) = overlay.filter(|overlay| !overlay.workflow_progress_by_call.is_empty()) {
            let merged = Arc::make_mut(&mut workflow_progress_by_call);
            for (card, entries) in overlay.workflow_progress_by_call.iter() {
                merged.entry(card.clone()).or_insert_with(|| entries.clone());
            }
            for (card, run_id) in &overlay.workflow_run_id_by_call {
                workflow_run_id_by_call.entry(card.clone()).or_insert_with(|| run_id.clone());
            }
        }
        let over = contexts.iter().enumerate().map(|(at, context)| (context.id().to_owned(), at)).collect();
        contexts.extend(own);
        Self {
            request_id: run.request_id.clone(),
            provider_name: run.provider_name.clone(),
            model_name: run.model_name.clone(),
            workspace_id: run.workspace_id.clone(),
            started_at: run.started_at.clone(),
            revision,
            contexts,
            in_place,
            streaming_ids,
            over,
            thinking: live_reasoning_from_model_run(Some(run)),
            retry: run.retry.clone(),
            workflow_progress_by_call,
            workflow_run_id_by_call,
            browser_automation_tool: browser_automation_tool_for_run(Some(run)),
        }
    }

    /// `mergeContextsWithStreamingRun`: the persisted contexts plus the projection. Streaming contexts win for
    /// duplicate ids because they contain the latest delta and streaming state. An overlay card stands in the place of
    /// its persisted card.
    pub fn rendered_contexts<'a>(&'a self, persisted: &'a [ContextItem]) -> impl Iterator<Item = RenderedContext<'a>> + 'a {
        persisted
            .iter()
            .filter(|context| !self.streaming_ids.contains(context.id()))
            .map(|context| match self.over.get(context.id()) {
                Some(&at) => RenderedContext::Live(&self.contexts[at]),
                None => RenderedContext::Persisted(context),
            })
            .chain(self.contexts[self.in_place..].iter().map(RenderedContext::Live))
    }

    /// `renderedTurns`: `materializeRunTurnContexts` over the rendered timeline. A turn's context ids are what group
    /// the timeline under its header, so this runs when the timeline itself is rebuilt, not per frame.
    pub fn rendered_turns(&self, turns: &[ConversationTurn], persisted: &[ContextItem]) -> Vec<ConversationTurn> {
        let ids: Vec<&str> = self.rendered_contexts(persisted).map(|context| context.id()).collect();
        let mut turns = turns.to_vec();
        materialize_run_turn_contexts_ids(&mut turns, &ids, &self.request_id);
        turns
    }
}

/// A row of the timeline: as persisted, or live.
#[derive(Clone, Copy, Debug)]
pub enum RenderedContext<'a> {
    Persisted(&'a ContextItem),
    Live(&'a LiveContext),
}

impl<'a> RenderedContext<'a> {
    pub fn item(self) -> &'a ContextItem {
        match self {
            Self::Persisted(item) => item,
            Self::Live(context) => &context.item,
        }
    }

    pub fn id(self) -> &'a str {
        self.item().id()
    }

    /// Whether the card is still growing.
    pub fn streaming(self) -> bool {
        matches!(self, Self::Live(context) if context.is_streaming())
    }

    pub fn stream_status(self) -> Option<StreamingToolPhase> {
        match self {
            Self::Live(context) => context.stream_status,
            Self::Persisted(_) => None,
        }
    }

    /// The child's live transcript, on a running agent call.
    pub fn live(self) -> Option<&'a Arc<SubagentLiveState>> {
        match self {
            Self::Live(context) => context.live.as_ref(),
            Self::Persisted(_) => None,
        }
    }

    /// When live reasoning opened, for the card's clock.
    pub fn started_at(self) -> Option<&'a str> {
        match self {
            Self::Live(context) => context.started_at.as_deref(),
            Self::Persisted(_) => None,
        }
    }
}

/// `ConversationStatus` of the sidebar's mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationStatus {
    Idle,
    Running,
    Blocked,
    Completed,
}

impl App {
    /// `run_view(conversation)`: live contexts and turn state of a conversation's run, if it has one. Cheap: built once per commit.
    pub fn run_view(&mut self, conversation_id: &str) -> Option<Arc<RunView>> {
        if self.runs().controller.run(conversation_id).is_none() {
            self.runs().view_cache.remove(conversation_id);
            return None;
        }
        // Tasks earlier runs started go on streaming into the conversation's overlay; the run's view draws it.
        let overlay = self.task_overlay_view(conversation_id);
        let overlay_revision = overlay.as_ref().map_or(0, |overlay| overlay.revision);
        let state = self.runs();
        let run_revision = state.controller.revision_of(conversation_id);
        let run = state.controller.run(conversation_id)?;
        if let Some((cached_run, cached_overlay, view)) = state.view_cache.get(conversation_id)
            && *cached_run == run_revision
            && *cached_overlay == overlay_revision
        {
            return Some(view.clone());
        }
        // Overlay revisions are unique app-wide and the run's count from 0, so the pair reads as one number that never
        // repeats; with no overlay it is the run's own.
        let revision = run_revision ^ overlay_revision.rotate_left(32);
        let view = Arc::new(RunView::of(run, revision, overlay.as_deref()));
        state.view_cache.insert(conversation_id.to_owned(), (run_revision, overlay_revision, view.clone()));
        Some(view)
    }

    /// `conversationTurns[conversation]`: the turn records as stored.
    pub fn conversation_turns(&mut self, conversation_id: &str) -> Vec<ConversationTurn> {
        self.runs().turns.get(conversation_id).cloned().unwrap_or_default()
    }

    /// Changes whenever any turn record does.
    pub fn conversation_turns_revision(&mut self) -> u64 {
        self.runs().turns_revision
    }

    /// `renderedTurns`: the turns with the live request's spans materialized over the rendered timeline.
    pub fn rendered_turns(&mut self, conversation_id: &str) -> Vec<ConversationTurn> {
        let turns = self.conversation_turns(conversation_id);
        let Some(view) = self.run_view(conversation_id) else { return turns };
        let Some(workspace) = workspace_of(&self.doc, conversation_id).map(|workspace| workspace.id.clone()) else { return turns };
        let persisted = find_conversation(&self.doc, &workspace, conversation_id).map_or(&[][..], |conversation| conversation.contexts.as_slice());
        view.rendered_turns(&turns, persisted)
    }

    /// `liveContextTokens` for this conversation's run; `None` with no run.
    pub fn live_context_tokens(&mut self, conversation_id: &str, fallback_tokens: u64) -> Option<LiveContextTokens> {
        self.runs().controller.run(conversation_id).map(|run| live_context_tokens(run, fallback_tokens))
    }

    /// `activeContextUsage` for any conversation: what the usage ring shows. The run's live figure, else the
    /// authoritative usage reported at the prior turn boundary when it was measured against the current provider and
    /// protocol, else the local estimate.
    pub fn context_usage_view(&mut self, conversation_id: &str) -> ContextUsage {
        self.init_runs();
        let target = match model_choice(&self.doc) {
            (Some(provider), Some(model)) if provider.enabled => Some((provider, model)),
            _ => None,
        };
        let target_id: Option<(String, ProviderFamily)> = target.map(|(provider, _)| (provider.id.clone(), provider.family));
        let contexts: &[ContextItem] = workspace_of(&self.doc, conversation_id)
            .and_then(|workspace| workspace.conversations.iter().find(|conversation| conversation.id == conversation_id))
            .map_or(&[], |conversation| conversation.contexts.as_slice());
        let estimate = estimate_wire_tokens(wire_view(contexts, target));
        let provider_id = target_id.as_ref().map(|(id, _)| id.clone());
        let family = target_id.as_ref().map(|(_, family)| *family);
        let state = self.local::<RunsState>();
        let projected = state.context_usage.get(conversation_id).filter(|usage| usage.provider_id == provider_id && usage.family == family).cloned();
        // The larger of local estimation and prior authoritative usage is a stable lower bound; contexts can only grow during a turn.
        let fallback = estimate.max(projected.as_ref().map_or(0, |usage| usage.tokens));
        if let Some(run) = state.controller.run(conversation_id) {
            let live = live_context_tokens(run, fallback);
            return ContextUsage { tokens: live.tokens, estimated: live.estimated, unprojectable: false, provider_id, family };
        }
        projected.unwrap_or(ContextUsage { tokens: fallback, estimated: true, unprojectable: false, provider_id, family })
    }

    /// `conversationStatus`. `other_activity` is what the shell and terminal stores know: an unfinished background
    /// command or a busy terminal in this conversation.
    pub fn conversation_status(&mut self, conversation_id: &str, other_activity: bool) -> ConversationStatus {
        let blocked = self.conversation_is_blocked(conversation_id);
        let state = self.runs();
        if blocked {
            ConversationStatus::Blocked
        } else if state.controller.summaries().contains_key(conversation_id) || other_activity {
            ConversationStatus::Running
        } else if state.unseen_completions.contains(conversation_id) {
            ConversationStatus::Completed
        } else {
            ConversationStatus::Idle
        }
    }

    /// `blockedConversationIds`: waiting on the user, with an approval card, a question card or a fork request.
    pub fn conversation_is_blocked(&mut self, conversation_id: &str) -> bool {
        let state = self.runs();
        state.tool_prompts.get(conversation_id).is_some_and(|prompts| !prompts.is_empty()) || state.fork_requests.iter().any(|request| request.source_conversation_id == conversation_id)
    }
}
