//! What a background task streams once the run that started it has settled.
//!
//! A child started by `agent_spawn` (or a `workflow` call) outlives the run that started it. While that run is live, the
//! child's events ride the run's own stream and fold into the run's state. Once the run has settled, the host pushes them
//! as `taskStreamEvent` instead, live only, and they land here: a display-only overlay per conversation that holds, for
//! each persisted card being updated, the child's live state as the same reducers a run uses fold it.
//!
//! The overlay never touches the document. A tool card carries the host's attestation, and a card edited here and later
//! written back would be quarantined; the timeline draws the overlay's copy over the persisted card instead, in its place.
//!
//! Lifecycle: an overlay exists only while its conversation has no run. A run's own stream owns its tasks' events, so a
//! pushed event for a conversation with a run is dropped, and starting (or adopting) a run drops the overlay. A card leaves
//! the overlay once its persisted counterpart is gone or is no longer the card it was seeded from: the host rewrote it,
//! which is how a backfill (or any reload) reaches the renderer. A terminal status settles the card's live transcript, and
//! the card stays on screen like that until the persisted card catches up. Deleting the conversation forgets its overlay.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::model_stream::{
    apply_nested_subagent_event, apply_subagent_channel_delta, push_workflow_progress, LiveContext, ModelStreamCoalescer, StreamingToolPhase, SubagentLiveState, SubagentLiveStatus,
    MODEL_STREAM_COMMIT_INTERVAL_MS, SUBAGENT_LIVE_CONTEXT_LIMIT,
};
use super::state::RunsState;
use super::types::WorkflowProgressEntry;
use super::view::RenderedContext;
use crate::model::{ContextItem, ModelStreamEvent, SubagentRunRecord, SubagentRunStatus};
use crate::timers::TimerId;
use crate::App;

/// Shared by every overlay, so an overlay dropped and started again never repeats a revision a view cached.
static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

/// The latest updates a seeded card keeps, as the channel reducer bounds them.
const SEEDED_UPDATE_LIMIT: usize = 50;

/// What a conversation's background tasks streamed since its last run, as of the latest commit.
#[derive(Clone, Debug)]
pub struct TaskOverlayView {
    /// Changes with every commit, and never repeats: a view that caches layout keys it on this.
    pub revision: u64,
    /// One per persisted card being updated: that card unchanged, drawn as a running call's row is, with its child's live
    /// state. Each replaces the persisted context with the same id, in its place.
    pub contexts: Vec<LiveContext>,
    /// Progress ledger rows per `workflow` call, as [`super::RunView::workflow_progress_by_call`] holds them.
    pub workflow_progress_by_call: Arc<BTreeMap<String, Vec<WorkflowProgressEntry>>>,
    /// Live run id per `workflow` call.
    pub workflow_run_id_by_call: BTreeMap<String, String>,
}

impl TaskOverlayView {
    /// The overlay's copy of a persisted context, if it has one.
    pub fn get(&self, id: &str) -> Option<&LiveContext> {
        self.contexts.iter().find(|context| context.id() == id)
    }

    /// The persisted contexts with each overlaid card in its own place: nothing moves, nothing is added.
    pub fn rendered_contexts<'a>(&'a self, persisted: &'a [ContextItem]) -> impl Iterator<Item = RenderedContext<'a>> + 'a {
        persisted.iter().map(move |item| self.get(item.id()).map_or(RenderedContext::Persisted(item), RenderedContext::Live))
    }
}

/// The fields of a persisted card that change when the host rewrites it.
#[derive(Clone, Debug, PartialEq)]
struct CardStamp {
    attestation: String,
    success: bool,
    output_len: usize,
    record: Option<(SubagentRunStatus, usize, usize)>,
}

impl CardStamp {
    fn of(item: &ContextItem) -> Option<Self> {
        match item {
            ContextItem::Tool { attestation, result, subagent, .. } => {
                Some(Self { attestation: attestation.clone(), success: result.success, output_len: result.output.len(), record: subagent.as_ref().map(record_stamp) })
            }
            _ => None,
        }
    }

