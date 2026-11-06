//! Port of `lib/modelStream.ts`: the pure projection of model streams.
//!
//! It folds [`ModelStreamEvent`] values into [`ModelRunState`], and nested subagent/workflow fragments into
//! [`SubagentLiveState`]. Upstream rebuilds immutable objects per event; here the fold mutates in place, because a
//! run's text would otherwise be copied once per token. What upstream expressed with object identity ("returns the
//! same run") is what `PartialEq` on the run says here, and nothing but the tests needs it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use super::capabilities::is_encrypted_reasoning;
use super::context_tokens::canonical_object;
use super::conversation_turns::{sum_model_usage, sum_usage_by_round};
use super::types::{ModelRunRequest, WorkflowProgressEntry};
use super::util::{create_id, parse_iso};
use crate::i18n::t;
use crate::model::{
    ContextItem, HookContextMetadata, JsonObject, ModelStreamEvent, ModelUsage, ReasoningForm, SubagentChannel, SubagentUpdate, ToolResult,
};

/// Where a live tool row is in the model execution pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingToolPhase {
    Announced,
    Ready,
    Running,
    Completed,
}

/// `SubagentLiveState.status`: the latest lifecycle transition streamed on the status channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentLiveStatus {
    Running,
    Idle,
    Interrupted,
    Failed,
    Stopped,
    RoundLimit,
}

/// A context as the timeline reads it while a run is live.
///
/// `ContextItem` is what is persisted; the renderer also needs what a persisted card never carries: whether it is
/// still growing, the phase of a live tool call, its child's live transcript, and the clock of live reasoning.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveContext {
    pub item: ContextItem,
    /// `undefined` / `false` / `true` of upstream's `streaming`.
    pub streaming: Option<bool>,
    /// Transient phase used only while a model run is active.
    pub stream_status: Option<StreamingToolPhase>,
    /// Live activity of a running subagent call; never persisted.
    pub live: Option<Arc<SubagentLiveState>>,
    /// When live reasoning opened; never reaches disk.
    pub started_at: Option<String>,
}

impl LiveContext {
    /// A context with nothing live about it.
    pub fn settled(item: ContextItem) -> Self {
        Self { item, streaming: None, stream_status: None, live: None, started_at: None }
    }

    fn streaming(item: ContextItem) -> Self {
        Self { streaming: Some(true), ..Self::settled(item) }
    }

    pub fn id(&self) -> &str {
        self.item.id()
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming == Some(true)
    }
}

