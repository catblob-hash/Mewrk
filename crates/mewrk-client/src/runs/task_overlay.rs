//! What a background task streams once the run that started it has settled.
//!
//! A child started by `agent_spawn` (or a `workflow` call) outlives the run that started it. While that run is live, the
//! child's events ride the run's own stream and fold into the run's state. After it, they land here: a display-only
//! overlay per conversation that holds, for each persisted card being updated, the child's live state as the same
//! reducers a run uses fold it. They come two ways. With no unsettled run the host pushes them as `taskStreamEvent`, live
//! only. While a later run is live they ride that run's stream, whose reducer has no row for a call it did not make and
//! hands them on (`ModelStreamEffect::ForeignTaskEvent`).
//!
//! The overlay never touches the document. A tool card carries the host's attestation, and a card edited here and later
//! written back would be quarantined; the timeline draws the overlay's copy over the persisted card instead, in its place.
//! With a run live, the run's view carries the overlay's cards ([`super::RunView`]), drawn in place beside the run's own
//! projection.
//!
//! Lifecycle: a card leaves the overlay once its persisted counterpart is gone or is no longer the card it was seeded
//! from: the host rewrote it, which is how a backfill (or any reload) reaches the renderer. A terminal status settles the
//! card's live transcript, and the card stays on screen like that until the persisted card catches up. Starting a run
//! leaves the overlay be; deleting the conversation forgets it.
//!
//! The settle seam, where nothing a child streams may fall through:
//!
//! - A run that settles hands its still-running tasks over ([`TaskHandoff`]): each card starts from the child's live state
//!   as the run's stream left it, not from its persisted record, which lacks the message the child is in the middle of.
//! - The host settles a run before the renderer does. A task event pushed in between (the run's stream delivered
//!   `run_concluded`, the renderer has not settled the run yet) is held, and folded once the run has handed over. Both
//!   channels post to one queue in the order the host sent, so an event pushed while the renderer's run has not reached
//!   `run_concluded` was pushed before that run began on the host: it belongs to a task an earlier run started, and is
//!   folded as usual.
//! - A running child's checkpoint rewrites its card (the host saves the rounds the child settled, and re-attests it), and
//!   the reload that settles a run brings it. The card is rebased onto the new copy rather than started over from it: the
//!   record's rows, then the live rows of rounds it has not saved. Only a record with an outcome, or a rewritten result,
//!   takes the card back.
//!
//! Every map here is keyed by the card's context id, the id the timeline and the subagent views know a card by. The host's
//! events name a card by its provider call id, which is resolved to the card once, where the event lands.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use super::model_stream::{
    apply_nested_subagent_event, apply_subagent_channel_delta, push_workflow_progress, LiveContext, ModelRunState, ModelStreamCoalescer, StreamingToolPhase, SubagentLiveState,
    SubagentLiveStatus, SUBAGENT_LIVE_CONTEXT_LIMIT,
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
    /// Progress ledger rows per `workflow` card, by its context id, as [`super::RunView::workflow_progress_by_call`] holds
    /// them.
    pub workflow_progress_by_call: Arc<BTreeMap<String, Vec<WorkflowProgressEntry>>>,
    /// Live run id per `workflow` card, by its context id.
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

    /// Whether `item` is this card as a running child's checkpoint leaves it: the call's result untouched, and a record in
    /// the state the host saves a running child in. A record with an outcome, or a rewritten result, is the host's
    /// account of how the task went, and the card is the authority again.
    fn is_checkpointed_by(&self, item: &ContextItem) -> bool {
        matches!(item, ContextItem::Tool { result, subagent: Some(record), .. }
            if result.success == self.success && result.output.len() == self.output_len && record.status == SubagentRunStatus::Interrupted)
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
    /// The provider call id the host's events address the card by, with the round below.
    call_id: String,
    /// The parent round the card's call ran in, which the host's events carry beside the call id. `None` for a card that
    /// records none.
    round: Option<usize>,
    /// Where the card was last found among the conversation's contexts.
    position: usize,
    seed: CardStamp,
    live: Arc<SubagentLiveState>,
}

