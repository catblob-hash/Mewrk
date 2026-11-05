//! Port of `lib/conversationTurns.ts` and of the turn handlers in `App.tsx`.
//!
//! Turns are frontend-only presentation metadata. They are intentionally stored outside `Conversation.contexts` and
//! never participate in a model request.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::capabilities::is_host_authored_user_context;
use super::util::{create_id, iso_from_ms, parse_iso};
use crate::model::{ContextItem, ModelUsage};

pub const CONVERSATION_TURNS_STORAGE_KEY: &str = "mewrk.conversation-turns.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationTurnStatus {
    Running,
    Completed,
    Interrupted,
}

/// Why a turn stopped producing. Carried on the turn so the failure is readable
/// where it happened instead of only in a composer banner that a reload erases —
/// a bare "stopped after 12s" header with no body is the shape of a bug report.
/// It is presentation metadata like the rest of the turn: never a model message,
/// never written into `Conversation.contexts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationTurnError {
    pub message: String,
    pub provider_name: String,
    pub model_name: String,
    pub at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationTurn {
    pub id: String,
    pub request_id: String,
    pub anchor_context_id: String,
    pub model_id: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub status: ConversationTurnStatus,
    pub context_ids: Vec<String>,
    pub usage: ModelUsage,
    /// Usage completed by earlier backend requests in the same logical UI turn.
    pub usage_offset: ModelUsage,
    /// Cumulative backend-run usage when this UI turn began.
    pub usage_baseline: ModelUsage,
    pub usage_revision_at_start: u64,
    /// Backend requests joined by resumable UI-only pauses such as ask_user.
    pub segment_count: u64,
    /// Set when the run behind this turn failed; cleared when the next run starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ConversationTurnError>,
}

pub type ConversationTurns = HashMap<String, Vec<ConversationTurn>>;

fn finite_token_count(value: Option<&Value>) -> Option<u64> {
    let number = value?.as_f64()?;
    if number.is_finite() && number >= 0.0 { Some(number.floor() as u64) } else { None }
}

pub fn normalize_model_usage(value: Option<&Value>) -> ModelUsage {
    let Some(usage) = value.and_then(Value::as_object) else { return ModelUsage::default() };
    ModelUsage {
        input_tokens: finite_token_count(usage.get("inputTokens")),
        cached_input_tokens: finite_token_count(usage.get("cachedInputTokens")),
        output_tokens: finite_token_count(usage.get("outputTokens")),
        total_tokens: finite_token_count(usage.get("totalTokens")),
        reasoning_tokens: None,
    }
}

fn normalize_turn_error(value: Option<&Value>) -> Option<ConversationTurnError> {
    let error = value?.as_object()?;
    // A blank message would render an error card that explains nothing, which is
    // the empty-header failure this field exists to remove. Drop it instead.
    let message = error.get("message")?.as_str()?;
    if message.trim().is_empty() {
        return None;
    }
    let text = |key: &str| error.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    Some(ConversationTurnError { message: message.to_owned(), provider_name: text("providerName"), model_name: text("modelName"), at: text("at") })
}

/// The status as written to disk, before load converts a stale `running` one.
/// `awaiting_user` is what a turn paused on an `ask_user` question used to be
/// stored as, back when the answer arrived as a user message that resumed it.
/// Questions now block inside the turn, so nothing resumes those any more; they
/// read as the unfinished turns they are.
fn stored_turn_status(value: &Value) -> ConversationTurnStatus {
    match value.get("status").and_then(Value::as_str) {
        Some("awaiting_user") | Some("interrupted") => ConversationTurnStatus::Interrupted,
        Some("completed") => ConversationTurnStatus::Completed,
        _ => ConversationTurnStatus::Running,
    }
}