/// The live state of one running child (`SubagentLiveState`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubagentLiveState {
    /// Failed projection survives backoff until this round starts replacing it.
    pub retry_round: Option<usize>,
    /// Standard contexts projected from the child's nested model stream.
    pub contexts: Vec<LiveContext>,
    /// Status updates explicitly reported by the child to its parent.
    pub updates: Vec<SubagentUpdate>,
    /// Latest lifecycle transition streamed on the status channel.
    pub status: Option<SubagentLiveStatus>,
    /// Provider-reported cumulative usage per child request round, streamed while the child runs. Keyed by round
    /// because each snapshot supersedes that round's previous one; summing the map is what gives the run's total.
    /// The persisted record wins once it exists — it is authoritative and spans every turn the agent ran — so this
    /// only ever fills the gap before settlement.
    pub usage_by_round: Option<BTreeMap<usize, ModelUsage>>,
    /// UI-only provider-call correlation; never copied into a persisted child transcript.
    pub tool_context_ids: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamingToolState {
    /// Stable local id; provider call id below exists only while this stream is active.
    pub id: String,
    pub call_id: String,
    pub tool_name: String,
    pub requested_input: Option<JsonObject>,
    pub input: JsonObject,
    pub result: ToolResult,
    pub stream_status: StreamingToolPhase,
    pub live: Arc<SubagentLiveState>,
    pub created_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingHookStatus {
    Running,
    Succeeded,
    Failed,
    Blocked,
}

impl StreamingHookStatus {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamingHookState {
    pub execution_id: String,
    pub hook_id: String,
    pub hook_name: String,
    pub event: String,
    pub status_message: Option<String>,
    pub result: ToolResult,
    pub status: StreamingHookStatus,
    pub context_injected: bool,
    pub reason: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostContextState {
    pub context: ContextItem,
    /// How many of the round's steered inputs had arrived before it. The host
    /// sends both in transcript order, so this puts each back in its place: a
    /// Stop hook's continuation precedes the round's steering, the notices
    /// delivered at its boundary follow it.
    pub after_steered: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRunRetryState {
    pub round: usize,
    pub attempt: u32,
    pub max_attempts: u32,
    pub message: String,
    /// True until the retried attempt streams its first event for the round;
    /// that first event discards the failed attempt's partial content.
    pub dirty: bool,
}

/// One followed run: everything streamed so far, by round.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRunState {
    pub request_id: String,
    pub provider_name: String,
    pub model_name: String,
    pub workspace_id: String,
    pub request: Arc<ModelRunRequest>,
    pub started_at: String,
    pub streamed_text_by_round: BTreeMap<usize, String>,
    /// Reasoning text per round, one slot per wire `item` ordinal. An encrypted-only
    /// item opens an empty slot at `reasoning_start` so its row remains visible.
    pub streamed_reasoning_by_round: BTreeMap<usize, Vec<String>>,
    pub reasoning_forms_by_round: BTreeMap<usize, BTreeMap<usize, ReasoningForm>>,
    pub completed_reasoning_by_round: BTreeMap<usize, bool>,
    pub active_reasoning_items_by_round: BTreeMap<usize, BTreeMap<usize, bool>>,
    /// When each round's reasoning opened, per `reasoning_start`. Drives the
    /// card's live clock before the provider's own figure lands, and is what
    /// makes a summary-less (encrypted-only) reasoning item visible at all.
    pub reasoning_started_at_by_round: BTreeMap<usize, String>,
    /// The sidecar's authoritative reasoning wall clock per round, from
    /// `reasoning_done`. Supersedes the live clock the moment it arrives so the
    /// number never changes across a reload.
    pub reasoning_duration_by_round: BTreeMap<usize, u64>,
    /// Per round and item, the sidecar's estimate of reasoning that streamed no
    /// text (`reasoning_progress`). Feeds the live token count beside the cat;
    /// never usage.
    pub hidden_reasoning_tokens_by_round: BTreeMap<usize, BTreeMap<usize, u64>>,
    pub streamed_tools_by_round: BTreeMap<usize, Vec<StreamingToolState>>,
    pub streamed_hooks_by_round: BTreeMap<usize, Vec<StreamingHookState>>,
    pub steered_inputs_by_round: BTreeMap<usize, Vec<ContextItem>>,
    /// Contexts the host added ahead of each round (`host_context_added`), in arrival order.
    pub host_contexts_by_round: BTreeMap<usize, Vec<HostContextState>>,
    /// Provider-reported cumulative snapshots, isolated by backend request round.
    pub usage_by_round: BTreeMap<usize, ModelUsage>,
    /// Nested agent usage snapshots, isolated by agent call and child request round.
    pub subagent_usage_by_call: BTreeMap<String, BTreeMap<usize, ModelUsage>>,
    /// Progress ledger rows per `workflow` call, in arrival order. The card folds
    /// them with `deriveWorkflowProgress`; keeping the raw stream here rather than
    /// the folded view means a late-arriving entry re-folds cheaply and the
    /// projection stays pure.
    pub workflow_progress_by_call: Arc<BTreeMap<String, Vec<WorkflowProgressEntry>>>,
    /// Live run id per `workflow` call — the address Skip/Retry commands carry.
    pub workflow_run_id_by_call: BTreeMap<String, String>,
    /// Lifecycle events that arrived before their call's announcement, keyed by
    /// round then call id. Replayed and cleared when the announcement lands.
    pub pending_tool_events_by_round: BTreeMap<usize, BTreeMap<String, Vec<ModelStreamEvent>>>,
    pub usage_revision: u64,
    pub retry: Option<ModelRunRetryState>,
}

impl ModelRunState {
    /// A run just registered (`registerRunState`).
    pub fn new(workspace_id: &str, request_id: &str, request: Arc<ModelRunRequest>, started_at: String) -> Self {
        Self {
            request_id: request_id.to_owned(),
            provider_name: request.provider.name.clone(),
            model_name: request.model.id.clone(),
            workspace_id: workspace_id.to_owned(),
            request,
            started_at,
            streamed_text_by_round: BTreeMap::new(),
            streamed_reasoning_by_round: BTreeMap::new(),
            reasoning_forms_by_round: BTreeMap::new(),
            completed_reasoning_by_round: BTreeMap::new(),
            active_reasoning_items_by_round: BTreeMap::new(),
            reasoning_started_at_by_round: BTreeMap::new(),
            reasoning_duration_by_round: BTreeMap::new(),
            hidden_reasoning_tokens_by_round: BTreeMap::new(),
            streamed_tools_by_round: BTreeMap::new(),
            streamed_hooks_by_round: BTreeMap::new(),
            steered_inputs_by_round: BTreeMap::new(),
            host_contexts_by_round: BTreeMap::new(),
            usage_by_round: BTreeMap::new(),
            subagent_usage_by_call: BTreeMap::new(),
            workflow_progress_by_call: Arc::default(),
            workflow_run_id_by_call: BTreeMap::new(),
            pending_tool_events_by_round: BTreeMap::new(),
            usage_revision: 0,
            retry: None,
        }
    }

    /// What settlement clears: everything streamed, leaving the usage counters.
    pub fn clear_streamed(&mut self) {
        self.streamed_text_by_round.clear();
        self.streamed_reasoning_by_round.clear();
        self.completed_reasoning_by_round.clear();
        self.reasoning_started_at_by_round.clear();
        self.reasoning_duration_by_round.clear();
        self.streamed_tools_by_round.clear();
        self.streamed_hooks_by_round.clear();
        self.steered_inputs_by_round.clear();
        self.host_contexts_by_round.clear();
    }
}

fn empty_result(at: &str) -> ToolResult {
    ToolResult { success: true, output: String::new(), images: Vec::new(), diff: None, executed_at: at.to_owned(), duration_ms: 0 }
}

/// Discards the failed attempt's streamed content for one round once its
/// retry starts delivering data, so the fresh attempt never appends onto the
/// partial it replaces. Hook records are host-side executions and survive;
/// tools of this round can only have been announced (they execute after the
/// stream completes), so dropping them is always safe.
pub fn reset_round_for_retry(run: &mut ModelRunState, round: usize) {
    run.streamed_text_by_round.remove(&round);
    run.streamed_reasoning_by_round.remove(&round);
    run.active_reasoning_items_by_round.remove(&round);
    run.reasoning_forms_by_round.remove(&round);
    run.completed_reasoning_by_round.remove(&round);
    // Retry-specific timing belongs to the retried round; retaining it would
    // start the new attempt's clock at the previous attempt.
    run.reasoning_started_at_by_round.remove(&round);
    run.reasoning_duration_by_round.remove(&round);
    run.hidden_reasoning_tokens_by_round.remove(&round);
    run.streamed_tools_by_round.remove(&round);
    run.usage_by_round.remove(&round);
}

pub fn cumulative_model_run_usage(run: &ModelRunState) -> ModelUsage {
    let mut usages = vec![sum_usage_by_round(&run.usage_by_round)];
    usages.extend(run.subagent_usage_by_call.values().map(sum_usage_by_round));
    sum_model_usage(&usages)
}

/// Unwraps a `subagent_event` envelope chain down to a leaf usage snapshot.
///
/// A child's events arrive wrapped once; a workflow step's arrive wrapped twice
/// — the run's envelope around the step's envelope — and nothing stops the tree
/// from being deeper still. Reading only the immediate payload therefore dropped
/// every workflow step's tokens on the floor, so the turn's own total silently
/// omitted whatever a workflow spent.
///
/// The returned key is the whole call path, because two steps of one run are
/// distinct spenders that would otherwise overwrite each other under the run's
/// single call id. A depth-one child's key is still its bare call id, so the
/// accounting map's existing entries keep their names.
fn nested_usage_update(event: &ModelStreamEvent) -> Option<(String, usize, &ModelUsage)> {
    let mut path: Vec<&str> = Vec::new();
    let mut node = event;
    while let ModelStreamEvent::SubagentEvent { call_id, event, .. } = node {
        path.push(call_id);
        node = event;
    }
    match node {
        ModelStreamEvent::UsageUpdated { round, usage } => Some((path.join("/"), *round, usage)),
        _ => None,
    }
}

/// Cap per orphaned call: a call that never announces should cost a bounded
/// amount of memory, not a transcript's worth of buffered lifecycle noise.
const PENDING_TOOL_EVENT_LIMIT: usize = 32;
/// Cap on distinct orphaned calls per round: without it a hostile or broken
/// stream can allocate one 32-event queue per fabricated call id. Beyond the
/// cap, extra orphans get the old behavior — dropped, backstopped by the
/// settled-card promotion.
const PENDING_TOOL_CALL_LIMIT: usize = 16;

/// Holds a lifecycle event received before its call announcement. Reorderings
/// can occur during adoption replay and coalescer batching; replay it after the
/// announcement so a completed call remains visible live.
fn buffer_orphan_tool_event(run: &mut ModelRunState, call_id: &str, event: &ModelStreamEvent, round: usize) {
    let round_pending = run.pending_tool_events_by_round.get(&round);
    let queue_len = round_pending.and_then(|pending| pending.get(call_id)).map_or(0, Vec::len);
    if queue_len >= PENDING_TOOL_EVENT_LIMIT {
        return;
    }
    if queue_len == 0 && round_pending.map_or(0, BTreeMap::len) >= PENDING_TOOL_CALL_LIMIT {
        return;
    }
    run.pending_tool_events_by_round.entry(round).or_default().entry(call_id.to_owned()).or_default().push(event.clone());
}

fn replay_pending_tool_events(run: &mut ModelRunState, call_id: &str, round: usize, event_at: &str) {
    let Some(queue) = run.pending_tool_events_by_round.get_mut(&round).and_then(|pending| pending.remove(call_id)) else { return };
    for pending in &queue {
        apply_tool_stream_event(run, pending, round, event_at);
    }
}

fn apply_tool_stream_event(run: &mut ModelRunState, event: &ModelStreamEvent, round: usize, event_at: &str) {
    let call_id = match event {
        ModelStreamEvent::ToolCallAnnounced { call_id, .. }
        | ModelStreamEvent::ToolCallArgumentsReady { call_id, .. }
        | ModelStreamEvent::ToolExecutionStarted { call_id, .. }
        | ModelStreamEvent::ToolExecutionCompleted { call_id, .. } => call_id,
        _ => return,
    };
    let index = run.streamed_tools_by_round.get(&round).and_then(|tools| tools.iter().position(|tool| &tool.call_id == call_id));
    match event {
        ModelStreamEvent::ToolCallAnnounced { tool_name, context_id, .. } => {
            if let Some(index) = index {
                let tool = &mut run.streamed_tools_by_round.get_mut(&round).expect("found above")[index];
                if tool.tool_name != *tool_name {
                    tool.tool_name = tool_name.clone();
                }
                return;
            }
            run.streamed_tools_by_round.entry(round).or_default().push(StreamingToolState {
                // The host derives this id and sends it with the announcement, so
                // the streaming row and the card that eventually persists are the
                // same timeline entry. Older streams that predate the field fall
                // back to a local id; such a row still renders, it simply cannot
                // be reconciled with the host's card by id.
                id: if context_id.is_empty() { create_id("ctx") } else { context_id.clone() },
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                requested_input: None,
                input: JsonObject::new(),
                result: empty_result(event_at),
                stream_status: StreamingToolPhase::Announced,
                live: Arc::new(SubagentLiveState::default()),
                created_at: event_at.to_owned(),
            });
            // A fresh announcement releases whatever this call buffered while orphaned.
            replay_pending_tool_events(run, call_id, round, event_at);
        }
        ModelStreamEvent::ToolCallArgumentsReady { input, .. } => {
            let Some(index) = index else { return buffer_orphan_tool_event(run, call_id, event, round) };
            let tool = &mut run.streamed_tools_by_round.get_mut(&round).expect("found above")[index];
            if tool.requested_input.is_none() {
                tool.requested_input = Some(input.clone());
            }
            tool.input = input.clone();
            if tool.stream_status == StreamingToolPhase::Announced {
                tool.stream_status = StreamingToolPhase::Ready;
            }
        }
        ModelStreamEvent::ToolExecutionStarted { .. } => {
            let Some(index) = index else { return buffer_orphan_tool_event(run, call_id, event, round) };
            let tool = &mut run.streamed_tools_by_round.get_mut(&round).expect("found above")[index];
            if tool.stream_status != StreamingToolPhase::Completed {
                tool.stream_status = StreamingToolPhase::Running;
            }
        }
        ModelStreamEvent::ToolExecutionCompleted { result, .. } => {
            let Some(index) = index else { return buffer_orphan_tool_event(run, call_id, event, round) };
            let tool = &mut run.streamed_tools_by_round.get_mut(&round).expect("found above")[index];
            tool.result = result.clone();
            tool.stream_status = StreamingToolPhase::Completed;
        }
        _ => {}
    }
}

pub const SUBAGENT_LIVE_CONTEXT_LIMIT: usize = 200;
const SUBAGENT_LIVE_UPDATE_LIMIT: usize = 50;
/// Retained progress entries per workflow call. Above the host's own 1000-row
/// ledger bound because this array holds transitions, not merged rows: a
/// 50-step run emits roughly three entries per step plus narration, and the
/// limit should only ever bite on a runaway narrator.
const WORKFLOW_PROGRESS_ENTRY_LIMIT: usize = 4_000;

fn nested_context_id(parent_tool_id: &str, kind: &str, nested_id: impl std::fmt::Display) -> String {
    format!("subagent-stream-{parent_tool_id}-{kind}-{nested_id}")
}

/// A round that ended without finishing its reasoning, on a card that has nothing
/// else in it.
///
/// Encrypted reasoning is a presentation copy of a trace whose substance is the
/// provider's ciphertext, and a round that never reached its `done` frame
/// produced none: the card has no body, no duration, and nothing a later turn
/// could replay. Plaintext reasoning is its own content, so even a fragment of it
/// is a truthful record and stays.
fn abandoned_encrypted_reasoning(context: &LiveContext) -> bool {
    match &context.item {
        ContextItem::Reasoning { content, form, .. } => context.is_streaming() && content.as_deref().unwrap_or("").is_empty() && is_encrypted_reasoning(*form, content.as_deref()),
        _ => false,
    }
}

/// What a child's transcript becomes once the child has stopped: nothing in it is growing any more.
pub fn settle_subagent_contexts(contexts: &mut Vec<LiveContext>) {
    contexts.retain(|context| !abandoned_encrypted_reasoning(context));
    for context in contexts.iter_mut() {
        if !context.is_streaming() {
            continue;
        }
        match &mut context.item {
            ContextItem::Tool { requested_input, input, result, .. } => {
                context.streaming = None;
                let status = context.stream_status.take();
                if requested_input.as_ref().is_some_and(|requested| canonical_object(requested) == canonical_object(input)) {
                    *requested_input = None;
                }
                if status == Some(StreamingToolPhase::Completed) {
                    continue;
                }
                // A call that reported its own terminal lifecycle ended on its own terms,
                // whatever its stream phase says. `workflow_step` is the case that makes
                // this mandatory rather than defensive: `spawn_step` announces the
                // synthetic call and sends its identity, then never emits a
                // `tool_execution_completed` for it — a step's outcome travels as an agent
                // status delta, and its real result is only minted at the end of the run
                // by `synthesize_step_context`. Reading the missing completion as an
                // interruption therefore reported *every* step of *every* workflow as a
                // failure for the rest of the streaming turn, however well it went.
                let own_status = context.live.as_ref().and_then(|live| live.status);
                match own_status {
                    // `idle` is the clean ending; the placeholder result stands. Anything
                    // else did end badly, so the row keeps its failure tone — but the
                    // reason belongs to the child's lifecycle, not to a fabricated
                    // "interrupted before it completed" that never happened.
                    Some(SubagentLiveStatus::Idle) => {}
                    Some(status) if status != SubagentLiveStatus::Running => result.success = false,
                    _ => {
                        result.success = false;
                        if result.output.is_empty() {
                            result.output = t("子代理工具调用在完成前被中断。", "The subagent tool call was interrupted before it completed.").to_owned();
                        }
                    }
                }
            }
            ContextItem::Assistant { .. } | ContextItem::Reasoning { .. } | ContextItem::System { .. } => context.streaming = None,
            ContextItem::User { .. } => {}
        }
    }
}

/// Applies one parent-facing lifecycle/progress fragment to a single live
/// child. The same reducer is used at every nesting level so a dispatcher,
/// executor, and page reader retain identical streaming semantics.
pub(super) fn apply_subagent_channel_delta(live: &mut SubagentLiveState, parent_tool_id: &str, channel: SubagentChannel, delta: &str, event_at: &str) {
    match channel {
        SubagentChannel::Text => {
            apply_nested_subagent_event(live, parent_tool_id, &ModelStreamEvent::TextDelta { round: 0, delta: delta.to_owned() }, event_at);
        }
        SubagentChannel::Reasoning => {
            apply_nested_subagent_event(live, parent_tool_id, &ModelStreamEvent::ReasoningDelta { round: 0, item: 0, delta: delta.to_owned() }, event_at);
        }
        SubagentChannel::Update => {
            live.updates.push(SubagentUpdate { content: delta.to_owned(), created_at: event_at.to_owned() });
            if live.updates.len() > SUBAGENT_LIVE_UPDATE_LIMIT {
                let excess = live.updates.len() - SUBAGENT_LIVE_UPDATE_LIMIT;
                live.updates.drain(..excess);
            }
        }
        SubagentChannel::Status => {
            // Empty status deltas are task_wait liveness heartbeats. Every other value
            // in `AgentLiveStatus::wire` is a real lifecycle transition: dropping the
            // terminal ones (failed/stopped/roundLimit) would leave the child pinned to
            // its last-known state and hide how the run actually ended.
            let status = match delta {
                "running" => SubagentLiveStatus::Running,
                "idle" => SubagentLiveStatus::Idle,
                "interrupted" => SubagentLiveStatus::Interrupted,
                "failed" => SubagentLiveStatus::Failed,
                "stopped" => SubagentLiveStatus::Stopped,
                "roundLimit" => SubagentLiveStatus::RoundLimit,
                _ => return,
            };
            if status != SubagentLiveStatus::Running {
                settle_subagent_contexts(&mut live.contexts);
            }
            live.status = Some(status);
        }
        SubagentChannel::Activity => {
            let mut input = JsonObject::new();
            input.insert("message".to_owned(), serde_json::Value::String(delta.to_owned()));
            let activity = ContextItem::Tool {
                id: nested_context_id(parent_tool_id, "activity", format!("{event_at}-{}", live.contexts.len())),
                tool_name: "subagent_activity".to_owned(),
                round: None,
                model_turn_id: None,
                provider_call_id: None,
                requested_input: None,
                input,
                result: ToolResult { output: t("活动已记录", "Activity recorded").to_owned(), ..empty_result(event_at) },
                subagent: None,
                notice: None,
                attestation: String::new(),
                created_at: event_at.to_owned(),
            };
            live.contexts.push(LiveContext::settled(activity));
            trim_child(live);
        }
    }
}

fn trim_child(live: &mut SubagentLiveState) {
    if live.contexts.len() > SUBAGENT_LIVE_CONTEXT_LIMIT {
        let excess = live.contexts.len() - SUBAGENT_LIVE_CONTEXT_LIMIT;
        live.contexts.drain(..excess);
    }
}

fn nested_reasoning_id(parent_tool_id: &str, round: usize, item: usize) -> String {
    let base = nested_context_id(parent_tool_id, "reasoning", round);
    if item == 0 { base } else { format!("{base}-item-{item}") }
}

/// Event types whose first post-retry arrival discards the failed attempt's
/// partial round. Subagent and hook events are excluded: their `round` refers
/// to the spawning round, not the request being retried.
fn resets_retry(event: &ModelStreamEvent) -> bool {
    matches!(
        event,
        ModelStreamEvent::TextDelta { .. }
            | ModelStreamEvent::ReasoningStart { .. }
            | ModelStreamEvent::ReasoningDelta { .. }
            | ModelStreamEvent::ReasoningDone { .. }
            | ModelStreamEvent::ReasoningProgress { .. }
            | ModelStreamEvent::UsageUpdated { .. }
            | ModelStreamEvent::ToolCallAnnounced { .. }
            | ModelStreamEvent::ToolCallArgumentsReady { .. }
            | ModelStreamEvent::ToolExecutionStarted { .. }
            | ModelStreamEvent::ToolExecutionCompleted { .. }
    )
}

/// The round an event carries, when it carries one.
fn event_round(event: &ModelStreamEvent) -> Option<usize> {
    match event {
        ModelStreamEvent::TextDelta { round, .. }
        | ModelStreamEvent::ReasoningStart { round, .. }
        | ModelStreamEvent::ReasoningDelta { round, .. }
        | ModelStreamEvent::ReasoningDone { round, .. }
        | ModelStreamEvent::ReasoningProgress { round, .. }
        | ModelStreamEvent::UsageUpdated { round, .. }
        | ModelStreamEvent::UserInputReceived { round, .. }
        | ModelStreamEvent::HostContextAdded { round, .. }
        | ModelStreamEvent::ToolCallAnnounced { round, .. }
        | ModelStreamEvent::ToolCallArgumentsReady { round, .. }
        | ModelStreamEvent::ToolExecutionStarted { round, .. }
        | ModelStreamEvent::SubagentEvent { round, .. }
        | ModelStreamEvent::SubagentDelta { round, .. }
        | ModelStreamEvent::WorkflowProgress { round, .. }
        | ModelStreamEvent::ToolExecutionCompleted { round, .. }
        | ModelStreamEvent::ToolContextSettled { round, .. }
        | ModelStreamEvent::HookExecutionStarted { round, .. }
        | ModelStreamEvent::HookExecutionCompleted { round, .. }
        | ModelStreamEvent::StreamRetryScheduled { round, .. } => Some(*round),
        #[cfg(debug_assertions)]
        ModelStreamEvent::DebugRequestBody { round, .. } => Some(*round),
        ModelStreamEvent::ToolApprovalRequested { .. } | ModelStreamEvent::ToolApprovalResolved { .. } | ModelStreamEvent::Ping | ModelStreamEvent::RunConcluded { .. } => None,
    }
}

fn replace_context(contexts: &mut [LiveContext], id: &str, update: impl FnOnce(&mut LiveContext)) {
    if let Some(context) = contexts.iter_mut().find(|context| context.id() == id) {
        update(context);
    }
}

fn hook_label() -> &'static str {
    t("生命周期钩子", "Lifecycle hook")
}

/// Projects a normalized nested model event into the same context stream
/// consumed by the primary conversation renderer.
pub(super) fn apply_nested_subagent_event(live: &mut SubagentLiveState, parent_tool_id: &str, event: &ModelStreamEvent, event_at: &str) {
    if let ModelStreamEvent::StreamRetryScheduled { round, .. } = event {
        live.retry_round = Some(*round);
        return;
    }
    if resets_retry(event) && live.retry_round.is_some() && event_round(event) == live.retry_round {
        // Match the top-level boundary: backoff itself does not erase visible output.
        let round = event_round(event).unwrap_or(0);
        let assistant_id = nested_context_id(parent_tool_id, "assistant", round);
        let reasoning_id = nested_context_id(parent_tool_id, "reasoning", round);
        let item_prefix = format!("{reasoning_id}-item-");
        live.retry_round = None;
        // The failed attempt's usage snapshot goes with them: the replacement
        // attempt reports its own cumulative figure for this round, and keeping
        // both would bill the child twice for one round it only ran once.
        if let Some(usage) = &mut live.usage_by_round {
            usage.remove(&round);
        }
        live.contexts.retain(|context| {
            let id = context.id();
            if id == assistant_id || id == reasoning_id || id.starts_with(&item_prefix) {
                return false;
            }
            !(matches!(&context.item, ContextItem::Tool { round: Some(r), .. } if *r == round)
                && context.is_streaming()
                && matches!(context.stream_status, Some(StreamingToolPhase::Announced | StreamingToolPhase::Ready)))
        });
        let kept: HashSet<&str> = live.contexts.iter().map(LiveContext::id).collect();
        let tool_context_ids = std::mem::take(&mut live.tool_context_ids);
        live.tool_context_ids = tool_context_ids.into_iter().filter(|(_, id)| kept.contains(id.as_str())).collect();
        return apply_nested_subagent_event(live, parent_tool_id, event, event_at);
    }

    let model_turn_id = event_round(event).map(|round| format!("model-turn-{parent_tool_id}-{round}"));
    match event {
        ModelStreamEvent::TextDelta { round, delta } => {
            let id = nested_context_id(parent_tool_id, "assistant", round);
            if let Some(context) = live.contexts.iter_mut().find(|context| context.id() == id) {
                if let ContextItem::Assistant { content, .. } = &mut context.item {
                    content.push_str(delta);
                    context.streaming = Some(true);
                }
                // The id is only ever an assistant's; anything else there is left alone.
            } else {
                live.contexts.push(LiveContext::streaming(ContextItem::Assistant {
                    id,
                    content: delta.clone(),
                    round: Some(*round),
                    model_turn_id,
                    interrupted: false,
                    sources: Vec::new(),
                    created_at: event_at.to_owned(),
                }));
            }
        }
        ModelStreamEvent::ReasoningStart { round, item, form } => {
            // Create the reasoning card at `reasoning_start`: encrypted-only reasoning
            // never emits a delta, and its form must not be inferred from the parent run.
            let id = nested_reasoning_id(parent_tool_id, *round, *item);
            if !live.contexts.iter().any(|context| context.id() == id) {
                let mut card = LiveContext::streaming(reasoning_item(id, String::new(), *form, *round, model_turn_id, event_at));
                card.started_at = Some(event_at.to_owned());
                live.contexts.push(card);
            }
        }
        ModelStreamEvent::ReasoningDelta { round, item, delta } => {
            let id = nested_reasoning_id(parent_tool_id, *round, *item);
            if let Some(context) = live.contexts.iter_mut().find(|context| context.id() == id) {
                if let ContextItem::Reasoning { content, .. } = &mut context.item {
                    content.get_or_insert_with(String::new).push_str(delta);
                    context.streaming = Some(true);
                }
            } else {
                let mut card = LiveContext::streaming(reasoning_item(id, delta.clone(), None, *round, model_turn_id, event_at));
                // Providers without `reasoning_start` use the first delta as the start.
                card.started_at = Some(event_at.to_owned());
                live.contexts.push(card);
            }
        }
        ModelStreamEvent::ReasoningDone { round, item, duration_ms } => {
            let id = nested_reasoning_id(parent_tool_id, *round, *item);
            replace_context(&mut live.contexts, &id, |context| {
                if matches!(context.item, ContextItem::Reasoning { .. }) {
                    context.streaming = Some(false);
                }
            });
            if let Some(duration) = duration_ms {
                replace_context(&mut live.contexts, &nested_reasoning_id(parent_tool_id, *round, 0), |context| {
                    if let ContextItem::Reasoning { duration_ms, .. } = &mut context.item {
                        *duration_ms = Some(*duration);
                    }
                });
            }
        }
        ModelStreamEvent::ToolCallAnnounced { round, call_id, tool_name, context_id } => {
            let assistant_id = nested_context_id(parent_tool_id, "assistant", round);
            if let Some(context) = live.contexts.iter_mut().find(|context| context.id() == assistant_id) {
                if matches!(context.item, ContextItem::Assistant { .. }) {
                    context.streaming = Some(false);
                }
            } else {
                live.contexts.push(LiveContext::streaming(ContextItem::Assistant {
                    id: assistant_id,
                    content: String::new(),
                    round: Some(*round),
                    model_turn_id: model_turn_id.clone(),
                    interrupted: false,
                    sources: Vec::new(),
                    created_at: event_at.to_owned(),
                }));
            }
            // Same rule as the top-level rows: the host announces the id the child's
            // card will carry once it persists, so the live row adopts it instead of
            // minting a rival. A nested card lives inside the parent's fingerprinted
            // subagent record, so a mismatch here does not just duplicate a row — it
            // changes the parent's fingerprint and makes the parent card unsaveable.
            let id = live.tool_context_ids.get(call_id).cloned().unwrap_or_else(|| if context_id.is_empty() { create_id("ctx") } else { context_id.clone() });
            live.tool_context_ids.insert(call_id.clone(), id.clone());
            let announced = || {
                let mut card = LiveContext::streaming(ContextItem::Tool {
                    id: id.clone(),
                    tool_name: tool_name.clone(),
                    round: Some(*round),
                    model_turn_id: model_turn_id.clone(),
                    provider_call_id: None,
                    requested_input: None,
                    input: JsonObject::new(),
                    result: empty_result(event_at),
                    subagent: None,
                    notice: None,
                    attestation: String::new(),
                    created_at: event_at.to_owned(),
                });
                card.stream_status = Some(StreamingToolPhase::Announced);
                card
            };
            if let Some(context) = live.contexts.iter_mut().find(|context| context.id() == id) {
                // A step announced again is one the user retried: the host stopped it and
                // starts it over under the same call id, so its tile starts over too
                // rather than appending the new attempt to the abandoned one's transcript.
                if tool_name == "workflow_step" {
                    *context = announced();
                }
            } else {
                let card = announced();
                live.contexts.push(card);
            }
        }
        ModelStreamEvent::ToolCallArgumentsReady { call_id, input: new_input, .. } => {
            let Some(id) = live.tool_context_ids.get(call_id).cloned() else { return };
            replace_context(&mut live.contexts, &id, |context| {
                if let ContextItem::Tool { requested_input, input, .. } = &mut context.item {
                    if requested_input.is_none() {
                        *requested_input = Some(new_input.clone());
                    }
                    *input = new_input.clone();
                    if context.stream_status == Some(StreamingToolPhase::Announced) {
                        context.stream_status = Some(StreamingToolPhase::Ready);
                    }
                }
            });
        }
        ModelStreamEvent::ToolExecutionStarted { call_id, .. } => {
            let Some(id) = live.tool_context_ids.get(call_id).cloned() else { return };
            replace_context(&mut live.contexts, &id, |context| {
                if matches!(context.item, ContextItem::Tool { .. }) {
                    context.stream_status = Some(StreamingToolPhase::Running);
                }
            });
        }
        ModelStreamEvent::ToolExecutionCompleted { call_id, result: new_result, .. } => {
            let Some(id) = live.tool_context_ids.get(call_id).cloned() else { return };
            replace_context(&mut live.contexts, &id, |context| {
                if let ContextItem::Tool { result, .. } = &mut context.item {
                    *result = new_result.clone();
                    context.stream_status = Some(StreamingToolPhase::Completed);
                }
            });
        }
        ModelStreamEvent::HookExecutionStarted { execution_id, hook_id, hook_name, event: hook_event, status_message, .. } => {
            let id = nested_context_id(parent_tool_id, "hook", execution_id);
            if !live.contexts.iter().any(|context| context.id() == id) {
                let status = status_message.as_deref().unwrap_or(t("执行中", "Running"));
                live.contexts.push(LiveContext::streaming(ContextItem::System {
                    id,
                    content: format!("[{} · {hook_event}]\n{hook_name}\n{status}", hook_label()),
                    local_only: true,
                    hook_execution: Some(HookContextMetadata {
                        execution_id: execution_id.clone(),
                        hook_id: hook_id.clone(),
                        hook_name: hook_name.clone(),
                        event: hook_event.clone(),
                        status: "running".to_owned(),
                        context_injected: false,
                    }),
                    tools_added: Vec::new(),
                    native_compaction: None,
                    created_at: event_at.to_owned(),
                }));
            }
        }
        ModelStreamEvent::HookExecutionCompleted { execution_id, hook_id, hook_name, event: hook_event, result, blocked, reason, context_injected, .. } => {
            let id = nested_context_id(parent_tool_id, "hook", execution_id);
            let status = hook_completion_status(*blocked, result.success);
            let status_label = hook_status_label(status);
            let body = if result.output.is_empty() { status_label } else { &result.output };
            let reason_text = reason.as_deref().map(|r| format!("\n\n{r}")).unwrap_or_default();
            let text = format!("[{} · {hook_event}]\n{hook_name}\n{body}{reason_text}", hook_label());
            let metadata = HookContextMetadata {
                execution_id: execution_id.clone(),
                hook_id: hook_id.clone(),
                hook_name: hook_name.clone(),
                event: hook_event.clone(),
                status: status.wire().to_owned(),
                context_injected: *context_injected,
            };
            if let Some(context) = live.contexts.iter_mut().find(|context| context.id() == id) {
                if let ContextItem::System { content, hook_execution, .. } = &mut context.item {
                    *content = text;
                    *hook_execution = Some(metadata);
                    context.streaming = Some(false);
                }
            } else {
                let mut card = LiveContext::settled(ContextItem::System {
                    id,
                    content: text,
                    local_only: true,
                    hook_execution: Some(metadata),
                    tools_added: Vec::new(),
                    native_compaction: None,
                    created_at: event_at.to_owned(),
                });
                card.streaming = Some(false);
                live.contexts.push(card);
            }
        }
        ModelStreamEvent::HostContextAdded { context, .. } => {
            // A child's transcript is in arrival order, which is the host's order.
            if !live.contexts.iter().any(|known| known.id() == context.id()) {
                live.contexts.push(LiveContext::settled((**context).clone()));
            }
        }
        ModelStreamEvent::UsageUpdated { round, usage } => {
            // The child's own provider snapshot. Keyed by round and replaced rather
            // than accumulated: each snapshot is already cumulative for its round, so
            // adding them would multiply the child's cost by the number of chunks its
            // provider happened to report usage in.
            live.usage_by_round.get_or_insert_with(BTreeMap::new).insert(*round, usage.clone());
            // Reasoning tokens belong to this round. Child contexts have no other
            // round-to-usage mapping, and zero is omitted.
            if let Some(reasoning_tokens) = usage.reasoning_tokens.filter(|&tokens| tokens > 0) {
                replace_context(&mut live.contexts, &nested_context_id(parent_tool_id, "reasoning", round), |context| {
                    if let ContextItem::Reasoning { tokens, .. } = &mut context.item {
                        *tokens = Some(reasoning_tokens);
                    }
                });
            }
        }
        ModelStreamEvent::SubagentEvent { call_id, event: nested, .. } => {
            // Nested agent events are correlated to the run-tool context announced in
            // this child's own stream. Preserve a live state on that context and feed
            // the payload through this same reducer, recursively. This is what makes a
            // running search group visible as dispatcher → executor → page reader
            // before the backend settles and persists the final transcripts.
            //
            // Usage rides the same recursion: a workflow step's snapshot arrives
            // wrapped twice (run envelope, then step envelope), so it lands on the
            // step's own live state exactly like its text does.
            let Some(id) = live.tool_context_ids.get(call_id).cloned() else { return };
            replace_context(&mut live.contexts, &id, |context| {
                if matches!(context.item, ContextItem::Tool { .. }) {
                    let child = Arc::make_mut(context.live.get_or_insert_with(Default::default));
                    apply_nested_subagent_event(child, &id, nested, event_at);
                }
            });
        }
        ModelStreamEvent::SubagentDelta { call_id, channel, delta, .. } => {
            let Some(id) = live.tool_context_ids.get(call_id).cloned() else { return };
            replace_context(&mut live.contexts, &id, |context| {
                if matches!(context.item, ContextItem::Tool { .. }) {
                    let child = Arc::make_mut(context.live.get_or_insert_with(Default::default));
                    apply_subagent_channel_delta(child, &id, *channel, delta, event_at);
                }
            });
        }
        _ => {}
    }
    trim_child(live);
}

fn reasoning_item(id: String, content: String, form: Option<ReasoningForm>, round: usize, model_turn_id: Option<String>, at: &str) -> ContextItem {
    ContextItem::Reasoning {
        id,
        content: Some(content),
        form,
        round: Some(round),
        model_turn_id,
        interrupted: false,
        duration_ms: None,
        tokens: None,
        replay: None,
        created_at: at.to_owned(),
    }
}

fn hook_completion_status(blocked: bool, success: bool) -> StreamingHookStatus {
    if blocked {
        StreamingHookStatus::Blocked
    } else if success {
        StreamingHookStatus::Succeeded
    } else {
        StreamingHookStatus::Failed
    }
}

fn hook_status_label(status: StreamingHookStatus) -> &'static str {
    match status {
        StreamingHookStatus::Blocked => t("已阻止当前操作", "Blocked the current operation"),
        StreamingHookStatus::Failed => t("执行错误（未阻断）", "Failed without blocking"),
        _ => t("执行完成", "Completed"),
    }
}

/// Append one workflow progress row to its call's stream.
///
/// Deliberately append-only: the merge lives in `deriveWorkflowProgress`, which
/// is pure and therefore testable without a tree. Folding here instead would put
/// the card's whole merge rule inside a reducer.
///
/// The stream is bounded, because one entry per step transition over a long run
/// accumulates for the whole turn. Only LOG entries are evicted — never agent
/// ones, mirroring the host ledger's rule. Dropping the oldest entries wholesale
/// would be wrong here in a way it is not on the host: the host stores one
/// merged row per step, while this array stores every transition, so evicting
/// the front of it can delete every trace of a step that finished early. That
/// step would vanish from the card mid-run, and the totals with it.
fn apply_workflow_progress(run: &mut ModelRunState, call_id: &str, run_id: &str, entry: WorkflowProgressEntry) {
    push_workflow_progress(&mut run.workflow_progress_by_call, &mut run.workflow_run_id_by_call, call_id, run_id, entry);
}

/// [`apply_workflow_progress`] on the two maps alone, for whoever keeps them outside a run.
pub(super) fn push_workflow_progress(progress_by_call: &mut Arc<BTreeMap<String, Vec<WorkflowProgressEntry>>>, run_id_by_call: &mut BTreeMap<String, String>, call_id: &str, run_id: &str, entry: WorkflowProgressEntry) {
    let entries = Arc::make_mut(progress_by_call).entry(call_id.to_owned()).or_default();
    entries.push(entry);
    if entries.len() > WORKFLOW_PROGRESS_ENTRY_LIMIT {
        let mut excess = entries.len() - WORKFLOW_PROGRESS_ENTRY_LIMIT;
        entries.retain(|entry| {
            if excess > 0 && entry.kind == super::types::WorkflowEntryKind::Log {
                excess -= 1;
                return false;
            }
            true
        });
    }
    // Last writer wins, and there is only ever one: the host mints a run id per
    // workflow call and reuses it for every transition of that run, including
    // across a resume.
    run_id_by_call.insert(call_id.to_owned(), run_id.to_owned());
}

/// Total live transcript rows the renderer keeps across every background child
/// of one run, on top of each child's own [`SUBAGENT_LIVE_CONTEXT_LIMIT`].
///
/// The per-child cap alone bounds nothing in aggregate: ten children hold ten
/// times as much, and a workflow fan-out holds as much as it likes. This is the
/// ceiling that actually holds.
pub const SUBAGENT_LIVE_TOTAL_CONTEXT_BUDGET: usize = 1_200;

/// One node of the live tree, addressed by the path of context indexes from its top-level tool.
struct LiveTrimTarget {
    /// Nesting level; a deeper child is dropped before a shallower one.
    depth: usize,
    /// Arrival order among equal depths; the earliest is dropped first.
    order: usize,
    len: usize,
    node: *const SubagentLiveState,
}

/// Walks one live subtree, appending every node in arrival order.
fn collect_live_targets(live: &Arc<SubagentLiveState>, depth: usize, targets: &mut Vec<LiveTrimTarget>) {
    targets.push(LiveTrimTarget { depth, order: targets.len(), len: live.contexts.len(), node: Arc::as_ptr(live) });
    for context in &live.contexts {
        if let (ContextItem::Tool { .. }, Some(child)) = (&context.item, &context.live) {
            collect_live_targets(child, depth + 1, targets);
        }
    }
}

/// Rebuilds one live subtree, applying whatever drop count it was assigned. A node is looked up by the address it had
/// when the targets were collected, so children are rewritten before the parent's `Arc` can move.
fn apply_live_drops(live: &mut Arc<SubagentLiveState>, drops: &HashMap<*const SubagentLiveState, usize>) {
    let drop = drops.get(&Arc::as_ptr(live)).copied().unwrap_or(0);
    let nested_hit = live.contexts.iter().any(|context| context.live.as_ref().is_some_and(|child| subtree_has_drop(child, drops)));
    if drop == 0 && !nested_hit {
        return;
    }
    let node = Arc::make_mut(live);
    if drop > 0 {
        node.contexts.drain(..drop.min(node.contexts.len()));
    }
    for context in &mut node.contexts {
        if matches!(context.item, ContextItem::Tool { .. }) {
            if let Some(child) = &mut context.live {
                apply_live_drops(child, drops);
            }
        }
    }
}

fn subtree_has_drop(live: &Arc<SubagentLiveState>, drops: &HashMap<*const SubagentLiveState, usize>) -> bool {
    drops.contains_key(&Arc::as_ptr(live)) || live.contexts.iter().any(|context| context.live.as_ref().is_some_and(|child| subtree_has_drop(child, drops)))
}

/// Enforces [`SUBAGENT_LIVE_TOTAL_CONTEXT_BUDGET`] over one run's whole child tree.
///
/// Eviction order is deepest-first, then oldest-first: a deep child's transcript
/// is the least likely thing the user is reading (it sits several disclosures
/// down), and within one depth the earliest rows are the ones already scrolled
/// past. Rows are dropped from the front of a child's own transcript rather than
/// dropping the child, so every agent keeps its identity, its status and its
/// most recent output no matter how far over budget the run goes.
///
/// `false` when nothing had to be dropped, so the common case costs one traversal and no allocation of rows.
pub fn trim_live_context_budget(run: &mut ModelRunState) -> bool {
    let mut targets = Vec::new();
    for tools in run.streamed_tools_by_round.values() {
        for tool in tools {
            collect_live_targets(&tool.live, 0, &mut targets);
        }
    }
    let mut total: usize = targets.iter().map(|target| target.len).sum();
    if total <= SUBAGENT_LIVE_TOTAL_CONTEXT_BUDGET {
        return false;
    }
    // Deepest first, then oldest first.
    targets.sort_by(|left, right| right.depth.cmp(&left.depth).then(left.order.cmp(&right.order)));
    let mut drops: HashMap<*const SubagentLiveState, usize> = HashMap::new();
    for target in &targets {
        if total <= SUBAGENT_LIVE_TOTAL_CONTEXT_BUDGET {
            break;
        }
        // Always leave one row so an evicted child still shows what it last did.
        let drop = (total - SUBAGENT_LIVE_TOTAL_CONTEXT_BUDGET).min(target.len.saturating_sub(1));
        if drop == 0 {
            continue;
        }
        total -= drop;
        drops.insert(target.node, drop);
    }
    if drops.is_empty() {
        return false;
    }
    for tools in run.streamed_tools_by_round.values_mut() {
        for tool in tools {
            apply_live_drops(&mut tool.live, &drops);
        }
    }
    true
}

fn apply_hook_stream_event(run: &mut ModelRunState, event: &ModelStreamEvent, round: usize, event_at: &str) {
    match event {
        ModelStreamEvent::HookExecutionStarted { execution_id, hook_id, hook_name, event: hook_event, status_message, .. } => {
            let hooks = run.streamed_hooks_by_round.entry(round).or_default();
            if hooks.iter().any(|hook| &hook.execution_id == execution_id) {
                return;
            }
            hooks.push(StreamingHookState {
                execution_id: execution_id.clone(),
                hook_id: hook_id.clone(),
                hook_name: hook_name.clone(),
                event: hook_event.clone(),
                status_message: status_message.clone(),
                result: empty_result(event_at),
                status: StreamingHookStatus::Running,
                context_injected: false,
                reason: None,
                created_at: event_at.to_owned(),
            });
        }
        ModelStreamEvent::HookExecutionCompleted { execution_id, hook_id, hook_name, event: hook_event, result, blocked, reason, context_injected, .. } => {
            let hooks = run.streamed_hooks_by_round.entry(round).or_default();
            let status = hook_completion_status(*blocked, result.success);
            if let Some(hook) = hooks.iter_mut().find(|hook| &hook.execution_id == execution_id) {
                hook.result = result.clone();
                hook.status = status;
                hook.context_injected = *context_injected;
                hook.reason = reason.clone();
            } else {
                hooks.push(StreamingHookState {
                    execution_id: execution_id.clone(),
                    hook_id: hook_id.clone(),
                    hook_name: hook_name.clone(),
                    event: hook_event.clone(),
                    status_message: None,
                    result: result.clone(),
                    status,
                    context_injected: *context_injected,
                    reason: reason.clone(),
                    created_at: event_at.to_owned(),
                });
            }
        }
        _ => {}
    }
}

/// Effects emitted after a stream event is folded. Reducers never perform them;
/// the caller applies them to external systems in emission order.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelStreamEffect {
    /// A queued message was delivered during the stream; split the completed turn
    /// before the input using `run_before_input`.
    TurnSplit { item: ContextItem, run_before_input: Box<ModelRunState> },
    /// Clear the delivered message's steering marker.
    SteeringDelivered { context_id: String },
    /// Publish cumulative usage for the active turn.
    TurnUsage { usage: ModelUsage, revision: u64 },
    /// A tool execution completed; the interpreter may trigger related work.
    ToolCompleted { tool_name: Option<String>, success: bool, output: String },
    /// A host-settled tool card replaces its persisted counterpart by id.
    ToolContextSettled { context: ContextItem },
}