    fn matches(&self, item: &ContextItem) -> bool {
        match item {
            ContextItem::Tool { attestation, result, subagent, .. } => {
                *attestation == self.attestation && result.success == self.success && result.output.len() == self.output_len && subagent.as_ref().map(record_stamp) == self.record
            }
            _ => false,
        }
    }
}

fn record_stamp(record: &SubagentRunRecord) -> (SubagentRunStatus, usize, usize) {
    (record.status, record.contexts.len(), record.updates.len())
}

/// One persisted card the overlay updates.
#[derive(Debug)]
struct OverlayCard {
    /// The persisted card's id, which the timeline merges by.
    context_id: String,
    /// The provider call id the host's events address the card by.
    call_id: String,
    /// Where the card was last found among the conversation's contexts.
    position: usize,
    seed: CardStamp,
    live: Arc<SubagentLiveState>,
}

/// One conversation's overlay.
#[derive(Debug, Default)]
pub(super) struct TaskOverlay {
    /// Events waiting for the next commit, which comes at the run stream's own cadence.
    pending: ModelStreamCoalescer,
    timer: Option<TimerId>,
    cards: Vec<OverlayCard>,
    workflow_progress_by_call: Arc<BTreeMap<String, Vec<WorkflowProgressEntry>>>,
    workflow_run_id_by_call: BTreeMap<String, String>,
    revision: u64,
    view: Option<Arc<TaskOverlayView>>,
}

fn is_task_tool(name: &str) -> bool {
    matches!(name, "subagent" | "agent_spawn" | "agent_send" | "send_message" | "followup_task" | "workflow" | "workflow_step")
}

/// The persisted card a call id names: the latest card with that provider call id, one that can own a task first, since a
/// provider that synthesizes its call ids can repeat one across runs.
fn find_call(persisted: &[ContextItem], call_id: &str) -> Option<usize> {
    let mut fallback = None;
    for (position, item) in persisted.iter().enumerate().rev() {
        if let ContextItem::Tool { provider_call_id: Some(provider_call_id), tool_name, .. } = item
            && provider_call_id == call_id
        {
            if is_task_tool(tool_name) {
                return Some(position);
            }
            fallback.get_or_insert(position);
        }
    }
    fallback
}

fn find_card(persisted: &[ContextItem], context_id: &str, hint: usize) -> Option<usize> {
    if persisted.get(hint).is_some_and(|item| item.id() == context_id) {
        return Some(hint);
    }
    persisted.iter().position(|item| item.id() == context_id)
}

/// A card's live state the first time an event reaches it: what its record already shows, so nothing visible goes away,
/// and running, which a child that streams evidently is.
fn seeded_live(record: Option<&SubagentRunRecord>) -> SubagentLiveState {
    let mut live = SubagentLiveState { status: Some(SubagentLiveStatus::Running), ..SubagentLiveState::default() };
    if let Some(record) = record {
        let skip = record.contexts.len().saturating_sub(SUBAGENT_LIVE_CONTEXT_LIMIT);
        live.contexts = record.contexts[skip..].iter().cloned().map(LiveContext::settled).collect();
        let skip = record.updates.len().saturating_sub(SEEDED_UPDATE_LIMIT);
        live.updates = record.updates[skip..].to_vec();
    }
    live
}

impl TaskOverlay {
    fn is_spent(&self) -> bool {
        self.cards.is_empty() && self.pending.is_empty()
    }