fn normalize_turn(value: &Value, now: i64) -> Option<ConversationTurn> {
    let turn = value.as_object()?;
    let text = |key: &str| turn.get(key).and_then(Value::as_str).map(str::to_owned);
    let (id, request_id, anchor_context_id, model_id, started_at) = (text("id")?, text("requestId")?, text("anchorContextId")?, text("modelId")?, text("startedAt")?);
    let started = parse_iso(&started_at)?;
    let stored_status = stored_turn_status(value);
    let status = if stored_status == ConversationTurnStatus::Running { ConversationTurnStatus::Interrupted } else { stored_status };
    let ended_at = if status == ConversationTurnStatus::Interrupted && stored_status == ConversationTurnStatus::Running {
        Some(iso_from_ms(now))
    } else {
        text("endedAt")
    };
    let stored_duration = finite_token_count(turn.get("durationMs"));
    let active_elapsed = (now - started).max(0) as u64;
    let duration_ms = if stored_status == ConversationTurnStatus::Running {
        stored_duration.unwrap_or(0) + active_elapsed
    } else {
        stored_duration.unwrap_or_else(|| ended_at.as_deref().and_then(parse_iso).map_or(0, |ended| (ended - started).max(0) as u64))
    };
    Some(ConversationTurn {
        id,
        request_id,
        anchor_context_id,
        model_id,
        started_at,
        ended_at,
        duration_ms: Some(duration_ms),
        status,
        context_ids: turn.get("contextIds").and_then(Value::as_array).map(|ids| ids.iter().filter_map(|id| id.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
        usage: normalize_model_usage(turn.get("usage")),
        usage_offset: normalize_model_usage(turn.get("usageOffset")),
        usage_baseline: normalize_model_usage(turn.get("usageBaseline")),
        usage_revision_at_start: finite_token_count(turn.get("usageRevisionAtStart")).unwrap_or(0),
        segment_count: finite_token_count(turn.get("segmentCount")).unwrap_or(1).max(1),
        error: normalize_turn_error(turn.get("error")),
    })
}

/// A round that produced nothing and explains nothing is not a record.
///
/// A stop, a dropped connection or a steering message that lands before the
/// first message leaves a turn owning no context at all. Storing it would make
/// the next run inherit a round the user never got: a bare Send continues the
/// newest unfinished round, so the header would come back carrying the dead
/// round's elapsed time and token counters over output it never produced.
///
/// A live turn is exempt — `running` still has its stream ahead of it. So is one
/// carrying a failure notice, which is the one thing an empty round still has
/// to say.
pub fn turn_leaves_no_record(turn: &ConversationTurn) -> bool {
    turn.status != ConversationTurnStatus::Running && turn.context_ids.is_empty() && turn.error.is_none()
}

/// `true` when anything was dropped, so a list with nothing to drop never churns state.
pub fn drop_empty_turns(turns: &mut Vec<ConversationTurn>) -> bool {
    let before = turns.len();
    turns.retain(|turn| !turn_leaves_no_record(turn));
    turns.len() != before
}

pub fn drop_empty_conversation_turns(turns: &mut ConversationTurns) -> bool {
    let mut changed = false;
    for entries in turns.values_mut() {
        changed |= drop_empty_turns(entries);
    }
    changed
}

/// `loadConversationTurns`, from what the store holds.
pub fn load_conversation_turns(raw: Option<&str>, now: i64) -> ConversationTurns {
    let Some(parsed) = raw.and_then(|raw| serde_json::from_str::<Value>(raw).ok()) else { return ConversationTurns::new() };
    let Some(parsed) = parsed.as_object() else { return ConversationTurns::new() };
    parsed
        .iter()
        .filter_map(|(conversation_id, value)| {
            let turns: Vec<ConversationTurn> = value
                .as_array()?
                .iter()
                .filter_map(|entry| {
                    let turn = normalize_turn(entry, now)?;
                    // A turn stored as `running` belongs to a run the host may still be
                    // advancing. Load marks it interrupted, but adoption resumes it, so
                    // it survives the emptiness rule until that resolves; anything else
                    // that reaches disk empty is residue this load is here to sweep.
                    let adoptable = stored_turn_status(entry) == ConversationTurnStatus::Running;
                    (adoptable || !turn_leaves_no_record(&turn)).then_some(turn)
                })
                .collect();
            (!turns.is_empty()).then(|| (conversation_id.clone(), turns))
        })
        .collect()
}

// `reasoning_tokens` participates in field-wise arithmetic but is a subset of `output_tokens`, not an independent billed amount.
// Omitting it would make `model_usage_equal` miss reasoning-only changes.
fn usage_fields(usage: &ModelUsage) -> [Option<u64>; 5] {
    [usage.input_tokens, usage.cached_input_tokens, usage.output_tokens, usage.total_tokens, usage.reasoning_tokens]
}

fn usage_from_fields(fields: [Option<u64>; 5]) -> ModelUsage {
    let [input_tokens, cached_input_tokens, output_tokens, total_tokens, reasoning_tokens] = fields;
    ModelUsage { input_tokens, cached_input_tokens, output_tokens, total_tokens, reasoning_tokens }
}

pub fn sum_model_usage(usages: &[ModelUsage]) -> ModelUsage {
    let mut result = [None; 5];
    for usage in usages {
        for (slot, value) in result.iter_mut().zip(usage_fields(usage)) {
            if let Some(value) = value {
                *slot = Some(slot.unwrap_or(0u64) + value);
            }
        }
    }
    usage_from_fields(result)
}

pub fn subtract_model_usage(total: &ModelUsage, baseline: &ModelUsage) -> ModelUsage {
    let mut result = usage_fields(total);
    for (slot, base) in result.iter_mut().zip(usage_fields(baseline)) {
        if let Some(value) = slot {
            *value = value.saturating_sub(base.unwrap_or(0));
        }
    }
    usage_from_fields(result)
}

/// Field-by-field equality, so an absent counter never reads as a changed one.
pub fn model_usage_equal(left: &ModelUsage, right: &ModelUsage) -> bool {
    usage_fields(left) == usage_fields(right)
}

pub fn sum_usage_by_round(usage_by_round: &BTreeMap<usize, ModelUsage>) -> ModelUsage {
    let usages: Vec<ModelUsage> = usage_by_round.values().cloned().collect();
    sum_model_usage(&usages)
}

/// Anchor value for a turn whose output begins at the very start of the timeline.
///
/// A turn is anchored *after* a context, so once every context that preceded it
/// has been deleted there is no id left to name. Dropping the turn instead would
/// erase a round the user really ran, along with the duration and token counts
/// that are the only record of what it cost.
pub const TIMELINE_START_ANCHOR: &str = "";

fn index_map<'a>(ids: impl IntoIterator<Item = &'a str>) -> HashMap<&'a str, isize> {
    ids.into_iter().enumerate().map(|(index, id)| (id, index as isize)).collect()
}

/// Index of the context a turn begins after, or `None` once its anchor was deleted.
pub fn turn_anchor_index(anchor_context_id: &str, indexes: &HashMap<&str, isize>) -> Option<isize> {
    if anchor_context_id == TIMELINE_START_ANCHOR { Some(-1) } else { indexes.get(anchor_context_id).copied() }
}

fn turn_time(turn: &ConversationTurn) -> i64 {
    parse_iso(&turn.started_at).unwrap_or(0)
}