/// The duration to record for a round whose reasoning just closed without the
/// sidecar reporting one.
///
/// A settled reasoning card states how long the round thought for, and it states
/// it from `duration_ms` alone — `started_at` is renderer-only and never reaches
/// disk, so a card that kept counting from a start timestamp would read the
/// elapsed time since the conversation was loaded once it came back. Measuring
/// the close here gives every completed round a figure of its own; a sidecar
/// figure still wins, because it times the provider rather than the transport.
///
/// Does nothing when there is no start to measure from, or when a figure is
/// already recorded — reopening reasoning must not restart the clock.
fn close_reasoning_duration(run: &mut ModelRunState, round: usize, event_at: &str) {
    if run.reasoning_duration_by_round.contains_key(&round) {
        return;
    }
    let Some(started_at) = run.reasoning_started_at_by_round.get(&round) else { return };
    let (Some(started), Some(closed)) = (parse_iso(started_at), parse_iso(event_at)) else { return };
    run.reasoning_duration_by_round.insert(round, (closed - started).max(0) as u64);
}

/// Folds one model-stream event into run state. Callers verify the request id and handle approval and ping events
/// before invoking it.
pub fn reduce_model_stream_event(run: &mut ModelRunState, event: &ModelStreamEvent, event_at: &str) -> Vec<ModelStreamEffect> {
    let mut effects = Vec::new();
    let round = event_round(event).unwrap_or(0);

    match event {
        ModelStreamEvent::UserInputReceived { id, content, images, files, created_at, .. } => {
            let item = ContextItem::User { id: id.clone(), content: content.clone(), images: images.clone(), files: files.clone(), created_at: created_at.clone() };
            let duplicate_anywhere = run.steered_inputs_by_round.values().any(|inputs| inputs.iter().any(|input| input.id() == id));
            if !duplicate_anywhere {
                effects.push(ModelStreamEffect::TurnSplit { item: item.clone(), run_before_input: Box::new(run.clone()) });
            }
            effects.push(ModelStreamEffect::SteeringDelivered { context_id: id.clone() });
            let current_inputs = run.steered_inputs_by_round.entry(round).or_default();
            if !current_inputs.iter().any(|input| input.id() == id) {
                current_inputs.push(item);
            }
            return effects;
        }
        ModelStreamEvent::HostContextAdded { context, .. } => {
            // Not reset by a retry: like a hook, it is the host's, and the retried
            // request carries it too.
            let known = run.host_contexts_by_round.values().any(|entries| entries.iter().any(|entry| entry.context.id() == context.id()));
            if known {
                return effects;
            }
            let after_steered = run.steered_inputs_by_round.get(&round).map_or(0, Vec::len);
            run.host_contexts_by_round.entry(round).or_default().push(HostContextState { context: (**context).clone(), after_steered });
            return effects;
        }
        ModelStreamEvent::StreamRetryScheduled { attempt, max_attempts, message, .. } => {
            run.retry = Some(ModelRunRetryState { round, attempt: *attempt, max_attempts: *max_attempts, message: message.clone(), dirty: true });
            return effects;
        }
        _ => {}
    }

    if resets_retry(event) && run.retry.as_ref().is_some_and(|retry| retry.dirty && retry.round == round) {
        reset_round_for_retry(run, round);
        run.retry = None;
        effects.push(ModelStreamEffect::TurnUsage { usage: cumulative_model_run_usage(run), revision: run.usage_revision });
    }

    match event {
        ModelStreamEvent::UsageUpdated { usage, .. } => {
            run.usage_by_round.insert(round, usage.clone());
            run.usage_revision += 1;
            effects.push(ModelStreamEffect::TurnUsage { usage: cumulative_model_run_usage(run), revision: run.usage_revision });
        }
        ModelStreamEvent::ToolCallAnnounced { call_id, tool_name, .. } => {
            let known = run.streamed_tools_by_round.get(&round).is_some_and(|tools| tools.iter().any(|tool| &tool.call_id == call_id));
            // Do not emit completion effects for orphaned events. Replay after the
            // announcement, when the tool name is available.
            if !known {
                let completed = run
                    .pending_tool_events_by_round
                    .get(&round)
                    .and_then(|pending| pending.get(call_id))
                    .and_then(|queue| queue.iter().rev().find_map(|pending| if let ModelStreamEvent::ToolExecutionCompleted { result, .. } = pending { Some(result) } else { None }));
                if let Some(result) = completed {
                    effects.push(ModelStreamEffect::ToolCompleted { tool_name: Some(tool_name.clone()), success: result.success, output: result.output.clone() });
                }
            }
            apply_tool_stream_event(run, event, round, event_at);
        }
        ModelStreamEvent::ToolExecutionCompleted { call_id, result, .. } => {
            if let Some(row) = run.streamed_tools_by_round.get(&round).and_then(|tools| tools.iter().find(|tool| &tool.call_id == call_id)) {
                effects.push(ModelStreamEffect::ToolCompleted { tool_name: Some(row.tool_name.clone()), success: result.success, output: result.output.clone() });
            }
            apply_tool_stream_event(run, event, round, event_at);
        }
        ModelStreamEvent::ToolCallArgumentsReady { .. } | ModelStreamEvent::ToolExecutionStarted { .. } => {
            apply_tool_stream_event(run, event, round, event_at);
        }
        ModelStreamEvent::SubagentEvent { call_id, event: nested, .. } => {
            if let Some(tool) = run.streamed_tools_by_round.get_mut(&round).and_then(|tools| tools.iter_mut().find(|tool| &tool.call_id == call_id)) {
                let id = tool.id.clone();
                apply_nested_subagent_event(Arc::make_mut(&mut tool.live), &id, nested, event_at);
            }
            if let Some((key, nested_round, usage)) = nested_usage_update(event) {
                run.subagent_usage_by_call.entry(key).or_default().insert(nested_round, usage.clone());
                run.usage_revision += 1;
                effects.push(ModelStreamEffect::TurnUsage { usage: cumulative_model_run_usage(run), revision: run.usage_revision });
            }
        }
        ModelStreamEvent::SubagentDelta { call_id, channel, delta, .. } => {
            if let Some(tool) = run.streamed_tools_by_round.get_mut(&round).and_then(|tools| tools.iter_mut().find(|tool| &tool.call_id == call_id)) {
                let id = tool.id.clone();
                apply_subagent_channel_delta(Arc::make_mut(&mut tool.live), &id, *channel, delta, event_at);
            }
        }
        ModelStreamEvent::WorkflowProgress { call_id, run_id, entry, .. } => {
            // The host's row and the renderer's interface are one JSON shape.
            if let Ok(entry) = serde_json::to_value(entry).and_then(serde_json::from_value) {
                apply_workflow_progress(run, call_id, run_id, entry);
            }
        }
        ModelStreamEvent::ToolContextSettled { context, .. } => {
            // Usually only persistence changes. If the completion event was absent,
            // advance a lingering live row to the settled terminal state so it cannot
            // hide the persisted card.
            effects.push(ModelStreamEffect::ToolContextSettled { context: (**context).clone() });
            if let ContextItem::Tool { id, input, result, .. } = &**context {
                for tools in run.streamed_tools_by_round.values_mut() {
                    let open = tools.iter_mut().find(|tool| {
                        &tool.id == id && matches!(tool.stream_status, StreamingToolPhase::Announced | StreamingToolPhase::Ready | StreamingToolPhase::Running)
                    });
                    if let Some(tool) = open {
                        tool.input = input.clone();
                        tool.result = result.clone();
                        tool.stream_status = StreamingToolPhase::Completed;
                        break;
                    }
                }
            }
        }
        ModelStreamEvent::HookExecutionStarted { .. } | ModelStreamEvent::HookExecutionCompleted { .. } => {
            apply_hook_stream_event(run, event, round, event_at);
        }
        ModelStreamEvent::ReasoningStart { item, form, .. } => {
            run.active_reasoning_items_by_round.entry(round).or_default().insert(*item, true);
            run.completed_reasoning_by_round.insert(round, false);
            if let Some(form) = form {
                run.reasoning_forms_by_round.entry(round).or_default().insert(*item, *form);
            }
            // Open an empty slot for this item. Encrypted-only reasoning emits no text,
            // so the slot is its only live-row evidence.
            let items = run.streamed_reasoning_by_round.entry(round).or_default();
            if items.len() <= *item {
                items.resize(*item + 1, String::new());
            }
            // Preserve the first start time. Providers can emit one start per summary
            // segment, but the live clock covers the entire reasoning step.
            if run.reasoning_started_at_by_round.get(&round).is_none_or(String::is_empty) {
                run.reasoning_started_at_by_round.insert(round, event_at.to_owned());
            }
        }
        ModelStreamEvent::ReasoningDone { item, duration_ms, .. } => {
            let opened = run.reasoning_started_at_by_round.contains_key(&round);
            if !opened && run.streamed_reasoning_by_round.get(&round).is_none_or(Vec::is_empty) {
                return effects;
            }
            // Item completion must not close another interleaved item's live surface.
            let active = run.active_reasoning_items_by_round.entry(round).or_default();
            active.insert(*item, false);
            let completed = !active.values().any(|&open| open);
            run.completed_reasoning_by_round.insert(round, completed);
            // The sidecar reports cumulative duration for this step, so replacing
            // the value is idempotent.
            match duration_ms {
                None => close_reasoning_duration(run, round, event_at),
                Some(duration) => {
                    run.reasoning_duration_by_round.insert(round, *duration);
                }
            }
        }
        ModelStreamEvent::ReasoningProgress { item, estimated_tokens, .. } => {
            run.hidden_reasoning_tokens_by_round.entry(round).or_default().insert(*item, *estimated_tokens);
        }
        ModelStreamEvent::ReasoningDelta { item, delta, .. } => {
            if delta.is_empty() {
                return effects;
            }
            run.active_reasoning_items_by_round.entry(round).or_default().insert(*item, true);
            run.completed_reasoning_by_round.insert(round, false);
            let items = run.streamed_reasoning_by_round.entry(round).or_default();
            if items.len() <= *item {
                items.resize(*item + 1, String::new());
            }
            items[*item].push_str(delta);
            // Providers without `reasoning_start` use the first delta as the start.
            if run.reasoning_started_at_by_round.get(&round).is_none_or(String::is_empty) {
                run.reasoning_started_at_by_round.insert(round, event_at.to_owned());
            }
        }
        ModelStreamEvent::TextDelta { delta, .. } => {
            if delta.is_empty() {
                return effects;
            }
            // Text begins after reasoning. A reasoning start timestamp also covers
            // encrypted-only reasoning with no visible text.
            let closes_reasoning = run.streamed_reasoning_by_round.get(&round).is_some_and(|items| !items.is_empty()) || run.reasoning_started_at_by_round.contains_key(&round);
            run.streamed_text_by_round.entry(round).or_default().push_str(delta);
            if closes_reasoning {
                run.completed_reasoning_by_round.insert(round, true);
                run.active_reasoning_items_by_round.insert(round, BTreeMap::new());
                close_reasoning_duration(run, round, event_at);
            }
        }
        _ => {}
    }
    effects
}