    /// Drops every card whose persisted counterpart is gone or is no longer the card it was seeded from. `true` when one went.
    fn validate(&mut self, persisted: &[ContextItem]) -> bool {
        let mut gone: Vec<String> = Vec::new();
        self.cards.retain_mut(|card| match find_card(persisted, &card.context_id, card.position) {
            Some(position) if card.seed.matches(&persisted[position]) => {
                card.position = position;
                true
            }
            _ => {
                gone.push(card.call_id.clone());
                false
            }
        });
        if gone.is_empty() {
            return false;
        }
        if gone.iter().any(|call_id| self.workflow_progress_by_call.contains_key(call_id)) {
            let progress = Arc::make_mut(&mut self.workflow_progress_by_call);
            for call_id in &gone {
                progress.remove(call_id);
            }
        }
        for call_id in &gone {
            self.workflow_run_id_by_call.remove(call_id);
        }
        true
    }

    /// The card a call id addresses, seeded from its persisted counterpart the first time.
    fn card_for(&mut self, persisted: &[ContextItem], call_id: &str) -> Option<&mut OverlayCard> {
        if let Some(index) = self.cards.iter().position(|card| card.call_id == call_id) {
            return Some(&mut self.cards[index]);
        }
        let position = find_call(persisted, call_id)?;
        let item = &persisted[position];
        let seed = CardStamp::of(item)?;
        let record = match item {
            ContextItem::Tool { subagent, .. } => subagent.as_ref(),
            _ => None,
        };
        self.cards.push(OverlayCard { context_id: item.id().to_owned(), call_id: call_id.to_owned(), position, seed, live: Arc::new(seeded_live(record)) });
        self.cards.last_mut()
    }

    /// Folds one event with the run stream's reducers. `false` when it addressed nothing this conversation shows.
    fn apply(&mut self, persisted: &[ContextItem], event: &ModelStreamEvent, at: &str) -> bool {
        match event {
            ModelStreamEvent::SubagentEvent { call_id, event: nested, .. } => {
                let Some(card) = self.card_for(persisted, call_id) else { return false };
                apply_nested_subagent_event(Arc::make_mut(&mut card.live), &card.context_id, nested, at);
                true
            }
            ModelStreamEvent::SubagentDelta { call_id, channel, delta, .. } => {
                let Some(card) = self.card_for(persisted, call_id) else { return false };
                apply_subagent_channel_delta(Arc::make_mut(&mut card.live), &card.context_id, *channel, delta, at);
                true
            }
            ModelStreamEvent::WorkflowProgress { call_id, run_id, entry, .. } => {
                if self.card_for(persisted, call_id).is_none() {
                    return false;
                }
                // The host's row and the renderer's interface are one JSON shape.
                let Ok(entry) = serde_json::to_value(entry).and_then(serde_json::from_value::<WorkflowProgressEntry>) else { return false };
                push_workflow_progress(&mut self.workflow_progress_by_call, &mut self.workflow_run_id_by_call, call_id, run_id, entry);
                true
            }
            _ => false,
        }
    }

    fn changed(&mut self) {
        self.revision = NEXT_REVISION.fetch_add(1, Ordering::Relaxed);
        self.view = None;
    }

    /// The view of the cards still in the overlay; [`Self::validate`] has just placed each one.
    fn view(&mut self, persisted: &[ContextItem]) -> Arc<TaskOverlayView> {
        if let Some(view) = &self.view {
            return view.clone();
        }
        let contexts = self
            .cards
            .iter()
            .filter_map(|card| {
                let item = persisted.get(card.position)?.clone();
                // A spawn's call completes when the host accepts it; what is still running is its child, and the live state says so.
                Some(LiveContext { item, streaming: Some(true), stream_status: Some(StreamingToolPhase::Completed), live: Some(card.live.clone()), started_at: None })
            })
            .collect();
        let view = Arc::new(TaskOverlayView {
            revision: self.revision,
            contexts,
            workflow_progress_by_call: self.workflow_progress_by_call.clone(),
            workflow_run_id_by_call: self.workflow_run_id_by_call.clone(),
        });
        self.view = Some(view.clone());
        view
    }
}

fn runs(app: &mut App) -> &mut RunsState {
    app.local::<RunsState>()
}

fn has_run(app: &mut App, conversation_id: &str) -> bool {
    runs(app).controller.run(conversation_id).is_some()
}