/// Recomputes only the live request's presentation spans. Historical records
/// retain explicit ids so later manual timeline edits cannot expand a turn.
pub fn materialize_run_turn_contexts_ids(turns: &mut [ConversationTurn], context_ids: &[&str], request_id: &str) {
    let mut request_turns: Vec<usize> = (0..turns.len()).filter(|&index| turns[index].request_id == request_id).collect();
    if request_turns.is_empty() {
        return;
    }
    request_turns.sort_by_key(|&index| turn_time(&turns[index]));
    let indexes = index_map(context_ids.iter().copied());
    let anchors: Vec<Option<isize>> = request_turns.iter().map(|&index| turn_anchor_index(&turns[index].anchor_context_id, &indexes)).collect();
    for (position, &index) in request_turns.iter().enumerate() {
        let Some(anchor_index) = anchors[position] else { continue };
        let next_anchor = anchors[position + 1..].iter().flatten().copied().find(|&candidate| candidate > anchor_index);
        let end = next_anchor.map_or(context_ids.len(), |next| next as usize);
        let start = ((anchor_index + 1) as usize).min(end);
        turns[index].context_ids = context_ids[start..end].iter().map(|id| (*id).to_owned()).collect();
    }
}

/// The same over contexts.
pub fn materialize_run_turn_contexts(turns: &mut [ConversationTurn], contexts: &[ContextItem], request_id: &str) {
    let ids: Vec<&str> = contexts.iter().map(ContextItem::id).collect();
    materialize_run_turn_contexts_ids(turns, &ids, request_id);
}

/// Re-place an anchor whose context the user deleted.
///
/// The anchor only records where a turn's output begins, so the honest
/// replacement is the context immediately before the first message the turn
/// still owns — or the timeline tail when the deletion took all of them. Without
/// this a turn can never claim another context after losing its anchor, so the
/// run that continues it streams into a headerless timeline.
pub fn repair_turn_anchor(turn: &ConversationTurn, contexts: &[ContextItem]) -> String {
    let indexes = index_map(contexts.iter().map(ContextItem::id));
    if turn_anchor_index(&turn.anchor_context_id, &indexes).is_some() {
        return turn.anchor_context_id.clone();
    }
    let boundary = turn.context_ids.iter().filter_map(|id| indexes.get(id.as_str()).copied()).min().map_or(contexts.len(), |index| index as usize);
    if boundary > 0 { contexts[boundary - 1].id().to_owned() } else { TIMELINE_START_ANCHOR.to_owned() }
}

/// A round ends on a new user message or on a normal final reply. Anything else
/// — a stop, a dropped connection — leaves it unfinished,
/// and a later run that carries no user message continues it rather than opening
/// a round of its own.
fn is_unfinished_turn(turn: &ConversationTurn) -> bool {
    turn.status == ConversationTurnStatus::Interrupted
}

/// Index of the last timeline position this turn accounts for: the newest
/// message it still owns, or its anchor when the deletion took all of them.
fn turn_boundary_index(turn: &ConversationTurn, contexts: &[ContextItem]) -> Option<isize> {
    let indexes = index_map(contexts.iter().map(ContextItem::id));
    turn.context_ids.iter().filter_map(|id| indexes.get(id.as_str()).copied()).max().or_else(|| turn_anchor_index(&turn.anchor_context_id, &indexes))
}

/// The turn a run carrying no new user message continues.
///
/// That is the bare Send and the task wake. It only ever continues the
/// newest round, and only while that round is genuinely unfinished — a user
/// message sitting after everything the turn owns already closed it, so the run
/// belongs to the round that message started instead. A round that produced
/// nothing is nothing to continue: it leaves no record (`turn_leaves_no_record`),
/// and one that outlived a reload as an unadopted host run must not merge
/// either, or the new round would open carrying its clock.
pub fn find_resumable_turn<'a>(turns: &'a [ConversationTurn], contexts: &[ContextItem]) -> Option<&'a ConversationTurn> {
    let last = turns.last()?;
    if !is_unfinished_turn(last) || last.context_ids.is_empty() {
        return None;
    }
    let boundary = turn_boundary_index(last, contexts).unwrap_or(contexts.len() as isize - 1);
    let closed = contexts.iter().enumerate().any(|(index, context)| index as isize > boundary && matches!(context, ContextItem::User { .. }) && !is_host_authored_user_context(context));
    if closed { None } else { Some(last) }
}

/// Hand an unfinished turn to a new backend request.
///
/// Its span against the old request is frozen first, then it carries its own
/// totals forward: elapsed time keeps accumulating from `duration_ms`, and
/// `usage_offset` makes the new request's input, cached-input and output counts
/// add to what the round already spent instead of replacing it.
pub fn resume_conversation_turn(turns: &mut [ConversationTurn], contexts: &[ContextItem], target_id: &str, request_id: &str, model_id: &str, started_at: &str) {
    let Some(old_request) = turns.iter().find(|turn| turn.id == target_id).map(|turn| turn.request_id.clone()) else { return };
    materialize_run_turn_contexts(turns, contexts, &old_request);
    let Some(turn) = turns.iter_mut().find(|turn| turn.id == target_id) else { return };
    // The round is live again, so its terminal stamp and the notice explaining
    // why it stopped are both stale; settlement records new ones.
    turn.ended_at = None;
    turn.error = None;
    turn.anchor_context_id = repair_turn_anchor(turn, contexts);
    turn.request_id = request_id.to_owned();
    turn.model_id = model_id.to_owned();
    turn.started_at = started_at.to_owned();
    turn.status = ConversationTurnStatus::Running;
    turn.usage_offset = turn.usage.clone();
    turn.usage_baseline = ModelUsage::default();
    turn.usage_revision_at_start = 0;
    turn.segment_count += 1;
}