/// An event in the commit buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct QueuedModelStreamEvent {
    pub event: ModelStreamEvent,
    /// Arrival timestamp. Batching preserves it instead of using flush time.
    pub at: String,
}

/// Fixed commit cadence: 10 FPS.
pub const MODEL_STREAM_COMMIT_INTERVAL_MS: u64 = 100;

/// Batches model-stream events at a fixed 10 FPS. Every event enters one buffer in arrival order, so batching changes
/// commit frequency only. Decoupling rendering from event arrival bounds projection and Markdown work while
/// preserving every event in each batch.
///
/// The timer itself belongs to whoever owns this: [`Self::push`] says when one must be armed.
#[derive(Debug, Default)]
pub struct ModelStreamCoalescer {
    pending: Vec<QueuedModelStreamEvent>,
    armed: bool,
}

impl ModelStreamCoalescer {
    /// Buffers an event, stamped now. `true` means a commit needs scheduling.
    pub fn push(&mut self, event: ModelStreamEvent) -> bool {
        self.push_at(event, super::util::now_iso())
    }

    pub fn push_at(&mut self, event: ModelStreamEvent, at: String) -> bool {
        self.pending.push(QueuedModelStreamEvent { event, at });
        !std::mem::replace(&mut self.armed, true)
    }

    /// The batch to commit, in arrival order: what a timer tick and an immediate flush both do.
    pub fn take(&mut self) -> Vec<QueuedModelStreamEvent> {
        self.armed = false;
        std::mem::take(&mut self.pending)
    }

    /// Discards uncommitted events after the run ends.
    pub fn dispose(&mut self) {
        self.armed = false;
        self.pending.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}