impl OverlayCard {
    /// Whether the child is still streaming as far as this card knows: it has reported no outcome.
    fn is_running(&self) -> bool {
        matches!(self.live.status, None | Some(SubagentLiveStatus::Running))
    }
}

/// What a run leaves the tasks that outlive it, taken as it settles: for each task card whose child was still running,
/// the child's live state as the run's stream left it, and its workflow's progress rows, by the card's context id.
///
/// Without it the overlay would start each card over from its persisted record the next time the child streams, and the
/// record lacks whatever the child had streamed and not yet saved — the message it is in the middle of above all.
#[derive(Debug, Default)]
pub(super) struct TaskHandoff {
    cards: Vec<HandedOffCard>,
    workflow_progress: BTreeMap<String, Vec<WorkflowProgressEntry>>,
    workflow_run_ids: BTreeMap<String, String>,
}

#[derive(Debug)]
struct HandedOffCard {
    context_id: String,
    call_id: String,
    live: Arc<SubagentLiveState>,
}

impl TaskHandoff {
    pub(super) fn of(run: &ModelRunState) -> Self {
        let mut handoff = Self::default();
        for tool in run.streamed_tools_by_round.values().flatten() {
            // A spawn's call completes when the host accepts it; a call still open when its run settled ended with it.
            if !is_task_tool(&tool.tool_name) || tool.stream_status != StreamingToolPhase::Completed {
                continue;
            }
            // A child that reported how it ended has nothing more to stream.
            if !matches!(tool.live.status, None | Some(SubagentLiveStatus::Running)) {
                continue;
            }
            let progress = run.workflow_progress_by_call.get(&tool.id);
            let showing = tool.live.status.is_some() || !tool.live.contexts.is_empty() || !tool.live.updates.is_empty() || progress.is_some();
            if !showing {
                continue;
            }
            if let Some(entries) = progress {
                handoff.workflow_progress.insert(tool.id.clone(), entries.clone());
            }
            if let Some(run_id) = run.workflow_run_id_by_call.get(&tool.id) {
                handoff.workflow_run_ids.insert(tool.id.clone(), run_id.clone());
            }
            handoff.cards.push(HandedOffCard { context_id: tool.id.clone(), call_id: tool.call_id.clone(), live: tool.live.clone() });
        }
        handoff
    }

    fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }
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