/// Record why a run failed on the turn it failed in. The last turn of the
/// request owns it: an `ask_user` pause splits one request across several turns,
/// and only the segment that was still running when the run broke should carry
/// the notice. `false` when the request has no turn.
pub fn annotate_turn_failure(turns: &mut [ConversationTurn], request_id: &str, error: ConversationTurnError) -> bool {
    match turns.iter_mut().rev().find(|turn| turn.request_id == request_id) {
        Some(turn) => {
            turn.error = Some(error);
            true
        }
        None => false,
    }
}

/// Drop failure notices and the header-only turns that existed solely to carry
/// them. Sending again is the user retracting the last failure, so a run that
/// produced nothing at all leaves no trace; a run that produced output keeps its
/// output and loses only the notice. `false` when nothing failed, so state never churns.
pub fn clear_turn_failures(turns: &mut Vec<ConversationTurn>) -> bool {
    let mut changed = false;
    for turn in turns.iter_mut() {
        if turn.error.take().is_some() {
            changed = true;
        }
    }
    // Stripping the notice can be what empties a turn out, and an emptied round
    // is not a record — the same rule that sweeps a stop with nothing to show.
    if changed {
        drop_empty_turns(turns);
    }
    changed
}

/// `activeTurnSegmentDuration`.
pub fn active_turn_segment_duration(turn: &ConversationTurn, ended_at: &str) -> u64 {
    (parse_iso(ended_at).unwrap_or(0) - parse_iso(&turn.started_at).unwrap_or(0)).max(0) as u64
}

/// `startConversationTurn` of `App.tsx`: opens a turn anchored at a context, closing whatever was still running.
pub fn start_conversation_turn(
    existing: &mut Vec<ConversationTurn>,
    request_id: &str,
    anchor_context_id: &str,
    model_id: &str,
    started_at: &str,
    contexts: &[ContextItem],
) {
    if existing.iter().any(|turn| turn.request_id == request_id && turn.anchor_context_id == anchor_context_id) {
        return;
    }
    let anchor_index = contexts.iter().position(|context| context.id() == anchor_context_id);
    let previous_contexts = anchor_index.map_or(contexts, |index| &contexts[..index]);
    let mut open_requests: Vec<String> = existing.iter().filter(|turn| turn.status == ConversationTurnStatus::Running).map(|turn| turn.request_id.clone()).collect();
    open_requests.dedup();
    for open in &open_requests {
        materialize_run_turn_contexts(existing, previous_contexts, open);
    }
    for turn in existing.iter_mut().filter(|turn| turn.status == ConversationTurnStatus::Running) {
        let duration = turn.duration_ms.unwrap_or(0) + active_turn_segment_duration(turn, started_at);
        turn.status = ConversationTurnStatus::Interrupted;
        turn.ended_at = Some(started_at.to_owned());
        turn.duration_ms = Some(duration);
    }
    existing.push(ConversationTurn {
        id: create_id("ui-turn"),
        request_id: request_id.to_owned(),
        anchor_context_id: anchor_context_id.to_owned(),
        model_id: model_id.to_owned(),
        started_at: started_at.to_owned(),
        ended_at: None,
        duration_ms: Some(0),
        status: ConversationTurnStatus::Running,
        context_ids: Vec::new(),
        usage: ModelUsage::default(),
        usage_offset: ModelUsage::default(),
        usage_baseline: ModelUsage::default(),
        usage_revision_at_start: 0,
        segment_count: 1,
        error: None,
    });
}

/// `updateRunningTurnUsage`.
pub fn update_running_turn_usage(existing: &mut [ConversationTurn], request_id: &str, cumulative_usage: &ModelUsage, usage_revision: u64) -> bool {
    let mut changed = false;
    for turn in existing.iter_mut() {
        if turn.request_id != request_id || turn.status != ConversationTurnStatus::Running || usage_revision <= turn.usage_revision_at_start {
            continue;
        }
        turn.usage = sum_model_usage(&[turn.usage_offset.clone(), subtract_model_usage(cumulative_usage, &turn.usage_baseline)]);
        changed = true;
    }
    changed
}

/// `resumeAdoptedConversationTurn`: restores an adopted same-generation turn to running. `startedAt` resets because
/// load conversion already accounted for its prior duration, preventing double-counting.
pub fn resume_adopted_conversation_turn(existing: &mut Vec<ConversationTurn>, request_id: &str, contexts: &[ContextItem], model_id: &str, resumed_at: &str) {
    if !existing.iter().any(|turn| turn.request_id == request_id) {
        let anchor = contexts.last().map_or(TIMELINE_START_ANCHOR, ContextItem::id);
        start_conversation_turn(existing, request_id, anchor, model_id, resumed_at, contexts);
        return;
    }
    let adopted = existing.iter_mut().rev().find(|turn| turn.request_id == request_id && turn.status == ConversationTurnStatus::Interrupted);
    if let Some(turn) = adopted {
        // The run is live again, so the failure recorded when loading marked it
        // interrupted is stale. Its settlement re-records one if it still applies.
        turn.ended_at = None;
        turn.error = None;
        turn.status = ConversationTurnStatus::Running;
        turn.started_at = resumed_at.to_owned();
    }
}

/// `continueConversationTurn`: attaches a run that carries no new user message to a round.
///
/// A round ends on a new user message or on a normal final reply, so a bare
/// Send after a stop, a retry, and a task wake all belong to the round they
/// follow: the resumed turn keeps accumulating its elapsed time and its input,
/// cached-input and output counts. When there is nothing to continue the run
/// still opens a turn of its own, anchored at the timeline tail — a round with
/// no header is a round whose cost the user cannot read.
pub fn continue_conversation_turn(existing: &mut Vec<ConversationTurn>, request_id: &str, model_id: &str, contexts: &[ContextItem], resumed_at: &str) {
    if let Some(target) = find_resumable_turn(existing, contexts).map(|turn| turn.id.clone()) {
        resume_conversation_turn(existing, contexts, &target, request_id, model_id, resumed_at);
        return;
    }
    let anchor = contexts.last().map_or(TIMELINE_START_ANCHOR, ContextItem::id);
    start_conversation_turn(existing, request_id, anchor, model_id, resumed_at, contexts);
}