/// What the overlay folds; anything else a task could push belongs to a run.
fn is_task_stream_event(event: &ModelStreamEvent) -> bool {
    matches!(event, ModelStreamEvent::SubagentEvent { .. } | ModelStreamEvent::SubagentDelta { .. } | ModelStreamEvent::WorkflowProgress { .. })
}

/// A `taskStreamEvent`: buffered, then folded at the run stream's commit cadence, however fast a child streams.
pub(super) fn receive_task_stream_event(app: &mut App, conversation_id: &str, event: ModelStreamEvent) {
    if !is_task_stream_event(&event) {
        return;
    }
    // A run's own stream carries its tasks' events; this is for the time between runs.
    if has_run(app, conversation_id) {
        app.forget_task_overlay(conversation_id);
        return;
    }
    let overlay = runs(app).task_overlays.entry(conversation_id.to_owned()).or_default();
    if !overlay.pending.push(event) {
        return;
    }
    let id = conversation_id.to_owned();
    let timer = app.set_timer(Duration::from_millis(MODEL_STREAM_COMMIT_INTERVAL_MS), move |app| {
        if let Some(overlay) = runs(app).task_overlays.get_mut(&id) {
            overlay.timer = None;
        }
        commit_task_overlay(app, &id);
    });
    if let Some(overlay) = runs(app).task_overlays.get_mut(conversation_id) {
        overlay.timer = Some(timer);
    }
}

/// Folds what arrived since the last commit, against the conversation's contexts as they are now.
fn commit_task_overlay(app: &mut App, conversation_id: &str) {
    if has_run(app, conversation_id) {
        return app.forget_task_overlay(conversation_id);
    }
    let Some(mut overlay) = runs(app).task_overlays.remove(conversation_id) else { return };
    if let Some(timer) = overlay.timer.take() {
        app.clear_timer(timer);
    }
    let batch = overlay.pending.take();
    let persisted: &[ContextItem] = app.conversation(conversation_id).map_or(&[], |conversation| conversation.contexts.as_slice());
    let mut changed = overlay.validate(persisted);
    for entry in &batch {
        changed |= overlay.apply(persisted, &entry.event, &entry.at);
    }
    if changed {
        overlay.changed();
    }
    if !overlay.is_spent() {
        runs(app).task_overlays.insert(conversation_id.to_owned(), overlay);
    }
    if changed {
        crate::prelude::repaint();
    }
}

impl App {
    /// What this conversation's background tasks streamed since its last run settled, merged by the timeline and the
    /// subagent views over the persisted cards. `None` while the conversation has a run, whose own stream carries its
    /// tasks: [`App::run_view`] is what to read then. Cheap: built at most once per commit.
    pub fn task_overlay(&mut self, conversation_id: &str) -> Option<Arc<TaskOverlayView>> {
        if !self.runs().task_overlays.contains_key(conversation_id) {
            return None;
        }
        if has_run(self, conversation_id) {
            self.forget_task_overlay(conversation_id);
            return None;
        }
        let mut overlay = runs(self).task_overlays.remove(conversation_id)?;
        let persisted: &[ContextItem] = self.conversation(conversation_id).map_or(&[], |conversation| conversation.contexts.as_slice());
        if overlay.validate(persisted) {
            overlay.changed();
        }
        let view = (!overlay.cards.is_empty()).then(|| overlay.view(persisted));
        if overlay.is_spent() {
            if let Some(timer) = overlay.timer.take() {
                self.clear_timer(timer);
            }
        } else {
            runs(self).task_overlays.insert(conversation_id.to_owned(), overlay);
        }
        view
    }

    /// Drops a conversation's overlay: a run of its own has started, or the conversation is gone.
    pub(crate) fn forget_task_overlay(&mut self, conversation_id: &str) {
        if let Some(overlay) = runs(self).task_overlays.remove(conversation_id) {
            if let Some(timer) = overlay.timer {
                self.clear_timer(timer);
            }
            crate::prelude::repaint();
        }
    }
}