/// The persisted card an event's `(round, callId)` names: the latest card with that provider call id in that round, one
/// that can own a task first. A provider that synthesizes its call ids can repeat one across rounds and runs, and the
/// round is what tells such cards apart; a card that records no round is matched by the call id alone.
fn find_call(persisted: &[ContextItem], round: usize, call_id: &str) -> Option<usize> {
    let mut fallback = None;
    for (position, item) in persisted.iter().enumerate().rev() {
        if let ContextItem::Tool { provider_call_id: Some(provider_call_id), tool_name, round: card_round, .. } = item
            && provider_call_id == call_id
            && card_round.is_none_or(|card_round| card_round == round)
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

/// A running card's live state over a record the host saved since it was built: the record's rows, as a seed has them,
/// then the live rows the record does not hold yet.
///
/// Live rows carry the renderer's ids and the record's rows the host's, and the subagent views union a card's record and
/// live rows by id: a live row of a round the record saved would show twice. A child's rounds restart at 1 with each of
/// its turns while the record spans every turn, so what the record saved of the turn in flight is read from its last
/// user message on; a live row without a round (an activity note) is the renderer's own and stays.
fn rebased_live(record: &SubagentRunRecord, live: &SubagentLiveState) -> SubagentLiveState {
    let mut rebased = seeded_live(Some(record));
    let turn = record.contexts.iter().rposition(|context| matches!(context, ContextItem::User { .. })).map_or(0, |index| index + 1);
    let saved_through = record.contexts[turn..].iter().filter_map(ContextItem::round).max();
    let saved: HashSet<&str> = record.contexts.iter().map(ContextItem::id).collect();
    rebased.contexts.extend(
        live.contexts
            .iter()
            .filter(|row| !saved.contains(row.id()) && row.item.round().zip(saved_through).is_none_or(|(round, saved_through)| round > saved_through))
            .cloned(),
    );
    let noted: HashSet<&str> = record.updates.iter().map(|update| update.content.as_str()).collect();
    rebased.updates.extend(live.updates.iter().filter(|update| !noted.contains(update.content.as_str())).cloned());
    let excess = rebased.updates.len().saturating_sub(SEEDED_UPDATE_LIMIT);
    rebased.updates.drain(..excess);
    let kept: HashSet<&str> = rebased.contexts.iter().map(LiveContext::id).collect();
    rebased.tool_context_ids = live.tool_context_ids.iter().filter(|(_, id)| kept.contains(id.as_str())).map(|(call, id)| (call.clone(), id.clone())).collect();
    rebased.status = live.status;
    rebased.retry_round = live.retry_round;
    rebased.usage_by_round = live.usage_by_round.clone();
    rebased
}

impl TaskOverlay {
    fn is_spent(&self) -> bool {
        self.cards.is_empty() && self.pending.is_empty()
    }

    /// Drops every card whose persisted counterpart is gone or is no longer the card it was seeded from, and moves a
    /// running card onto its checkpointed copy. `true` when a card went or now draws a different persisted copy.
    fn validate(&mut self, persisted: &[ContextItem]) -> bool {
        let mut gone: Vec<String> = Vec::new();
        let mut restamped = false;
        self.cards.retain_mut(|card| match find_card(persisted, &card.context_id, card.position) {
            Some(position) if card.seed.matches(&persisted[position]) => {
                card.position = position;
                true
            }
            // The checkpoint holds the rounds the child settled, which the live state streamed already; what streamed
            // since, the round in flight above all, it does not. Starting over from it would lose that, and keeping the
            // live rows it saved would show them twice: the card is rebased onto it.
            Some(position) if card.is_running() && card.seed.is_checkpointed_by(&persisted[position]) => {
                let item = &persisted[position];
                if let ContextItem::Tool { subagent: Some(record), .. } = item {
                    card.live = Arc::new(rebased_live(record, &card.live));
                }
                if let Some(seed) = CardStamp::of(item) {
                    card.seed = seed;
                }
                card.position = position;
                restamped = true;
                true
            }
            _ => {
                gone.push(card.context_id.clone());
                false
            }
        });
        if gone.is_empty() {
            return restamped;
        }
        if gone.iter().any(|context_id| self.workflow_progress_by_call.contains_key(context_id)) {
            let progress = Arc::make_mut(&mut self.workflow_progress_by_call);
            for context_id in &gone {
                progress.remove(context_id);
            }
        }
        for context_id in &gone {
            self.workflow_run_id_by_call.remove(context_id);
        }
        true
    }

    /// Takes over what a settled run handed over, for each card the conversation still shows as its child running.
    /// `true` when a card was taken.
    fn adopt(&mut self, persisted: &[ContextItem], handoff: TaskHandoff) -> bool {
        let TaskHandoff { cards, mut workflow_progress, mut workflow_run_ids } = handoff;
        let mut adopted = false;
        for card in cards {
            let Some(position) = find_card(persisted, &card.context_id, 0) else { continue };
            let item = &persisted[position];
            // A record with an outcome is the host's account of how the child ended: the card is the authority, and
            // nothing live is left to carry on. A running child's record is a checkpoint.
            if let ContextItem::Tool { subagent: Some(record), .. } = item
                && record.status != SubagentRunStatus::Interrupted
            {
                continue;
            }
            let Some(seed) = CardStamp::of(item) else { continue };
            // During the run the card drew its live state alone; once a checkpoint record sits under it, the live rows
            // of the rounds it saved would show twice.
            let live = match item {
                ContextItem::Tool { subagent: Some(record), .. } => Arc::new(rebased_live(record, &card.live)),
                _ => card.live,
            };
            if let Some(entries) = workflow_progress.remove(&card.context_id) {
                Arc::make_mut(&mut self.workflow_progress_by_call).insert(card.context_id.clone(), entries);
            }
            if let Some(run_id) = workflow_run_ids.remove(&card.context_id) {
                self.workflow_run_id_by_call.insert(card.context_id.clone(), run_id);
            }
            self.cards.retain(|existing| existing.context_id != card.context_id);
            self.cards.push(OverlayCard { context_id: card.context_id, call_id: card.call_id, round: item.round(), position, seed, live });
            adopted = true;
        }
        adopted
    }

    /// The card an event's `(round, callId)` addresses, seeded from its persisted counterpart the first time.
    fn card_for(&mut self, persisted: &[ContextItem], round: usize, call_id: &str) -> Option<&mut OverlayCard> {
        if let Some(index) = self.cards.iter().position(|card| card.call_id == call_id && card.round.is_none_or(|card_round| card_round == round)) {
            return Some(&mut self.cards[index]);
        }
        let position = find_call(persisted, round, call_id)?;
        let item = &persisted[position];
        let seed = CardStamp::of(item)?;
        let record = match item {
            ContextItem::Tool { subagent, .. } => subagent.as_ref(),
            _ => None,
        };
        self.cards.push(OverlayCard { context_id: item.id().to_owned(), call_id: call_id.to_owned(), round: item.round(), position, seed, live: Arc::new(seeded_live(record)) });
        self.cards.last_mut()
    }

    /// Folds one event with the run stream's reducers. `false` when it addressed nothing this conversation shows.
    fn apply(&mut self, persisted: &[ContextItem], event: &ModelStreamEvent, at: &str) -> bool {
        match event {
            ModelStreamEvent::SubagentEvent { round, call_id, event: nested } => {
                let Some(card) = self.card_for(persisted, *round, call_id) else { return false };
                apply_nested_subagent_event(Arc::make_mut(&mut card.live), &card.context_id, nested, at);
                true
            }
            ModelStreamEvent::SubagentDelta { round, call_id, channel, delta } => {
                let Some(card) = self.card_for(persisted, *round, call_id) else { return false };
                apply_subagent_channel_delta(Arc::make_mut(&mut card.live), &card.context_id, *channel, delta, at);
                true
            }
            ModelStreamEvent::WorkflowProgress { round, call_id, run_id, entry } => {
                // Kept under the card's context id, which is what every reader looks the card up by.
                let Some(context_id) = self.card_for(persisted, *round, call_id).map(|card| card.context_id.clone()) else { return false };
                // The host's row and the renderer's interface are one JSON shape.
                let Ok(entry) = serde_json::to_value(entry).and_then(serde_json::from_value::<WorkflowProgressEntry>) else { return false };
                push_workflow_progress(&mut self.workflow_progress_by_call, &mut self.workflow_run_id_by_call, &context_id, run_id, entry);
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

/// Whether the conversation's run is one the host has settled and the renderer has not: its stream is over, and its tasks
/// are about to be handed over.
fn run_settled_on_host(app: &mut App, conversation_id: &str) -> bool {
    let state = runs(app);
    state.controller.run(conversation_id).is_some_and(|run| state.settled_on_host.get(conversation_id) == Some(&run.request_id))
}

/// What the overlay folds; anything else a task could push belongs to a run.
fn is_task_stream_event(event: &ModelStreamEvent) -> bool {
    matches!(event, ModelStreamEvent::SubagentEvent { .. } | ModelStreamEvent::SubagentDelta { .. } | ModelStreamEvent::WorkflowProgress { .. })
}

/// A `taskStreamEvent`: buffered, then folded at the run stream's commit cadence, however fast a child streams.
pub(super) fn receive_task_stream_event(app: &mut App, conversation_id: &str, event: ModelStreamEvent) {
    receive_task_stream_event_at(app, conversation_id, event, super::util::now_iso());
}

/// [`receive_task_stream_event`] for an event that arrived at `at`: a push, or one a live run's stream carried for a task
/// an earlier run started.
pub(super) fn receive_task_stream_event_at(app: &mut App, conversation_id: &str, event: ModelStreamEvent, at: String) {
    if !is_task_stream_event(&event) {
        return;
    }
    let held = run_settled_on_host(app, conversation_id);
    let overlay = runs(app).task_overlays.entry(conversation_id.to_owned()).or_default();
    if held {
        // The host has settled the run and the renderer has not: the event is held until the run has handed its own
        // tasks over (`run_ended`), which folds it on top of them.
        overlay.pending.push_at(event, at);
        return;
    }
    if !overlay.pending.push_at(event, at) {
        return;
    }
    let delay = overlay.pending.delay();
    let id = conversation_id.to_owned();
    let timer = app.set_timer(delay, move |app| {
        if let Some(overlay) = runs(app).task_overlays.get_mut(&id) {
            overlay.timer = None;
        }
        commit_task_overlay(app, &id);
    });
    if let Some(overlay) = runs(app).task_overlays.get_mut(conversation_id) {
        overlay.timer = Some(timer);
    }
}

/// Folds what arrived since the last commit, against the conversation's contexts as they are now. A run's commit that carried events
/// for these tasks calls it at once: they have waited for one beat already, and a second would put a child a whole second behind.
pub(super) fn commit_task_overlay(app: &mut App, conversation_id: &str) {
    // What is held for a run that is settling waits for it to hand over.
    if run_settled_on_host(app, conversation_id) {
        return;
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

/// The renderer has settled the conversation's run and taken it out of the controller. The tasks the run handed over
/// carry on in the overlay from where its stream left them, and what they pushed while the run was settling is folded
/// on top, in order, at once.
pub(super) fn run_ended(app: &mut App, conversation_id: &str, handoff: Option<TaskHandoff>) {
    // A run that already took the conversation over owns nothing of this one's; what was held is folded all the same.
    let handoff = handoff.filter(|handoff| !handoff.is_empty() && !has_run(app, conversation_id));
    let adopted = match handoff {
        Some(handoff) => {
            let mut overlay = runs(app).task_overlays.remove(conversation_id).unwrap_or_default();
            let persisted: &[ContextItem] = app.conversation(conversation_id).map_or(&[], |conversation| conversation.contexts.as_slice());
            let adopted = overlay.adopt(persisted, handoff);
            if adopted {
                overlay.changed();
            }
            if !overlay.is_spent() {
                runs(app).task_overlays.insert(conversation_id.to_owned(), overlay);
            }
            adopted
        }
        None => false,
    };
    if runs(app).task_overlays.contains_key(conversation_id) {
        commit_task_overlay(app, conversation_id);
    }
    if adopted {
        crate::prelude::repaint();
    }
}

impl App {
    /// What this conversation's background tasks streamed outside the runs that started them, merged by the timeline and
    /// the subagent views over the persisted cards. `None` while the conversation has a run: [`App::run_view`] draws the
    /// overlay then, beside the run's own projection, and is what to read. Cheap: built at most once per commit.
    pub fn task_overlay(&mut self, conversation_id: &str) -> Option<Arc<TaskOverlayView>> {
        if has_run(self, conversation_id) {
            return None;
        }
        self.task_overlay_view(conversation_id)
    }

    /// [`App::task_overlay`] whether or not the conversation has a run, for the run's view to draw.
    pub(super) fn task_overlay_view(&mut self, conversation_id: &str) -> Option<Arc<TaskOverlayView>> {
        if !self.runs().task_overlays.contains_key(conversation_id) {
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

    /// Drops a conversation's overlay: the conversation is gone.
    pub(crate) fn forget_task_overlay(&mut self, conversation_id: &str) {
        if let Some(overlay) = runs(self).task_overlays.remove(conversation_id) {
            if let Some(timer) = overlay.timer {
                self.clear_timer(timer);
            }
            crate::prelude::repaint();
        }
    }
}