/// `splitConversationTurn`: a steered message closes the running segment and opens the next, within one request.
#[allow(clippy::too_many_arguments)]
pub fn split_conversation_turn(
    existing: &mut Vec<ConversationTurn>,
    projected: &[ContextItem],
    request_id: &str,
    model_name: &str,
    usage_revision: u64,
    cumulative_usage: &ModelUsage,
    next_anchor_id: &str,
    ended_at: &str,
) {
    materialize_run_turn_contexts(existing, projected, request_id);
    for turn in existing.iter_mut() {
        if turn.request_id != request_id || turn.status != ConversationTurnStatus::Running {
            continue;
        }
        if usage_revision > turn.usage_revision_at_start {
            turn.usage = sum_model_usage(&[turn.usage_offset.clone(), subtract_model_usage(cumulative_usage, &turn.usage_baseline)]);
        }
        turn.duration_ms = Some(turn.duration_ms.unwrap_or(0) + active_turn_segment_duration(turn, ended_at));
        turn.status = ConversationTurnStatus::Interrupted;
        turn.ended_at = Some(ended_at.to_owned());
    }
    existing.push(ConversationTurn {
        id: create_id("ui-turn"),
        request_id: request_id.to_owned(),
        anchor_context_id: next_anchor_id.to_owned(),
        model_id: model_name.to_owned(),
        started_at: ended_at.to_owned(),
        ended_at: None,
        duration_ms: Some(0),
        status: ConversationTurnStatus::Running,
        context_ids: Vec::new(),
        usage: ModelUsage::default(),
        usage_offset: ModelUsage::default(),
        usage_baseline: cumulative_usage.clone(),
        usage_revision_at_start: usage_revision,
        segment_count: 1,
        error: None,
    });
}

/// `finishConversationTurns`: settles the request's running segment. `false` when none was running.
pub fn finish_conversation_turns(
    existing: &mut Vec<ConversationTurn>,
    request_id: &str,
    contexts: &[ContextItem],
    status: ConversationTurnStatus,
    meta: &super::types::TurnCompletionMeta,
    ended_at: &str,
) -> bool {
    let mut materialized = existing.clone();
    materialize_run_turn_contexts(&mut materialized, contexts, request_id);
    let request_turns: Vec<&ConversationTurn> = materialized.iter().filter(|turn| turn.request_id == request_id).collect();
    let Some(running) = request_turns.iter().rev().find(|turn| turn.status == ConversationTurnStatus::Running).copied() else { return false };
    let running_id = running.id.clone();
    let previous: Vec<ModelUsage> = request_turns.iter().filter(|turn| turn.id != running_id).map(|turn| subtract_model_usage(&turn.usage, &turn.usage_offset)).collect();
    let previous_run_usage = sum_model_usage(&previous);
    let current_segment_usage = match &meta.usage {
        Some(usage) => subtract_model_usage(usage, &previous_run_usage),
        None => subtract_model_usage(&running.usage, &running.usage_offset),
    };
    let terminal_usage = sum_model_usage(&[running.usage_offset.clone(), current_segment_usage]);
    let single_segment = request_turns.len() == 1;
    let turn = materialized.iter_mut().find(|turn| turn.id == running_id).expect("found above");
    let segment_duration = match meta.duration_ms {
        Some(duration) if single_segment => duration,
        _ => active_turn_segment_duration(turn, ended_at),
    };
    if let Some(model_id) = meta.model_id.as_ref().filter(|id| !id.is_empty()) {
        turn.model_id = model_id.clone();
    }
    turn.status = status;
    turn.ended_at = Some(ended_at.to_owned());
    turn.duration_ms = Some(turn.duration_ms.unwrap_or(0) + segment_duration);
    turn.usage = terminal_usage;
    *existing = materialized;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::types::TurnCompletionMeta;
    use serde_json::json;

    const STARTED: &str = "2026-07-20T00:00:00.000Z";

    fn user(id: &str) -> ContextItem {
        ContextItem::User { id: id.into(), content: id.into(), images: vec![], files: vec![], created_at: STARTED.into() }
    }

    fn assistant(id: &str) -> ContextItem {
        ContextItem::Assistant { id: id.into(), content: id.into(), round: None, model_turn_id: None, interrupted: false, sources: vec![], created_at: STARTED.into() }
    }

    fn turn(id: &str, request_id: &str, anchor: &str) -> ConversationTurn {
        ConversationTurn {
            id: id.into(),
            request_id: request_id.into(),
            anchor_context_id: anchor.into(),
            model_id: "model".into(),
            started_at: STARTED.into(),
            ended_at: None,
            duration_ms: Some(0),
            status: ConversationTurnStatus::Running,
            context_ids: vec![],
            usage: ModelUsage::default(),
            usage_offset: ModelUsage::default(),
            usage_baseline: ModelUsage::default(),
            usage_revision_at_start: 0,
            segment_count: 1,
            error: None,
        }
    }

    fn usage(input: u64, cached: Option<u64>, output: Option<u64>) -> ModelUsage {
        ModelUsage { input_tokens: Some(input), cached_input_tokens: cached, output_tokens: output, ..Default::default() }
    }

    #[test]
    fn sums_provider_usage_fields_independently_and_subtracts_a_clamped_baseline() {
        assert_eq!(
            sum_model_usage(&[usage(10, Some(2), None), ModelUsage { input_tokens: Some(5), output_tokens: Some(3), ..Default::default() }, ModelUsage::default()]),
            ModelUsage { input_tokens: Some(15), cached_input_tokens: Some(2), output_tokens: Some(3), ..Default::default() }
        );
        assert_eq!(subtract_model_usage(&usage(10, None, Some(2)), &usage(12, None, Some(1))), ModelUsage { input_tokens: Some(0), output_tokens: Some(1), ..Default::default() });
        // A counter the total lacks stays absent rather than reading as zero.
        assert!(!model_usage_equal(&ModelUsage::default(), &ModelUsage { input_tokens: Some(0), ..Default::default() }));
    }

    #[test]
    fn materializes_every_live_turn_from_its_anchor_up_to_the_next_anchor() {
        let contexts = [user("u1"), assistant("a1"), user("u2"), assistant("a2"), assistant("a3")];
        let mut turns = vec![turn("t1", "r", "u1"), ConversationTurn { started_at: "2026-07-20T00:00:01.000Z".into(), ..turn("t2", "r", "u2") }, turn("other", "other", "u1")];
        materialize_run_turn_contexts(&mut turns, &contexts, "r");
        assert_eq!(turns[0].context_ids, ["a1", "u2"].map(str::to_owned)[..1]);
        assert_eq!(turns[1].context_ids, ["a2", "a3"]);
        // Another request's span is frozen.
        assert!(turns[2].context_ids.is_empty());
    }

    #[test]
    fn recovers_a_stale_persisted_running_turn_as_an_interruption() {
        let now = parse_iso("2026-07-20T00:00:10.000Z").unwrap();
        let raw = json!({ "c": [{
            "id": "t", "requestId": "r", "anchorContextId": "u", "modelId": "m", "startedAt": STARTED,
            "status": "running", "durationMs": 1000, "contextIds": ["a"], "usage": {"inputTokens": 3, "outputTokens": "bad"},
            "usageOffset": {}, "usageBaseline": {}, "usageRevisionAtStart": 2
        }]})
        .to_string();
        let loaded = load_conversation_turns(Some(&raw), now);
        let turn = &loaded["c"][0];
        assert_eq!(turn.status, ConversationTurnStatus::Interrupted);
        assert_eq!(turn.ended_at.as_deref(), Some("2026-07-20T00:00:10.000Z"));
        assert_eq!(turn.duration_ms, Some(11_000));
        assert_eq!(turn.usage, ModelUsage { input_tokens: Some(3), ..Default::default() });
        assert_eq!(turn.segment_count, 1);
    }

    #[test]
    fn reads_a_turn_persisted_as_paused_on_an_old_ask_user_as_the_unfinished_turn_it_is() {
        let raw = json!({ "c": [{
            "id": "t", "requestId": "r", "anchorContextId": "u", "modelId": "m", "startedAt": STARTED,
            "endedAt": "2026-07-20T00:00:05.000Z", "status": "awaiting_user", "contextIds": ["a"], "usage": {}, "usageOffset": {}, "usageBaseline": {}
        }]})
        .to_string();
        let loaded = load_conversation_turns(Some(&raw), parse_iso("2026-07-21T00:00:00.000Z").unwrap());
        assert_eq!(loaded["c"][0].status, ConversationTurnStatus::Interrupted);
        assert_eq!(loaded["c"][0].duration_ms, Some(5_000));
    }

    #[test]
    fn records_a_failure_on_the_requests_last_turn_so_a_split_request_blames_the_running_segment() {
        let mut turns = vec![turn("a", "r", "u1"), turn("b", "r", "u2")];
        let error = ConversationTurnError { message: "boom".into(), provider_name: "p".into(), model_name: "m".into(), at: STARTED.into() };
        assert!(annotate_turn_failure(&mut turns, "r", error.clone()));
        assert_eq!(turns[0].error, None);
        assert_eq!(turns[1].error, Some(error.clone()));
        // Untouched when the request has no turn.
        assert!(!annotate_turn_failure(&mut turns, "missing", error));
    }

    #[test]
    fn drops_a_header_only_failed_turn_on_clear_but_keeps_one_that_produced_output() {
        let error = ConversationTurnError { message: "boom".into(), provider_name: "p".into(), model_name: "m".into(), at: STARTED.into() };
        let finished = |id: &str, context_ids: Vec<String>| ConversationTurn { status: ConversationTurnStatus::Interrupted, context_ids, error: Some(error.clone()), ..turn(id, "r", "u") };
        let mut turns = vec![finished("empty", vec![]), finished("output", vec!["a".into()])];
        assert!(clear_turn_failures(&mut turns));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].id, "output");
        assert_eq!(turns[0].error, None);
    }

    #[test]
    fn keeps_an_empty_but_still_running_turn_which_has_yet_to_produce_anything() {
        let mut turns = vec![turn("t", "r", "u")];
        assert!(!drop_empty_turns(&mut turns));
        assert_eq!(turns.len(), 1);
    }

    #[test]
    fn reports_no_change_when_nothing_failed_so_state_never_churns() {
        let mut turns = vec![turn("t", "r", "u")];
        assert!(!clear_turn_failures(&mut turns));
    }

    #[test]
    fn round_trips_a_failure_through_storage_and_drops_one_with_no_message() {
        let error = ConversationTurnError { message: "boom".into(), provider_name: "p".into(), model_name: "m".into(), at: STARTED.into() };
        let stored = ConversationTurn { status: ConversationTurnStatus::Interrupted, error: Some(error.clone()), ..turn("t", "r", "u") };
        let raw = serde_json::to_string(&HashMap::from([("c", vec![stored.clone()])])).unwrap();
        assert_eq!(load_conversation_turns(Some(&raw), 0)["c"][0].error, Some(error));
        let mut blank = serde_json::to_value(&stored).unwrap();
        blank["error"]["message"] = json!("   ");
        blank["contextIds"] = json!(["a"]);
        let raw = json!({ "c": [blank] }).to_string();
        assert_eq!(load_conversation_turns(Some(&raw), 0)["c"][0].error, None);
    }

    #[test]
    fn drops_only_the_rounds_with_nothing_to_show_per_conversation() {
        let stopped = |id: &str, ids: Vec<String>| ConversationTurn { status: ConversationTurnStatus::Interrupted, context_ids: ids, ..turn(id, "r", "u") };
        let mut turns = ConversationTurns::from([("a".to_owned(), vec![stopped("kept", vec!["x".into()]), stopped("gone", vec![])]), ("b".to_owned(), vec![stopped("k", vec!["y".into()])])]);
        assert!(drop_empty_conversation_turns(&mut turns));
        assert_eq!(turns["a"].len(), 1);
        assert_eq!(turns["b"].len(), 1);
        assert!(!drop_empty_conversation_turns(&mut turns));
    }

    #[test]
    fn sweeps_a_stored_empty_round_but_keeps_one_a_host_run_may_still_be_advancing() {
        let stored = |id: &str, status: &str| json!({
            "id": id, "requestId": "r", "anchorContextId": "u", "modelId": "m", "startedAt": STARTED,
            "status": status, "contextIds": [], "usage": {}, "usageOffset": {}, "usageBaseline": {}
        });
        let raw = json!({ "c": [stored("dead", "interrupted"), stored("live", "running")] }).to_string();
        let loaded = load_conversation_turns(Some(&raw), parse_iso("2026-07-20T00:00:10.000Z").unwrap());
        assert_eq!(loaded["c"].iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["live"]);
    }

    #[test]
    fn an_empty_round_is_not_one_to_continue_so_a_bare_send_opens_its_own() {
        let turns = vec![ConversationTurn { status: ConversationTurnStatus::Interrupted, ..turn("t", "r", "u") }];
        assert!(find_resumable_turn(&turns, &[user("u")]).is_none());
    }

    fn stopped(ids: &[&str], anchor: &str) -> ConversationTurn {
        ConversationTurn {
            status: ConversationTurnStatus::Interrupted,
            context_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            ended_at: Some("2026-07-20T00:00:05.000Z".into()),
            duration_ms: Some(5_000),
            usage: ModelUsage { input_tokens: Some(100), cached_input_tokens: Some(40), output_tokens: Some(10), ..Default::default() },
            segment_count: 1,
            ..turn("t", "r", anchor)
        }
    }

    #[test]
    fn keeps_an_anchor_that_is_still_in_the_timeline() {
        assert_eq!(repair_turn_anchor(&stopped(&["a"], "u"), &[user("u"), assistant("a")]), "u");
    }

    #[test]
    fn re_anchors_to_the_context_before_the_first_message_the_turn_still_owns() {
        assert_eq!(repair_turn_anchor(&stopped(&["a"], "gone"), &[user("x"), assistant("a")]), "x");
    }

    #[test]
    fn re_anchors_to_the_timeline_tail_once_the_whole_round_is_deleted() {
        assert_eq!(repair_turn_anchor(&stopped(&["gone"], "gone"), &[user("x"), user("y")]), "y");
    }

    #[test]
    fn anchors_at_the_start_of_the_timeline_when_nothing_precedes_what_it_owns() {
        assert_eq!(repair_turn_anchor(&stopped(&["a"], "gone"), &[assistant("a")]), TIMELINE_START_ANCHOR);
        assert_eq!(repair_turn_anchor(&stopped(&["gone"], "gone"), &[]), TIMELINE_START_ANCHOR);
    }

    #[test]
    fn claims_the_whole_timeline_for_a_turn_anchored_at_its_start() {
        let mut turns = vec![turn("t", "r", TIMELINE_START_ANCHOR)];
        materialize_run_turn_contexts(&mut turns, &[assistant("a"), assistant("b")], "r");
        assert_eq!(turns[0].context_ids, ["a", "b"]);
    }

    #[test]
    fn continues_the_newest_round_while_a_stop_left_it_unfinished() {
        let turns = vec![stopped(&["a"], "u")];
        assert_eq!(find_resumable_turn(&turns, &[user("u"), assistant("a")]).map(|t| t.id.as_str()), Some("t"));
    }

    #[test]
    fn continues_a_round_whose_messages_were_all_deleted_which_is_the_only_trace_left_of_it() {
        let turns = vec![stopped(&["gone"], "u")];
        assert!(find_resumable_turn(&turns, &[user("u")]).is_some());
    }

    #[test]
    fn refuses_to_continue_a_round_a_later_user_message_already_closed() {
        let turns = vec![stopped(&["a"], "u")];
        assert!(find_resumable_turn(&turns, &[user("u"), assistant("a"), user("next")]).is_none());
        // A host-authored user context closes nothing.
        assert!(find_resumable_turn(&turns, &[user("u"), assistant("a"), user("ctx_agent-result_1")]).is_some());
    }

    #[test]
    fn carries_elapsed_time_and_every_token_counter_into_the_continuing_request() {
        let contexts = [user("u"), assistant("a")];
        let mut turns = vec![stopped(&["a"], "u")];
        resume_conversation_turn(&mut turns, &contexts, "t", "r2", "m2", "2026-07-20T00:01:00.000Z");
        let resumed = &turns[0];
        assert_eq!(resumed.request_id, "r2");
        assert_eq!(resumed.model_id, "m2");
        assert_eq!(resumed.status, ConversationTurnStatus::Running);
        assert_eq!(resumed.ended_at, None);
        assert_eq!(resumed.usage_offset, stopped(&["a"], "u").usage);
        assert_eq!(resumed.usage_baseline, ModelUsage::default());
        assert_eq!(resumed.segment_count, 2);
        assert_eq!(resumed.duration_ms, Some(5_000));

        // The new request's counts add to what the round already spent.
        let mut live = turns.clone();
        assert!(update_running_turn_usage(&mut live, "r2", &ModelUsage { input_tokens: Some(1), output_tokens: Some(2), ..Default::default() }, 1));
        assert_eq!(live[0].usage, ModelUsage { input_tokens: Some(101), cached_input_tokens: Some(40), output_tokens: Some(12), ..Default::default() });
        // And elapsed time keeps accumulating.
        assert!(finish_conversation_turns(&mut turns, "r2", &contexts, ConversationTurnStatus::Completed, &TurnCompletionMeta::default(), "2026-07-20T00:01:02.000Z"));
        assert_eq!(turns[0].duration_ms, Some(7_000));
    }

    #[test]
    fn a_new_turn_closes_whatever_was_still_running() {
        let contexts = [user("u1"), assistant("a1"), user("u2")];
        let mut turns = vec![turn("old", "r1", "u1")];
        start_conversation_turn(&mut turns, "r2", "u2", "m", "2026-07-20T00:00:03.000Z", &contexts);
        assert_eq!(turns[0].status, ConversationTurnStatus::Interrupted);
        assert_eq!(turns[0].context_ids, ["a1"]);
        assert_eq!(turns[0].duration_ms, Some(3_000));
        assert_eq!(turns[1].status, ConversationTurnStatus::Running);
        // Starting the same anchor again is a no-op.
        start_conversation_turn(&mut turns, "r2", "u2", "m", "2026-07-20T00:00:04.000Z", &contexts);
        assert_eq!(turns.len(), 2);
    }

    #[test]
    fn a_steered_message_splits_the_request_into_two_turns_with_their_own_usage() {
        let contexts = [user("u1"), assistant("a1")];
        let mut turns = vec![turn("first", "r", "u1")];
        let cumulative = ModelUsage { input_tokens: Some(10), output_tokens: Some(2), ..Default::default() };
        split_conversation_turn(&mut turns, &contexts, "r", "m", 1, &cumulative, "steer", "2026-07-20T00:00:03.000Z");
        assert_eq!(turns[0].status, ConversationTurnStatus::Interrupted);
        assert_eq!(turns[0].usage, cumulative);
        assert_eq!(turns[0].context_ids, ["a1"]);
        assert_eq!(turns[1].anchor_context_id, "steer");
        assert_eq!(turns[1].usage_baseline, cumulative);
        assert_eq!(turns[1].usage_revision_at_start, 1);
        // A snapshot no newer than the split changes nothing on the new segment.
        assert!(!update_running_turn_usage(&mut turns, "r", &cumulative, 1));
        let later = ModelUsage { input_tokens: Some(30), output_tokens: Some(5), ..Default::default() };
        assert!(update_running_turn_usage(&mut turns, "r", &later, 2));
        assert_eq!(turns[1].usage, ModelUsage { input_tokens: Some(20), output_tokens: Some(3), ..Default::default() });
        // The terminal usage subtracts what the earlier segment already claimed.
        let finished = [user("u1"), assistant("a1"), user("steer"), assistant("a2")];
        let meta = TurnCompletionMeta { usage: Some(ModelUsage { input_tokens: Some(40), output_tokens: Some(6), ..Default::default() }), ..Default::default() };
        assert!(finish_conversation_turns(&mut turns, "r", &finished, ConversationTurnStatus::Completed, &meta, "2026-07-20T00:00:09.000Z"));
        assert_eq!(turns[1].usage, ModelUsage { input_tokens: Some(30), output_tokens: Some(4), ..Default::default() });
        assert_eq!(turns[1].context_ids, ["a2"]);
    }

    #[test]
    fn finishing_prefers_the_reported_duration_only_for_a_single_segment_request() {
        let contexts = [user("u"), assistant("a")];
        let mut turns = vec![turn("t", "r", "u")];
        let meta = TurnCompletionMeta { model_id: Some("actual".into()), usage: None, duration_ms: Some(1234) };
        assert!(finish_conversation_turns(&mut turns, "r", &contexts, ConversationTurnStatus::Completed, &meta, "2026-07-20T00:00:09.000Z"));
        assert_eq!(turns[0].duration_ms, Some(1234));
        assert_eq!(turns[0].model_id, "actual");
        // Nothing running: nothing to settle.
        assert!(!finish_conversation_turns(&mut turns, "r", &contexts, ConversationTurnStatus::Interrupted, &meta, "2026-07-20T00:00:10.000Z"));
    }

    #[test]
    fn an_adopted_turn_goes_back_to_running_with_a_fresh_clock() {
        let contexts = [user("u"), assistant("a")];
        let mut turns = vec![ConversationTurn { error: Some(ConversationTurnError { message: "x".into(), provider_name: String::new(), model_name: String::new(), at: String::new() }), ..stopped(&["a"], "u") }];
        resume_adopted_conversation_turn(&mut turns, "r", &contexts, "m", "2026-07-21T00:00:00.000Z");
        assert_eq!(turns[0].status, ConversationTurnStatus::Running);
        assert_eq!(turns[0].started_at, "2026-07-21T00:00:00.000Z");
        assert_eq!(turns[0].error, None);
        // An adopted run with no turn of its own opens one at the tail.
        let mut none = Vec::new();
        resume_adopted_conversation_turn(&mut none, "r", &contexts, "m", "2026-07-21T00:00:00.000Z");
        assert_eq!(none[0].anchor_context_id, "a");
    }
}
