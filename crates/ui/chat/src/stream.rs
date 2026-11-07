//! `ContextStream.tsx`: the conversation timeline.
//!
//! A timeline can hold tens of thousands of cards and the view runs every frame, so only rows near the viewport are built;
//! spacers stand for the rest. Heights start as estimates and become measurements as rows are built, and a row above the
//! reader is laid out at its stored height, clipped, so that what it really measures changes nothing on screen until the
//! scroll position has been corrected for it in the very same frame. Upstream leaned on the browser (`content-visibility`,
//! `overflow-anchor`); this is those two, by hand.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use mewrk_client::model::{ContextItem, ToolCategory, ToolDescriptor};
use mewrk_client::prelude::*;
use mewrk_client::runs::timeline_history::{timeline_clipboard_key, timeline_clipboard_shortcut, timeline_history_key, TimelineClipboardAction, TimelineKeyPress, IS_APPLE_PLATFORM};
use mewrk_client::runs::{ConversationTurn, ConversationTurnStatus, LiveContext, RunView, TaskOverlayView, TimelineHistoryAction};
use mewrk_client::task_tools::is_preview_tool_name;
use mewrk_ui_common::context_menu::{context_menu, ContextMenuAnchor, ContextMenuItem, ContextMenuProps, ContextMenuSection};
use mewrk_ui_common::markdown::{clear_text_selection, estimate_markdown_height, has_text_selection};

use crate::cards::{context_card, insertion_line_view, stock_empty_state, timeline_insert_editor, turn_notices, waiting_indicator};
use crate::drag::{start_session, DockAt, DragGeometry, DragOutcome, DragSession, Look, MarkState, Mover, PressDrag, Slide, Unit, BLOCK_PAD_BOTTOM, BLOCK_PAD_TOP, SLOT_GAP};
use crate::frame::{drawn_slide, extras_key, hash_id, row_key, timeline_state, unit_key, DockRoom, Frame, MarqueeDrag, MenuState, MenuView, PreviousTimeline, Resolved, Settling, Shared, TimelineState, LIVE_BIT};
use crate::inline_text_editor::InsertKind;
use crate::item::Item;
use crate::kit::clipboard_leaf;
use crate::nodes::{build_context_render_nodes, build_context_render_nodes_from, context_visual_length, display_insertion_index, ends_answer_search, project_turns, rendered_context_indexes, RenderNode, TurnProjection};
use crate::orchestration::{is_host_authored_user_context, is_legacy_paused_question};
use crate::props::{ConversationViewProps, OpenInsert, TimelineEditorState};
use crate::question_card::{question_timeline_card, QuestionCardProps};
use crate::rows::{find_item, timeline_block, timeline_rows};
use crate::turn_changes::{message_change_spans, summarize_span_changes, turn_changes, ChangeSpan, TurnChangeSummary};

/// Wide enough to absorb fractional scroll metrics, narrow enough that the user has to actually be at the bottom to
/// re-attach to follow-output.
const SCROLL_BOTTOM_EPSILON: f32 = 16.0;
/// How close to the top or bottom edge a selection drag scrolls the timeline.
const MARQUEE_SCROLL_EDGE: f32 = 32.0;
/// The most a selection drag scrolls the timeline per frame.
const MARQUEE_SCROLL_STEP: f32 = 18.0;
/// How far a press with the selection modifier has to travel before it draws a box.
const MARQUEE_THRESHOLD: f32 = 4.0;
/// `.context-stream { padding: 24px 0 72px }`.
const PAD_TOP: f32 = 24.0;
const PAD_BOTTOM: f32 = 72.0;
/// How far past the viewport rows are built: measured before they arrive, not as they do.
const OVERSCAN: f32 = 600.0;
/// How far past the visible timeline a reflow still animates what it moves.
const REFLOW_REACH: f32 = 600.0;
/// `.context-stream { width: min(820px, ...) }`, and 1180px under `data-wide-messages`.
fn column_width(app: &App) -> f32 {
    if app.doc.global_settings.appearance.wide_messages { 1180.0 } else { 820.0 }
}
/// Copies of a card under the pointer, or set down at the home line, have keys of their own.
const GHOST_SALT: u64 = 0x6807;
const DOCK_SALT: u64 = 0xD0C4;

/// `FOLLOW_GLIDE_RATE`: a line-high step is 90 % covered in about 200 ms. `stepFollowGlide`'s critically damped spring is the
/// scroller's own ([`fluent::ui::glide_step`]): it never overshoots, so from rest it only ever moves down, and an upward step is
/// how the timeline recognises the reader leaving the bottom.
const FOLLOW_GLIDE_RATE: f32 = 18.0;

/// `upwardScrollKeys`, and the rest of what a page scrolls with when nothing editable has the keys.
fn scroll_key_delta(key: Key, viewport: f32, content: f32) -> Option<f32> {
    match key {
        Key::Up => Some(-40.0),
        Key::Down => Some(40.0),
        Key::PageUp => Some(-viewport * 0.9),
        Key::PageDown => Some(viewport * 0.9),
        Key::Home => Some(-content),
        Key::End => Some(content),
        _ => None,
    }
}

fn key_name(key: Key) -> Option<String> {
    match key {
        Key::Char(c) => Some(c.to_string()),
        _ => None,
    }
}

/// A rough first guess at a row's height: wrong by little, and corrected the first time it is built.
fn estimate_node(contexts: &Resolved<'_>, node: &RenderNode, width: f32, message_size: f32) -> f32 {
    match node {
        RenderNode::Context { index } => match contexts.get(*index).item {
            ContextItem::User { content, images, files, .. } => {
                let per_line = ((width * 0.78 - 24.0) / (message_size * 0.56)).max(8.0);
                let lines: f32 = content.split('\n').map(|line| (line.chars().count() as f32 / per_line).ceil().max(1.0)).sum();
                lines.min(10.0) * message_size * 1.58 + 22.0 + if images.is_empty() && files.is_empty() { 0.0 } else { 88.0 }
            }
            ContextItem::Assistant { content, .. } => estimate_markdown_height(content, width * 0.86) + 2.0,
            _ => 32.0,
        },
        RenderNode::Block { entries, .. } => entries.len() as f32 * 32.0 + 4.0,
        RenderNode::Question { .. } => 120.0,
    }
}

/// The parts of a frame's fingerprint, kept apart so that an update can tell what changed. A change to the persisted contexts or
/// to the rest builds the model again in full; one only to the live contexts or to the turns keeps what it can of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FingerprintParts {
    /// The persisted contexts: where they are, how many, and when the conversation last changed.
    persisted: u64,
    /// The run's and the task overlay's revisions, which change with each commit.
    live: u64,
    /// `props.turns`, by address and length.
    turns: u64,
    /// Everything else the model reads: workflow keys, `change_spans`, `streaming`, the language and `props.contexts`.
    rest: u64,
}

impl FingerprintParts {
    fn combined(&self) -> u64 {
        hash_key((self.persisted, self.live, self.turns, self.rest))
    }
}

fn turns_part(props: &ConversationViewProps) -> u64 {
    hash_key(((!props.turns.is_empty()).then(|| Rc::as_ptr(&props.turns) as usize), props.turns.len()))
}

fn rest_part(props: &ConversationViewProps) -> u64 {
    let mut workflow_keys: Vec<&str> = props.workflow_run_by_call.keys().map(String::as_str).collect();
    workflow_keys.sort_unstable();
    hash_key((&workflow_keys, props.change_spans.as_ref().map(|spans| Rc::as_ptr(spans) as usize), props.streaming, mewrk_client::i18n::is_english(), (!props.contexts.is_empty()).then(|| Rc::as_ptr(&props.contexts) as usize)))
}

/// What holds the live contexts a model was built from. The model keeps it, so that the next update can tell which of them changed.
#[derive(Clone, Default)]
enum LiveHolder {
    #[default]
    None,
    Run(Arc<RunView>),
    Overlay(Arc<TaskOverlayView>),
    Contexts(Rc<Vec<LiveContext>>),
}

impl LiveHolder {
    fn contexts(&self) -> &[LiveContext] {
        match self {
            Self::None => &[],
            Self::Run(run) => &run.contexts,
            Self::Overlay(overlay) => &overlay.contexts,
            Self::Contexts(contexts) => contexts,
        }
    }

    /// Whether both hold the very same contexts.
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::None, Self::None) => true,
            (Self::Run(a), Self::Run(b)) => Arc::ptr_eq(a, b),
            (Self::Overlay(a), Self::Overlay(b)) => Arc::ptr_eq(a, b),
            (Self::Contexts(a), Self::Contexts(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// One change span as last summed: the contexts from `lo` up to the user's message at `hi` (or the end), and the list they came to
/// with the node it is drawn after.
#[derive(Clone, Debug, PartialEq)]
struct SpanMemo {
    lo: usize,
    hi: usize,
    summary: Option<(usize, Rc<TurnChangeSummary>)>,
}

/// What a timeline's model was last built from, and the parts of it an update can pick up from: see [`rebuild_tail`].
#[derive(Default)]
pub struct ModelMemo {
    /// The fingerprint's parts as of the last build: `None` before the first, and once the timeline is drawn anew.
    parts: Option<FingerprintParts>,
    /// The live contexts the model was built from, and how many of them stand in the place of a persisted one.
    live: LiveHolder,
    in_place: usize,
    /// Context indexes a turn is anchored at, ascending: `build_context_render_nodes`' `break_after_indexes`.
    anchors: Vec<usize>,
    /// Whether the last change span was withheld as still being written.
    open: bool,
    /// Two contexts' ids hash alike, the same id twice or two that collide: what is kept by id cannot then be updated piece by piece,
    /// and every update is a full one.
    ambiguous: bool,
    /// The messages the user wrote as drawn, by index, ascending: where change spans begin and end.
    boundaries: Vec<usize>,
    /// Every change span, in order. Empty while `props.change_spans` is given.
    spans: Vec<SpanMemo>,
    /// What the layout reads of the turn projection, by node position: how many notices follow a node, and whether the last of them is
    /// a failure, which takes away the gap above the next row.
    notice_marks: Vec<(usize, (usize, bool))>,
    /// Estimated heights by node position, NaN where none has been needed yet, at the width and type size of `estimated_at`.
    estimates: Vec<f32>,
    estimated_at: (u32, u32),
    /// The width and type size of the last layout, when nothing was being dragged in it: a layout that can be continued.
    laid_out_at: Option<(u32, u32)>,
}

impl ModelMemo {
    /// The next update builds everything.
    fn forget(&mut self) {
        self.parts = None;
    }
}

fn layout_key(width: f32, message_size: f32) -> (u32, u32) {
    (width.to_bits(), message_size.to_bits())
}

/// The first row whose stored height is filed under one of `keys`. Heights are kept by hashed node key, and keys can hash alike, so
/// this need not be the row that was measured.
fn first_row_of(state: &TimelineState, keys: &[u64]) -> usize {
    state.node_keys.iter().position(|key| keys.contains(key)).unwrap_or(state.node_keys.len())
}

/// Context indexes a turn is anchored at, ascending and once each.
fn turn_anchor_indexes(turns: &[ConversationTurn], index_of: impl Fn(&str) -> Option<usize>) -> Vec<usize> {
    let mut anchors: Vec<usize> = turns.iter().filter_map(|turn| index_of(&turn.anchor_context_id)).collect();
    anchors.sort_unstable();
    anchors.dedup();
    anchors
}

/// Whether the last change span is withheld: a running round is still writing it.
fn spans_open(props: &ConversationViewProps) -> bool {
    props.streaming || props.turns.iter().any(|turn| turn.status == ConversationTurnStatus::Running)
}

/// The index of the message a node draws, when it is one the user wrote: an answer folded into its question card and a note the host
/// wrote in the user's place are not where the user spoke.
fn user_message_index(node: &RenderNode, contexts: &Resolved<'_>) -> Option<usize> {
    let RenderNode::Context { index } = node else { return None };
    let item = contexts.get(*index);
    (matches!(item.item, ContextItem::User { .. }) && !is_host_authored_user_context(item.item)).then_some(*index)
}

/// The stretches `message_change_spans` cuts after the user's message at `opener` (the start, with none), by index: each runs from
/// just past one message up to the next in `boundaries`, and the last, which a running round is still writing, only when not `open`.
fn span_ranges(opener: Option<usize>, boundaries: &[usize], count: usize, open: bool) -> Vec<(Option<usize>, usize, usize)> {
    let mut ranges = Vec::new();
    let (mut opener, mut lo) = (opener, opener.map_or(0, |at| at + 1));
    for &at in boundaries {
        if lo < at {
            ranges.push((opener, lo, at));
        }
        opener = Some(at);
        lo = at + 1;
    }
    if lo < count && !open {
        ranges.push((opener, lo, count));
    }
    ranges
}

/// What one span's calls changed, and the drawn node holding the newest of its contexts, which is where the span ends on screen.
fn span_summary(span: &ChangeSpan, contexts: &Resolved<'_>, index_of: &impl Fn(&str) -> Option<usize>, node_of: &impl Fn(usize) -> Option<usize>) -> Option<(usize, Rc<TurnChangeSummary>)> {
    let summary = summarize_span_changes(span, |id| index_of(id).map(|index| contexts.get(index)))?;
    let last = span.context_ids.iter().filter_map(|id| index_of(id)).filter_map(node_of).max()?;
    Some((last, Rc::new(summary)))
}

/// What the layout reads of a turn projection, by node position, ascending.
fn notice_marks(projection: &TurnProjection, turns: &[ConversationTurn]) -> Vec<(usize, (usize, bool))> {
    let failed = |at: &usize| turns.get(*at).is_some_and(|turn| turn.error.is_some());
    let mut marks: Vec<_> = projection.notices_after_node.iter().map(|(position, notices)| (*position, (notices.len(), notices.last().is_some_and(failed)))).collect();
    marks.sort_unstable_by_key(|(position, _)| *position);
    marks
}

/// How many lists of changed files follow each node, by position, ascending: all the layout reads of them.
fn change_marks(changes: &HashMap<usize, Vec<Rc<TurnChangeSummary>>>) -> Vec<(usize, usize)> {
    let mut marks: Vec<_> = changes.iter().map(|(position, summaries)| (*position, summaries.len())).collect();
    marks.sort_unstable_by_key(|(position, _)| *position);
    marks
}

/// The first position at which two lists of marks, each ascending by position, say different things.
fn first_mark_difference<T: PartialEq>(a: &[(usize, T)], b: &[(usize, T)]) -> Option<usize> {
    let mut pairs = a.iter().zip(b);
    if let Some(((at_a, mark_a), (at_b, mark_b))) = pairs.find(|((at_a, mark_a), (at_b, mark_b))| at_a != at_b || mark_a != mark_b) {
        return Some(if at_a == at_b && mark_a != mark_b { *at_a } else { (*at_a).min(*at_b) });
    }
    let shorter = a.len().min(b.len());
    a.get(shorter).or(b.get(shorter)).map(|(at, _)| *at)
}

/// The least index in one ascending list and not in the other.
fn first_set_difference(a: &[usize], b: &[usize]) -> Option<usize> {
    let mut pairs = a.iter().zip(b);
    if let Some((at_a, at_b)) = pairs.find(|(at_a, at_b)| at_a != at_b) {
        return Some((*at_a).min(*at_b));
    }
    let shorter = a.len().min(b.len());
    a.get(shorter).or(b.get(shorter)).copied()
}

/// Whether the model reads two live contexts alike. It reads the card and how it streams, not a child's live transcript or a clock.
fn reads_alike(a: &LiveContext, b: &LiveContext) -> bool {
    a.streaming == b.streaming && a.stream_status == b.stream_status && a.item == b.item
}

/// The map of this update's contexts. When the live contexts only grew at the end, and none of the new ones stands for a context the
/// timeline already draws, it is the old map with them added; otherwise it is made anew.
fn next_map(old_map: &[u32], old_live: &[LiveContext], old_in_place: usize, persisted: &[ContextItem], live: &[LiveContext], in_place: usize, context_index: &HashMap<u64, u32>) -> Rc<Vec<u32>> {
    let grown = in_place == old_in_place
        && live.len() >= old_live.len()
        && live.iter().zip(old_live).all(|(new, old)| new.id() == old.id())
        && live[old_live.len()..].iter().all(|context| !context_index.contains_key(&hash_id(context.id())));
    if grown {
        let mut map = Vec::with_capacity(old_map.len() + live.len() - old_live.len());
        map.extend_from_slice(old_map);
        map.extend((old_live.len() as u32..live.len() as u32).map(|at| at | LIVE_BIT));
        return Rc::new(map);
    }
    Rc::new(Resolved::map_with_in_place(persisted, live, in_place))
}

/// The first index at which the contexts differ from what the model was built from, in anything the model reads. The persisted
/// contexts are the same ones: a change to them is not followed piece by piece.
fn first_changed(old_map: &[u32], map: &[u32], old_live: &[LiveContext], live: &[LiveContext]) -> usize {
    let end = old_map.iter().zip(map).position(|(old, new)| old != new).unwrap_or(old_map.len().min(map.len()));
    let differs = |at: &u32| at & LIVE_BIT != 0 && !reads_alike(&old_live[(at & !LIVE_BIT) as usize], &live[(at & !LIVE_BIT) as usize]);
    old_map[..end].iter().position(differs).unwrap_or(end)
}

/// The nearest context before `before` at which a legacy question's search for its answer would stop.
fn answer_search_end(contexts: &Resolved<'_>, before: usize) -> Option<usize> {
    (0..before).rev().find(|&at| ends_answer_search(&contexts.get(at)))
}

/// Where the node pass picks up after `last` with no block open: just past a card or a question (whose answer, if it lies further on,
/// is held aside), or just past a block's last row.
fn resume_index(last: Option<&RenderNode>) -> usize {
    match last {
        None => 0,
        Some(RenderNode::Context { index }) => index + 1,
        Some(RenderNode::Question { entry, .. }) => entry + 1,
        Some(RenderNode::Block { entries, .. }) => entries.iter().map(|entry| entry.index).max().map_or(0, |index| index + 1),
    }
}

/// How an update of the model went.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelUpdate {
    /// Nothing the fingerprint covers changed.
    Unchanged,
    /// Something changed, but nothing the model reads of it: only what points at the new contexts moved on.
    Kept,
    /// What follows the first change was built again, and what precedes it kept.
    Tail,
    /// Everything was built again.
    Full,
}

/// What one frame builds the model from.
struct ModelSource<'a> {
    persisted: &'a [ContextItem],
    live: LiveHolder,
    in_place: usize,
    parts: FingerprintParts,
}

/// Brings the model up to date with a frame's contexts: not at all when the fingerprint says nothing changed, from the first change
/// on when only the live contexts or the turns did ([`rebuild_tail`]), and in full otherwise.
fn update_model(state: &mut TimelineState, source: ModelSource<'_>, props: &ConversationViewProps, width: f32, message_size: f32) -> ModelUpdate {
    let ModelSource { persisted, live: holder, in_place, parts } = source;
    let combined = parts.combined();
    let unchanged = state.fingerprint == combined;
    let live = holder.contexts();
    if unchanged && state.map.is_empty() == (persisted.is_empty() && live.is_empty()) {
        return ModelUpdate::Unchanged;
    }
    state.fingerprint = combined;
    let in_place = in_place.min(live.len());
    let old_holder = std::mem::take(&mut state.memo.live);
    let follows = !unchanged
        && state.memo.parts.is_some_and(|old| old.persisted == parts.persisted && old.rest == parts.rest)
        && !state.memo.ambiguous
        && props.change_spans.is_none()
        && state.drag.as_ref().is_none_or(|drag| drag.session.is_none());
    let selection_before = (follows && verify_enabled()).then(|| state.selection.clone());
    let mut update = None;
    if follows {
        let old = Resolved { persisted, live: old_holder.contexts(), map: state.map.clone() };
        let same_live = old_holder.same(&holder) && state.memo.in_place == in_place;
        update = rebuild_tail(state, persisted, live, in_place, &old, same_live, props, width, message_size);
    }
    let update = match update {
        Some(update) => {
            if let Some(selection) = selection_before {
                verify_model(state, persisted, live, in_place, selection, props, width, message_size);
            }
            update
        }
        None => {
            state.map = Rc::new(Resolved::map_with_in_place(persisted, live, in_place));
            let contexts = Resolved { persisted, live, map: state.map.clone() };
            rebuild_model(state, &contexts, props, width, message_size);
            ModelUpdate::Full
        }
    };
    state.memo.parts = Some(parts);
    state.memo.in_place = in_place;
    drop(old_holder);
    state.memo.live = holder;
    update
}

/// Rebuilds what is derived from the contexts, when they changed.
fn rebuild_model(state: &mut TimelineState, contexts: &Resolved<'_>, props: &ConversationViewProps, width: f32, message_size: f32) {
    let count = contexts.count();
    let mut ambiguous = false;
    let mut context_index = HashMap::with_capacity(count);
    for index in 0..count {
        let id = contexts.get(index).id();
        ambiguous |= context_index.insert(hash_id(id), index as u32).is_some();
    }
    let anchors = turn_anchor_indexes(&props.turns, |id| context_index.get(&hash_id(id)).map(|index| *index as usize));
    let turn_anchors: HashSet<usize> = anchors.iter().copied().collect();
    let views = props.workflow_run_by_call.clone();
    let nodes = build_context_render_nodes(contexts, &turn_anchors, |call| views.contains_key(call));
    let mut node_of_context = vec![u32::MAX; count];
    let mut node_keys = Vec::with_capacity(nodes.len());
    let mut question_answers = HashMap::new();
    for (position, node) in nodes.iter().enumerate() {
        for index in node.indexes() {
            node_of_context[index] = position as u32;
        }
        node_keys.push(hash_id(&node.key(&contexts)));
        if let RenderNode::Question { entry, answer: Some(answer), .. } = node {
            question_answers.insert(contexts.get(*entry).id().to_owned(), contexts.get(*answer).id().to_owned());
        }
    }
    let index_of = |id: &str| context_index.get(&hash_id(id)).map(|index| *index as usize);
    let node_of = |index: usize| node_of_context.get(index).copied().filter(|node| *node != u32::MAX).map(|node| node as usize);
    state.projection = project_turns(&props.turns, index_of, node_of, contexts);

    // The messages the user wrote, as drawn: an answer folded into its question card and a note the host wrote in the user's
    // place are not where the user spoke.
    state.changes_after.clear();
    let items: Vec<Item<'_>> = (0..count).map(|index| contexts.get(index)).collect();
    let boundaries: Vec<usize> = nodes.iter().filter_map(|node| user_message_index(node, contexts)).collect();
    let user_nodes: HashSet<&str> = boundaries.iter().map(|index| contexts.get(*index).id()).collect();
    let open = spans_open(props);
    let owned;
    let spans = match &props.change_spans {
        Some(spans) => spans.as_slice(),
        None => {
            owned = message_change_spans(&items, |item| user_nodes.contains(item.id()), open);
            owned.as_slice()
        }
    };
    // The same spans by index, for an update to pick up from.
    let ranges = if props.change_spans.is_none() { span_ranges(None, &boundaries, count, open) } else { Vec::new() };
    ambiguous |= props.change_spans.is_none() && ranges.len() != spans.len();
    let mut memo_spans = Vec::with_capacity(ranges.len());
    for (at, span) in spans.iter().enumerate() {
        let summary = span_summary(span, contexts, &index_of, &node_of);
        if let Some((last, summary)) = &summary {
            state.changes_after.entry(*last).or_default().push(summary.clone());
        }
        if let Some(&(_, lo, hi)) = ranges.get(at) {
            memo_spans.push(SpanMemo { lo, hi, summary });
        }
    }
    state.rendered = Rc::new(rendered_context_indexes(&nodes));
    state.nodes = Rc::new(nodes);
    state.node_keys = node_keys;
    state.context_index = context_index;
    state.node_of_context = node_of_context;
    state.question_answers = question_answers;
    state.selection.retain(|id| state.context_index.contains_key(&hash_id(id)));
    let memo = &mut state.memo;
    memo.anchors = anchors;
    memo.open = open;
    memo.ambiguous = ambiguous;
    memo.boundaries = boundaries;
    memo.spans = memo_spans;
    memo.notice_marks = notice_marks(&state.projection, &state.turns);
    memo.estimates.clear();
    state.layout_dirty = true;
    relayout(state, contexts, width, message_size);
}

/// The incremental half of [`update_model`], for an update in which only the live contexts or the turns changed. The model is kept up
/// to the first context that differs from what it was built from, and only what that change can reach is built again; what comes out
/// is exactly what [`rebuild_model`] would build. `None`, with nothing touched, for a change it cannot follow.
///
/// - Nodes are kept up to the last one the node pass had closed before the change. A block still open there could take new rows, and
///   a legacy question whose search for its answer runs on into the change could find a different one, so those are built again, as
///   is everything after them.
/// - The index of ids, the node of each context, node keys, answered questions and what is drawn lose what the rebuilt nodes held
///   and take what the new ones hold.
/// - A change span that ends at a message of the user's before the rebuilt nodes keeps its sum; those after it are summed again.
/// - The layout goes on from the first row whose height can differ, when the last layout had nothing dragged in it.
#[allow(clippy::too_many_arguments)]
fn rebuild_tail(state: &mut TimelineState, persisted: &[ContextItem], live: &[LiveContext], in_place: usize, old: &Resolved<'_>, same_live: bool, props: &ConversationViewProps, width: f32, message_size: f32) -> Option<ModelUpdate> {
    let old_count = old.count();
    let map = if same_live { old.map.clone() } else { next_map(&old.map, old.live, state.memo.in_place, persisted, live, in_place, &state.context_index) };
    let contexts = Resolved { persisted, live, map };
    let count = contexts.count();
    let changed = if same_live { old_count } else { first_changed(&old.map, &contexts.map, old.live, live) };

    // ---- the ids from the change on. One whose hash the model already holds before it would make what is kept by id ambiguous.
    let mut tail_ids: HashMap<u64, u32> = HashMap::with_capacity(count - changed);
    for index in changed..count {
        let id = contexts.get(index).id();
        let key = hash_id(id);
        let held_before = state.context_index.get(&key).is_some_and(|at| (*at as usize) < changed);
        if held_before || tail_ids.insert(key, index as u32).is_some() {
            return None;
        }
    }
    let anchors = {
        let index_of = |id: &str| {
            let key = hash_id(id);
            tail_ids.get(&key).or_else(|| state.context_index.get(&key).filter(|at| (**at as usize) < changed)).map(|at| *at as usize)
        };
        turn_anchor_indexes(&props.turns, index_of)
    };
    let open = spans_open(props);
    // A turn anchored somewhere else now splits blocks somewhere else.
    let cut = first_set_difference(&state.memo.anchors, &anchors).map_or(changed, |at| at.min(changed));
    let nodes_changed = cut < old_count || cut < count;

    // ---- which nodes are kept, and where the pass picks up
    let old_nodes = state.nodes.clone();
    let (keep, start, held) = if nodes_changed {
        let mut keep = old_nodes.partition_point(|node| node.first_index() < cut);
        if keep > 0 && matches!(old_nodes[keep - 1], RenderNode::Block { .. }) {
            keep -= 1;
        }
        // A legacy question that has not met the end of its search before the cut may find its answer in what changed.
        if let Some(at) = answer_search_end(&contexts, cut)
            && is_legacy_paused_question(&contexts.get(at))
        {
            match state.node_of_context.get(at) {
                Some(&node) if node != u32::MAX => keep = keep.min(node as usize),
                _ => return None,
            }
        }
        let start = resume_index(keep.checked_sub(1).map(|at| &old_nodes[at]));
        // The one kept question whose answer can lie past the start: the nearest place before it a search stops at.
        let held = answer_search_end(&contexts, start).and_then(|at| {
            let node = *state.node_of_context.get(at)? as usize;
            match old_nodes.get(node)? {
                RenderNode::Question { entry, answer: Some(answer), .. } if *entry == at && *answer >= start => Some((*answer, node)),
                _ => None,
            }
        });
        (keep, start, held)
    } else {
        (old_nodes.len(), count, None)
    };
    let tail = if nodes_changed {
        let views = props.workflow_run_by_call.clone();
        let turn_anchors: HashSet<usize> = anchors.iter().copied().collect();
        build_context_render_nodes_from(&contexts, &turn_anchors, |call| views.contains_key(call), start, held.map(|(answer, _)| answer))
    } else {
        Vec::new()
    };

    // ---- from here on the model changes in place
    if nodes_changed {
        for index in changed..old_count {
            let key = hash_id(old.get(index).id());
            if state.context_index.get(&key) == Some(&(index as u32)) {
                state.context_index.remove(&key);
            }
        }
        state.context_index.extend(tail_ids);
        for node in &old_nodes[keep..] {
            if let RenderNode::Question { entry, answer: Some(_), .. } = node {
                state.question_answers.remove(old.get(*entry).id());
            }
        }
        drop(old_nodes);
        let nodes = Rc::make_mut(&mut state.nodes);
        nodes.truncate(keep);
        state.node_keys.truncate(keep);
        state.node_of_context.truncate(start);
        state.node_of_context.resize(count, u32::MAX);
        let rendered = Rc::make_mut(&mut state.rendered);
        rendered.truncate(rendered.partition_point(|index| *index < start));
        let mut drawn: Vec<usize> = Vec::new();
        if let Some((answer, node)) = held {
            state.node_of_context[answer] = node as u32;
            drawn.push(answer);
        }
        for node in tail {
            let position = nodes.len() as u32;
            for index in node.indexes() {
                state.node_of_context[index] = position;
                drawn.push(index);
            }
            state.node_keys.push(hash_id(&node.key(&&contexts)));
            if let RenderNode::Question { entry, answer: Some(answer), .. } = &node {
                state.question_answers.insert(contexts.get(*entry).id().to_owned(), contexts.get(*answer).id().to_owned());
            }
            nodes.push(node);
        }
        drawn.sort_unstable();
        rendered.extend(drawn);
        state.memo.estimates.truncate(keep);
    }
    // A box adds what was picked out when it began to what it touches, which may be gone by now.
    state.selection.retain(|id| state.context_index.contains_key(&hash_id(id)));
    state.map = contexts.map.clone();
    let index_of = |id: &str| state.context_index.get(&hash_id(id)).map(|index| *index as usize);
    let node_of = |index: usize| state.node_of_context.get(index).copied().filter(|node| *node != u32::MAX).map(|node| node as usize);
    let projection = project_turns(&props.turns, index_of, node_of, &contexts);

    // ---- change spans: those closed at a message before the start keep their sums
    let old_change_marks = change_marks(&state.changes_after);
    let mut changes_moved = false;
    if nodes_changed || open != state.memo.open {
        let memo = &mut state.memo;
        let first = memo.boundaries.partition_point(|at| *at < start);
        let opener = first.checked_sub(1).map(|at| memo.boundaries[at]);
        memo.boundaries.truncate(first);
        memo.boundaries.extend(state.nodes[keep..].iter().filter_map(|node| user_message_index(node, &contexts)));
        let kept = opener.map_or(0, |opener| memo.spans.partition_point(|span| span.hi <= opener));
        let gone = memo.spans.split_off(kept);
        for (opener, lo, hi) in span_ranges(opener, &memo.boundaries[first..], count, open) {
            let key = format!("after:{}", opener.map_or("start", |at| contexts.get(at).id()));
            let span = ChangeSpan { key, context_ids: (lo..hi).map(|index| contexts.get(index).id().to_owned()).collect() };
            let summary = span_summary(&span, &contexts, &index_of, &node_of);
            memo.spans.push(SpanMemo { lo, hi, summary });
        }
        changes_moved = memo.spans[kept..].iter().map(|span| &span.summary).ne(gone.iter().map(|span| &span.summary));
        if changes_moved {
            state.changes_after.clear();
            for span in &memo.spans {
                if let Some((last, summary)) = &span.summary {
                    state.changes_after.entry(*last).or_default().push(summary.clone());
                }
            }
        }
    }

    // ---- the layout, from the first row whose height can have changed
    let marks = notice_marks(&projection, &state.turns);
    let projection_moved = projection != state.projection;
    let mut from = keep;
    if let Some(at) = first_mark_difference(&state.memo.notice_marks, &marks) {
        from = from.min(at);
    }
    if let Some(at) = first_mark_difference(&old_change_marks, &change_marks(&state.changes_after)) {
        from = from.min(at);
    }
    state.projection = projection;
    let memo = &mut state.memo;
    memo.notice_marks = marks;
    memo.anchors = anchors;
    memo.open = open;
    relayout_from(state, &contexts, width, message_size, from);
    Some(if nodes_changed || projection_moved || changes_moved { ModelUpdate::Tail } else { ModelUpdate::Kept })
}

/// `MEWRK_TIMELINE_VERIFY=1`: every incremental update of a timeline's model is checked against a full rebuild of the same inputs,
/// and what differs is printed. A full rebuild per update is what this costs.
fn verify_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MEWRK_TIMELINE_VERIFY").is_ok_and(|value| value.trim() == "1"))
}

/// Builds the model in full beside the one an incremental update left, from the same inputs, and prints how the two differ.
#[allow(clippy::too_many_arguments)]
fn verify_model(state: &TimelineState, persisted: &[ContextItem], live: &[LiveContext], in_place: usize, selection: HashSet<String>, props: &ConversationViewProps, width: f32, message_size: f32) {
    let mut full = TimelineState { selection, heights: state.heights.clone(), turns: state.turns.clone(), ..TimelineState::default() };
    full.map = Rc::new(Resolved::map_with_in_place(persisted, live, in_place));
    let contexts = Resolved { persisted, live, map: full.map.clone() };
    rebuild_model(&mut full, &contexts, props, width, message_size);
    let differences = model_differences(state, &full);
    if !differences.is_empty() {
        eprintln!("timeline {}: the incremental model update differs from a full rebuild:\n  {}", props.timeline_id.as_deref().unwrap_or("timeline"), differences.join("\n  "));
    }
}

/// Where two lists first differ, said briefly: a timeline's lists are long.
fn list_difference<T: PartialEq + std::fmt::Debug>(name: &str, a: &[T], b: &[T]) -> Option<String> {
    if a == b {
        return None;
    }
    let at = a.iter().zip(b).position(|(a, b)| a != b).unwrap_or(a.len().min(b.len()));
    Some(format!("{name}: {} against {} long, first differing at {at}: {:?} against {:?}", a.len(), b.len(), a.get(at), b.get(at)))
}

fn sorted_entries<K: Ord + Clone, V: Clone>(map: impl IntoIterator<Item = (K, V)>) -> Vec<(K, V)> {
    let mut entries: Vec<(K, V)> = map.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// What two timelines' models disagree on, field by field: nothing when one is exactly the other.
fn model_differences(a: &TimelineState, b: &TimelineState) -> Vec<String> {
    let mut out = Vec::new();
    let changes = |state: &TimelineState| sorted_entries(state.changes_after.iter().map(|(at, summaries)| (*at, summaries.iter().map(|summary| (**summary).clone()).collect::<Vec<_>>())));
    let sorted = |state: &TimelineState| {
        let mut ids: Vec<String> = state.selection.iter().cloned().collect();
        ids.sort();
        ids
    };
    let (sorted_a, sorted_b) = (sorted(a), sorted(b));
    out.extend(list_difference("map", &a.map[..], &b.map[..]));
    out.extend(list_difference("nodes", &a.nodes[..], &b.nodes[..]));
    out.extend(list_difference("node keys", &a.node_keys[..], &b.node_keys[..]));
    out.extend(list_difference("context index", &sorted_entries(a.context_index.iter().map(|(k, v)| (*k, *v))), &sorted_entries(b.context_index.iter().map(|(k, v)| (*k, *v)))));
    out.extend(list_difference("node of context", &a.node_of_context[..], &b.node_of_context[..]));
    out.extend(list_difference("question answers", &sorted_entries(a.question_answers.clone()), &sorted_entries(b.question_answers.clone())));
    if a.projection != b.projection {
        out.push(format!("projection: {:?} against {:?}", a.projection, b.projection));
    }
    out.extend(list_difference("changes after", &changes(a), &changes(b)));
    out.extend(list_difference("rendered", &a.rendered[..], &b.rendered[..]));
    out.extend(list_difference("selection", &sorted_a, &sorted_b));
    out.extend(list_difference("row heights", &a.row_h[..], &b.row_h[..]));
    out.extend(list_difference("tops", &a.tops[..], &b.tops[..]));
    if a.total != b.total {
        out.push(format!("total: {} against {}", a.total, b.total));
    }
    out.extend(list_difference("adjust", &sorted_entries(a.adjust.iter().map(|(k, v)| (*k, *v))), &sorted_entries(b.adjust.iter().map(|(k, v)| (*k, *v)))));
    let extras = |state: &TimelineState| {
        let mut rows: Vec<usize> = state.extras_only.iter().copied().collect();
        rows.sort_unstable();
        rows
    };
    out.extend(list_difference("extras only", &extras(a), &extras(b)));
    if a.dock != b.dock || a.flush != b.flush {
        out.push(format!("dock and flush: {:?} {:?} against {:?} {:?}", a.dock, a.flush, b.dock, b.flush));
    }
    out.extend(list_difference("turn anchors", &a.memo.anchors[..], &b.memo.anchors[..]));
    out.extend(list_difference("user messages", &a.memo.boundaries[..], &b.memo.boundaries[..]));
    out.extend(list_difference("change spans", &a.memo.spans[..], &b.memo.spans[..]));
    if a.memo.open != b.memo.open || a.memo.ambiguous != b.memo.ambiguous {
        out.push(format!("open and ambiguous: {} {} against {} {}", a.memo.open, a.memo.ambiguous, b.memo.open, b.memo.ambiguous));
    }
    out.extend(list_difference("notice marks", &a.memo.notice_marks[..], &b.memo.notice_marks[..]));
    out
}

/// A timeline's model outside a frame: the update [`context_stream`] makes, over contexts handed to it rather than read off the app.
/// For tests and benches, which hold an incremental update to what a full rebuild of the same inputs builds.
#[doc(hidden)]
pub struct ModelProbe {
    state: TimelineState,
    width: f32,
    message_size: f32,
}

#[doc(hidden)]
impl ModelProbe {
    pub fn new(width: f32, message_size: f32) -> Self {
        Self { state: TimelineState::default(), width, message_size }
    }

    /// One frame's update. The revisions stand for what a frame fingerprints the persisted and the live contexts by, and the first
    /// `in_place` live contexts are drawn in the place of the persisted ones they stand for. With `full`, any change builds everything
    /// again, as every change did before updates were incremental.
    #[allow(clippy::too_many_arguments)]
    pub fn update(&mut self, persisted: &[ContextItem], persisted_revision: u64, live: &Rc<Vec<LiveContext>>, in_place: usize, live_revision: u64, props: &ConversationViewProps, full: bool) -> ModelUpdate {
        if full {
            self.state.memo.forget();
        }
        self.state.turns = props.turns.clone();
        let parts = FingerprintParts { persisted: hash_key((persisted.as_ptr() as usize, persisted.len(), persisted_revision)), live: live_revision, turns: turns_part(props), rest: rest_part(props) };
        update_model(&mut self.state, ModelSource { persisted, live: LiveHolder::Contexts(live.clone()), in_place, parts }, props, self.width, self.message_size)
    }

    /// The row at `position` measured at `height`, laid out again as the frame that measured it would: from that row on, or in full.
    pub fn measure(&mut self, persisted: &[ContextItem], live: &Rc<Vec<LiveContext>>, position: usize, height: f32, full: bool) {
        let Some(&key) = self.state.node_keys.get(position) else { return };
        self.state.heights.insert(key, height);
        let contexts = Resolved { persisted, live, map: self.state.map.clone() };
        let from = if full { 0 } else { first_row_of(&self.state, &[key]) };
        relayout_from(&mut self.state, &contexts, self.width, self.message_size, from);
    }

    /// The column at another width, as a resize leaves it: nothing is laid out again until something else asks for it.
    pub fn set_width(&mut self, width: f32) {
        self.width = width;
    }

    /// Drops every estimated height, as the layout did before it kept them: for a bench to compare against.
    pub fn forget_estimates(&mut self) {
        self.state.memo.estimates.clear();
    }

    /// Picks these contexts out, as a click or a box would.
    pub fn select(&mut self, ids: impl IntoIterator<Item = String>) {
        self.state.selection.extend(ids);
    }

    pub fn nodes(&self) -> usize {
        self.state.nodes.len()
    }

    pub fn total(&self) -> f32 {
        self.state.total
    }

    /// What this model and `other` disagree on: nothing when one is exactly the other.
    pub fn differences(&self, other: &Self) -> Vec<String> {
        model_differences(&self.state, &other.state)
    }
}

/// `nodeLayout`'s positions: the stored or estimated height of every row, and where each starts. While a drag has pieces out of the
/// timeline it closes up over them, keeping what follows them on screen, and pieces docked at the home line take room where they stand.
fn relayout(state: &mut TimelineState, contexts: &Resolved<'_>, width: f32, message_size: f32) {
    layout_rows(state, contexts, width, message_size, 0);
}

/// [`relayout`], keeping the rows before `from` as they are when the last layout can be gone on from: it had nothing dragged in it,
/// was made at this width and type size, and nothing the rows before `from` read has changed since (their heights included: see
/// [`first_row_of`]). Otherwise all of it.
fn relayout_from(state: &mut TimelineState, contexts: &Resolved<'_>, width: f32, message_size: f32, from: usize) {
    let from = from.min(state.nodes.len()).min(state.row_h.len());
    let goes_on = from > 0 && !state.layout_dirty && state.memo.laid_out_at == Some(layout_key(width, message_size)) && state.tops.len() > from && state.drag.as_ref().is_none_or(|drag| drag.session.is_none());
    layout_rows(state, contexts, width, message_size, if goes_on { from } else { 0 });
}

/// The layout of the rows from `from` on; the rows before it stay as they are, which only a caller that knows them current asks for.
fn layout_rows(state: &mut TimelineState, contexts: &Resolved<'_>, width: f32, message_size: f32, from: usize) {
    let count = state.nodes.len();
    state.row_h.truncate(from);
    state.row_h.reserve(count - from);
    state.tops.truncate(from);
    state.tops.reserve(count + 1 - from);
    state.adjust.clear();
    state.extras_only.clear();
    // Out of the state while it is read, as everything below writes to the state.
    let drag = state.drag.take();
    let session = drag.as_ref().and_then(|drag| drag.session.as_ref());
    debug_assert!(from == 0 || session.is_none(), "a layout with a drag in it is made in full");
    if session.is_none() {
        state.dock_measured = None;
    }
    let look = session.map(|session| session.look);
    let mut hidden: HashMap<usize, f32> = HashMap::new();
    let mut shrink: HashMap<usize, f32> = HashMap::new();
    if let Some(session) = session
        && matches!(look, Some(Look::Gathered | Look::Docked))
    {
        for (at, piece) in session.pieces.iter().enumerate() {
            if piece.row {
                *shrink.entry(piece.node).or_default() += session.piece_boxes[at].height;
            } else {
                let is_block = session.units[piece.units[0]].row.is_some();
                hidden.insert(piece.node, session.piece_boxes[at].height + if is_block { BLOCK_PAD_TOP + BLOCK_PAD_BOTTOM } else { 0.0 });
            }
        }
    }
    let has_extras = |state: &TimelineState, position: usize| state.changes_after.contains_key(&position) || state.projection.notices_after_node.contains_key(&position);
    // A slot keeps its distance from the one before it, and a slot lifted out is still the one before it: with nothing left above, that
    // distance would stand at the head of the column as room for what left.
    let first_shown = (!hidden.is_empty() || !shrink.is_empty()).then(|| (0..count).find(|position| !hidden.contains_key(position) || has_extras(state, *position))).flatten();
    let nothing_leads = state.projection.leading_turns.is_empty();
    let mut flush = first_shown.filter(|position| nothing_leads && *position > 0 && !hidden.contains_key(position));
    let mut room: Option<DockRoom> = None;
    if let Some(session) = session
        && look == Some(Look::Docked)
    {
        let layout = session.dock_layout();
        let content = state.dock_measured.unwrap_or(layout.height);
        let at = session.dock_place();
        let (lead, trail) = match at {
            // First in the column it has no gap of its own, and what follows it keeps one.
            DockAt::Before(position) if first_shown == Some(position) && nothing_leads => {
                flush = None;
                (0.0, if position == 0 { SLOT_GAP } else { 0.0 })
            }
            DockAt::Before(_) => (SLOT_GAP, 0.0),
            DockAt::End => (if first_shown.is_some() { SLOT_GAP } else { 0.0 }, 0.0),
            // Among a block's rows, a card among the copies stands off from the rows as it will once it is a slot of its own there.
            DockAt::Inside { .. } => (if layout.first_is_slot { SLOT_GAP } else { 0.0 }, if layout.last_is_slot { SLOT_GAP } else { 0.0 }),
        };
        room = Some(DockRoom { at, height: lead + content + trail, lead });
    }
    state.dock = room;
    state.flush = flush;
    // A row's estimate reads its text, so it is kept between layouts at one width and type size; a drag's layouts read what they
    // are handed.
    let key = layout_key(width, message_size);
    let kept_estimates = session.is_none();
    if kept_estimates {
        if state.memo.estimated_at != key {
            state.memo.estimates.clear();
            state.memo.estimated_at = key;
        }
        state.memo.estimates.resize(count, f32::NAN);
    }
    // Where the row at `from` starts, as the loop below would have come to it.
    let mut y = if from == 0 { 0.0 } else { state.tops[from - 1] + state.row_h[from - 1] };
    for position in from..count {
        // The pieces set down at the home line take room ahead of what follows them.
        if let Some(room) = room
            && room.at == DockAt::Before(position)
        {
            y += room.height;
        }
        state.tops.push(y);
        let key = state.node_keys[position];
        let stored = match state.heights.get(&key) {
            Some(h) => *h,
            None => {
                let gap = row_gap(state, position);
                let extras = state.changes_after.get(&position).map_or(0.0, |summaries| summaries.len() as f32 * 76.0) + state.projection.notices_after_node.get(&position).map_or(0.0, |notices| notices.len() as f32 * 48.0);
                let estimate = if kept_estimates {
                    let slot = &mut state.memo.estimates[position];
                    if slot.is_nan() {
                        *slot = estimate_node(contexts, &state.nodes[position], width, message_size);
                    }
                    *slot
                } else {
                    estimate_node(contexts, &state.nodes[position], width, message_size)
                };
                gap + extras + estimate
            }
        };
        let mut less = shrink.get(&position).copied().unwrap_or(0.0);
        if let Some(slot) = hidden.get(&position) {
            if !has_extras(state, position) {
                state.row_h.push(0.0);
                continue;
            }
            // What follows a slot lifted out stays where it is on the timeline: a list of changed files is not a card to drag.
            less += slot + row_gap(state, position);
            state.extras_only.insert(position);
        }
        if flush == Some(position) {
            let flush_held = std::mem::take(&mut state.flush);
            less += row_gap(state, position);
            state.flush = flush_held;
        }
        if let Some(DockRoom { at: DockAt::Inside { node, .. }, height, .. }) = room
            && node == position
        {
            less -= height;
        }
        if less != 0.0 {
            state.adjust.insert(position, less);
        }
        let h = (stored - less).max(0.0);
        state.row_h.push(h);
        y += h;
    }
    if let Some(room) = room
        && room.at == DockAt::End
    {
        y += room.height;
    }
    state.tops.push(y);
    state.total = y;
    state.layout_dirty = false;
    state.memo.laid_out_at = kept_estimates.then_some(key);
    state.drag = drag;
}

/// `.context-slot + .context-slot`: the distance a row keeps from the one before it. A failure notice hides the slot under it from that
/// rule, and a row with nothing left above it in a drag has nothing to keep it from.
fn row_gap(state: &TimelineState, position: usize) -> f32 {
    let after_notice = position.checked_sub(1).and_then(|above| state.projection.notices_after_node.get(&above)).and_then(|notices| notices.last()).is_some_and(|at| state.turns.get(*at).is_some_and(|turn| turn.error.is_some()));
    if position == 0 || state.flush == Some(position) || after_notice { 0.0 } else { SLOT_GAP }
}

fn unit_height(state: &TimelineState, unit: &Unit) -> f32 {
    state.unit_heights.get(&hash_id(&unit.id)).copied().unwrap_or(if unit.row.is_some() { 30.0 } else { 40.0 })
}

/// Everything on the timeline a drag or a box can touch, in order. A prompt or a compaction is a card around its one row, so
/// the card is the unit.
fn timeline_units(contexts: &Resolved<'_>, nodes: &[RenderNode], selection: &HashSet<String>) -> Vec<Unit> {
    let mut units = Vec::new();
    for (position, node) in nodes.iter().enumerate() {
        match node {
            RenderNode::Context { index } => {
                let id = contexts.get(*index).id().to_owned();
                units.push(Unit { node: position, row: None, picked: selection.contains(&id), ids: vec![id.clone()], id, index: *index });
            }
            RenderNode::Question { entry, answer, .. } => {
                let id = contexts.get(*entry).id().to_owned();
                let mut ids = vec![id.clone()];
                ids.extend(answer.map(|answer| contexts.get(answer).id().to_owned()));
                units.push(Unit { node: position, row: None, picked: selection.contains(&id), ids, id, index: *entry });
            }
            RenderNode::Block { entries, .. } => {
                for (row, entry) in entries.iter().enumerate() {
                    let id = contexts.get(entry.index).id().to_owned();
                    units.push(Unit { node: position, row: Some(row), picked: selection.contains(&id), ids: vec![id.clone()], id, index: entry.index });
                }
            }
        }
    }
    units
}

/// Where each unit sits in the content, top and bottom, as the timeline lies now.
fn unit_spans(state: &TimelineState, units: &[Unit], lifted: Option<&HashSet<String>>) -> Vec<(f32, f32)> {
    let mut spans = Vec::with_capacity(units.len());
    let mut at = 0;
    while at < units.len() {
        let node = units[at].node;
        let mut y = PAD_TOP + state.tops.get(node).copied().unwrap_or(0.0) + row_gap(state, node);
        if units[at].row.is_some() {
            y += BLOCK_PAD_TOP;
        }
        while at < units.len() && units[at].node == node {
            if let Some(DockRoom { at: DockAt::Inside { before, .. }, height, .. }) = state.dock
                && before == at
            {
                y += height;
            }
            let unit = &units[at];
            let h = if lifted.is_some_and(|lifted| lifted.contains(&unit.id)) { 0.0 } else { unit_height(state, unit) };
            spans.push((y, y + h));
            y += h;
            at += 1;
        }
    }
    spans
}

/// The timeline as a drag sees it.
struct Geometry<'a, 'b> {
    state: &'a mut TimelineState,
    contexts: &'a Resolved<'b>,
    width: f32,
    message_size: f32,
    view: Rect,
    offset: f32,
    spans: Vec<(f32, f32)>,
    count: usize,
    /// Scrolling this frame, requested once at the end.
    shift: f32,
}

impl Geometry<'_, '_> {
    fn max_offset(&self) -> f32 {
        (PAD_TOP + self.state.total + PAD_BOTTOM - self.view.h).max(0.0)
    }
}

impl DragGeometry for Geometry<'_, '_> {
    fn span(&self, unit: usize) -> (f32, f32) {
        let (top, bottom) = self.spans.get(unit).copied().unwrap_or((0.0, 0.0));
        (self.view.y + top - self.offset, self.view.y + bottom - self.offset)
    }

    fn view(&self) -> (f32, f32) {
        (self.view.y, self.view.bottom())
    }

    fn column_top(&self) -> f32 {
        self.view.y + PAD_TOP - self.offset
    }

    fn end_index(&self) -> usize {
        self.count
    }

    fn scroll_by(&mut self, delta: f32) -> f32 {
        let next = (self.offset + delta).clamp(0.0, self.max_offset());
        let moved = next - self.offset;
        self.offset = next;
        self.shift += moved;
        moved
    }

    fn apply_look(&mut self, session: &DragSession, look: Look) {
        // The session lives in `state.drag` taken out for the step, so the layout reads a stand-in with the new look.
        let mut stand_in = session.clone();
        stand_in.look = look;
        let mut drag = PressDrag::new(0.0, 0.0);
        drag.session = Some(stand_in);
        self.state.drag = Some(drag);
        relayout(self.state, self.contexts, self.width, self.message_size);
        self.state.drag = None;
        let lifted = (look != Look::Apart).then_some(&session.lifted_ids);
        self.spans = unit_spans(self.state, &session.units, lifted);
    }

    fn dock_top(&self, _session: &DragSession) -> Option<f32> {
        let room = self.state.dock?;
        let top = match room.at {
            DockAt::Before(position) => PAD_TOP + self.state.tops.get(position).copied().unwrap_or(self.state.total) - room.height,
            DockAt::End => PAD_TOP + self.state.total - room.height,
            DockAt::Inside { before, .. } => self.spans.get(before)?.0 - room.height,
        };
        Some(self.view.y + top + room.lead - self.offset)
    }

    fn reflow_snapshot(&self, session: &DragSession) -> Vec<(Mover, f32)> {
        reflow_snapshot(self.state, session, &self.spans, self.view, self.offset, Instant::now())
    }

    fn reflow_from(&mut self, session: &DragSession, tops: Vec<(Mover, f32)>, now: Instant) {
        // Measured where it was drawn, slides that were still running included, and moves from there.
        self.state.slides.clear();
        self.state.unit_slides.retain(|_, slide| slide.fade_from.is_some());
        if session.still {
            return;
        }
        let by_id: HashMap<u64, f32> = session.units.iter().zip(&self.spans).map(|(unit, span)| (hash_id(&unit.id), span.0)).collect();
        let by_key: HashMap<u64, f32> = self.state.node_keys.iter().zip(&self.state.tops).map(|(key, top)| (*key, PAD_TOP + *top)).collect();
        let feet = row_feet(self.state);
        slide_from(self.state, &tops, &by_key, &feet, &by_id, self.view, self.offset, now);
    }

    fn fly_piece(&mut self, session: &DragSession, piece: usize, dx: f32, dy: f32, now: Instant) {
        for &unit in &session.pieces[piece].units {
            self.state.unit_slides.insert(hash_id(&session.units[unit].id), Slide { dx, dy, fade_from: None, started: now });
        }
    }
}

/// `reflowSnapshot`: the things a change of look moves, with where each is drawn now. A block losing some of its rows moves the rest
/// one by one; anything else moves whole. Only what is near the view: past that nobody sees it slide.
fn reflow_snapshot(state: &TimelineState, session: &DragSession, spans: &[(f32, f32)], view: Rect, offset: f32, now: Instant) -> Vec<(Mover, f32)> {
    let lifted: HashSet<usize> = session.pieces.iter().filter(|piece| !piece.row).map(|piece| piece.node).collect();
    let partial: HashSet<usize> = session.pieces.iter().filter(|piece| piece.row).map(|piece| piece.node).collect();
    let near = |top: f32, bottom: f32| bottom >= view.y - REFLOW_REACH && top <= view.bottom() + REFLOW_REACH;
    let residual = |slide: Option<&Slide>| slide.map_or(0.0, |slide| slide.offset(now).1);
    let mut tops = Vec::new();
    for (position, key) in state.node_keys.iter().enumerate() {
        let height = state.row_h.get(position).copied().unwrap_or(0.0);
        let top = view.y + PAD_TOP + state.tops.get(position).copied().unwrap_or(0.0) - offset;
        if lifted.contains(&position) || partial.contains(&position) {
            // The slot moves as its pieces do, or not at all, and what follows it in the row still has to get where it is going.
            let extras = state.changes_after.contains_key(&position) || state.projection.notices_after_node.contains_key(&position);
            if extras && near(top + height, top + height) {
                tops.push((Mover::Extras(*key), top + height + residual(state.slides.get(&extras_key(*key)))));
            }
            continue;
        }
        if height > 0.0 && near(top, top + height) {
            tops.push((Mover::Node(*key), top + residual(state.slides.get(key))));
        }
    }
    for (unit, span) in session.units.iter().zip(spans) {
        if !partial.contains(&unit.node) || session.lifted_ids.contains(&unit.id) {
            continue;
        }
        let top = view.y + span.0 - offset;
        if near(top, view.y + span.1 - offset) {
            let id = hash_id(&unit.id);
            tops.push((Mover::Unit(id), top + residual(state.unit_slides.get(&id))));
        }
    }
    tops
}

/// Where each row ends: what is measured for the lists after a card, which are the last thing in its row.
fn row_feet(state: &TimelineState) -> HashMap<u64, f32> {
    state.node_keys.iter().enumerate().map(|(position, key)| (*key, PAD_TOP + state.tops.get(position).copied().unwrap_or(0.0) + state.row_h.get(position).copied().unwrap_or(0.0))).collect()
}

/// `reflowFrom`: slides everything in `tops` from where it was to where the layout has it now. What has gone has nothing to slide.
#[allow(clippy::too_many_arguments)]
fn slide_from(state: &mut TimelineState, tops: &[(Mover, f32)], by_key: &HashMap<u64, f32>, feet: &HashMap<u64, f32>, by_id: &HashMap<u64, f32>, view: Rect, offset: f32, started: Instant) {
    for (mover, was) in tops {
        let (now, slides, key) = match mover {
            Mover::Node(key) => (by_key.get(key), &mut state.slides, *key),
            Mover::Unit(id) => (by_id.get(id), &mut state.unit_slides, *id),
            Mover::Extras(key) => (feet.get(key), &mut state.slides, extras_key(*key)),
        };
        let Some(now) = now else { continue };
        let shift = was - (view.y + now - offset);
        if shift.abs() < 0.5 {
            slides.remove(&key);
        } else {
            slides.insert(key, Slide { dx: 0.0, dy: shift, fade_from: None, started });
        }
    }
}

/// `end`: what a drag ending leaves to `settle`, read off the page as it stands: what stayed and where it is, where the copies of what
/// was dragged are, in the stack or at the home line, and what sits just above the drop.
fn settling_of(state: &TimelineState, session: &DragSession, pointer_y: f32, view: Rect, offset: f32, now: Instant) -> Settling {
    let lifted = (session.look != Look::Apart).then_some(&session.lifted_ids);
    let spans = unit_spans(state, &session.units, lifted);
    let tops = if session.look == Look::Apart { Vec::new() } else { reflow_snapshot(state, session, &spans, view, offset, now) };
    let mut held = Vec::new();
    let mut hold = |piece: usize, x: f32, y: f32, padded: bool| {
        let mut y = y + if padded { BLOCK_PAD_TOP } else { 0.0 };
        for &unit in &session.pieces[piece].units {
            let unit = &session.units[unit];
            held.push((hash_id(&unit.id), (x, y), session.piece_boxes[piece].left));
            y += unit_height(state, unit);
        }
    };
    let whole_block = |piece: usize| !session.pieces[piece].row && session.units[session.pieces[piece].units[0]].row.is_some();
    match session.look {
        Look::Gathered => {
            for entry in &session.ghost {
                let ((x, y, _), _) = entry.frame(now);
                hold(entry.piece, session.layer.0 + x, session.layer.1 + y, whole_block(entry.piece));
            }
        }
        Look::Docked => {
            let layout = session.dock_layout();
            for piece in 0..session.pieces.len() {
                hold(piece, session.piece_boxes[piece].left, session.mark_y + layout.tops[piece], whole_block(piece));
            }
        }
        Look::Apart => {}
    }
    // Whatever sits just above the drop stays where it is while the rest falls into place.
    let steady = match session.look {
        Look::Gathered => {
            let below = session.rest.partition_point(|&unit| (spans[unit].0 + spans[unit].1) * 0.5 - offset + view.y <= pointer_y);
            below.checked_sub(1).map(|at| session.rest[at])
        }
        Look::Docked => session.anchor,
        Look::Apart => None,
    };
    let steady = steady.map(|unit| (session.units[unit].id.clone(), view.y + spans[unit].1 - offset));
    Settling { tops, held, steady, still: session.still, frames: 2, started: now }
}

/// `settle`: what stayed slides to where it is now, and what was dragged lands from where its copy was. Answers how far to scroll.
fn settle(state: &mut TimelineState, contexts: &Resolved<'_>, settling: &Settling, view: Rect, offset: f32) -> f32 {
    let units = timeline_units(contexts, &state.nodes, &HashSet::new());
    let spans = unit_spans(state, &units, None);
    let by_id: HashMap<u64, (f32, f32)> = units.iter().zip(&spans).map(|(unit, span)| (hash_id(&unit.id), *span)).collect();
    let mut next = offset;
    if let Some((id, bottom)) = &settling.steady
        && let Some(span) = by_id.get(&hash_id(id))
    {
        next = (view.y + span.1 - bottom).clamp(0.0, (PAD_TOP + state.total + PAD_BOTTOM - view.h).max(0.0));
    }
    if settling.still {
        return next - offset;
    }
    if !settling.tops.is_empty() || !settling.held.is_empty() {
        state.slides.clear();
        state.unit_slides.clear();
    }
    let tops_by_id: HashMap<u64, f32> = by_id.iter().map(|(id, span)| (*id, span.0)).collect();
    let by_key: HashMap<u64, f32> = state.node_keys.iter().zip(&state.tops).map(|(key, top)| (*key, PAD_TOP + *top)).collect();
    let feet = row_feet(state);
    slide_from(state, &settling.tops, &by_key, &feet, &tops_by_id, view, next, settling.started);
    for (id, (x, y), left) in &settling.held {
        let Some(span) = by_id.get(id) else { continue };
        state.unit_slides.insert(*id, Slide { dx: x - left, dy: y - (view.y + span.0 - next), fade_from: Some(0.9), started: settling.started });
    }
    next - offset
}

/// `end` for a drag called off: the page settles as it does after a drop, with nothing moved.
fn call_off_drag(shared: &Shared, scroller: u64, now: Instant) {
    let mut state = shared.borrow_mut();
    let view = rect_of(scroller).unwrap_or_default();
    let offset = scroll_state(scroller).map_or(0.0, |s| s.0);
    if let Some(session) = state.drag.take().and_then(|drag| drag.session) {
        let pointer_y = session.layer.1;
        let settling = settling_of(&state, &session, pointer_y, view, offset, now);
        state.settling = Some(settling);
    }
    state.drag_target = None;
    state.layout_dirty = true;
    repaint();
}

/// Runs `f` with the drag taken out of the state and a geometry over the rest.
fn with_drag(state: &mut TimelineState, contexts: &Resolved<'_>, view: Rect, offset: f32, width: f32, message_size: f32, f: impl FnOnce(&mut PressDrag, &mut Geometry<'_, '_>)) -> f32 {
    let Some(mut drag) = state.drag.take() else { return 0.0 };
    let lifted = drag.session.as_ref().filter(|session| session.look != Look::Apart).map(|session| session.lifted_ids.clone());
    let units = drag.session.as_ref().map(|session| session.units.clone()).unwrap_or_else(|| timeline_units(contexts, &state.nodes, &state.selection));
    let spans = unit_spans(state, &units, lifted.as_ref());
    let count = contexts.count();
    let mut geometry = Geometry { state, contexts, width, message_size, view, offset, spans, count, shift: 0.0 };
    f(&mut drag, &mut geometry);
    let shift = geometry.shift;
    let state = geometry.state;
    state.drag = Some(drag);
    relayout(state, contexts, width, message_size);
    shift
}

/// `groupMeta`'s order: which categories a tool can be placed from, in the menu's order.
const TOOL_GROUPS: [ToolCategory; 8] = [ToolCategory::Filesystem, ToolCategory::Shell, ToolCategory::Web, ToolCategory::Screen, ToolCategory::Android, ToolCategory::Orchestration, ToolCategory::Memory, ToolCategory::Mcp];

fn group_label(category: ToolCategory) -> String {
    match category {
        ToolCategory::Filesystem => t("文件与搜索", "Files and search").to_owned(),
        ToolCategory::Web => t("预览", "Preview").to_owned(),
        ToolCategory::Screen => t("屏幕", "Screens").to_owned(),
        ToolCategory::Android => t("Android", "Android").to_owned(),
        ToolCategory::Memory => t("长期记忆", "Long-term memory").to_owned(),
        ToolCategory::Mcp => "MCP".to_owned(),
        ToolCategory::Orchestration => t("代理编排", "Agent orchestration").to_owned(),
        _ => "Shell".to_owned(),
    }
}

/// Orchestration tools run only inside the model loop, so they are not offered as calls to place by hand. The preview tools and
/// those of a remote-control screen need a live page or device to act on, so they are not placeable either.
pub fn insertable_tool_groups<'a>(tools: &'a [ToolDescriptor], enabled: &[String]) -> Vec<(ToolCategory, Vec<&'a ToolDescriptor>)> {
    let placeable: Vec<&ToolDescriptor> = tools.iter().filter(|tool| tool.category != ToolCategory::Orchestration && !is_preview_tool_name(&tool.name) && enabled.contains(&tool.name)).collect();
    TOOL_GROUPS.iter().map(|category| (*category, placeable.iter().copied().filter(|tool| tool.category == *category).collect::<Vec<_>>())).filter(|(_, tools)| !tools.is_empty()).collect()
}

/// `contextMeta`, in its order.
const INSERT_KINDS: [(InsertKind, &str, &str); 5] = [
    (InsertKind::System, "系统提示词", "System prompt"),
    (InsertKind::User, "用户输入", "User input"),
    (InsertKind::Reasoning, "思考字段", "Reasoning field"),
    (InsertKind::Tool, "工具调用", "Tool call"),
    (InsertKind::Assistant, "模型回复", "Model reply"),
];

fn menu_row(id: impl Into<String>, label: impl Into<String>, on_select: impl Fn(&mut App) + 'static) -> ContextMenuItem {
    ContextMenuItem { id: id.into(), label: label.into(), on_select: Some(Rc::new(on_select)), ..ContextMenuItem::default() }
}

/// The menu a right click opens: what can be placed at the line, what was copied, and forking there.
fn insert_menu_sections(props: &Rc<ConversationViewProps>, index: usize) -> Vec<ContextMenuSection> {
    let mut kinds = Vec::new();
    let groups = insertable_tool_groups(&props.tools, &props.enabled_tools);
    for (kind, zh, en) in INSERT_KINDS {
        // A timeline with no way to commit a placed card does not offer to place one. Being unable to *execute* it is not that.
        if kind == InsertKind::Tool && props.on_save_tool.is_none() && props.on_save_tool_edit.is_none() {
            continue;
        }
        if kind != InsertKind::Tool {
            let insert = props.on_insert.clone();
            kinds.push(menu_row(en, t(zh, en), move |app| {
                if let Some(insert) = &insert {
                    insert(app, index, kind, None);
                }
            }));
            continue;
        }
        // A call is placed by naming the tool, which is one choice more than the other kinds need, so this row opens a list beside itself.
        let disabled = groups.is_empty();
        let children: Vec<ContextMenuItem> = groups
            .iter()
            .map(|(category, tools)| {
                let rows = tools
                    .iter()
                    .map(|tool| {
                        let insert = props.on_insert.clone();
                        let name = tool.name.clone();
                        ContextMenuItem { title: Some(tool.name.clone()), ..menu_row(tool.name.clone(), tool.label.clone(), move |app| {
                            if let Some(insert) = &insert {
                                insert(app, index, InsertKind::Tool, Some(name.clone()));
                            }
                        }) }
                    })
                    .collect();
                ContextMenuItem { id: format!("{category:?}"), label: group_label(*category), children: Some(Rc::new(vec![ContextMenuSection::new("tools", rows)])), ..ContextMenuItem::default() }
            })
            .collect();
        kinds.push(ContextMenuItem {
            id: "tool".to_owned(),
            label: t(zh, en).to_owned(),
            disabled,
            title: disabled.then(|| t("当前对话没有启用工具", "No tools are enabled in this conversation").to_owned()),
            children: (!disabled).then(|| Rc::new(vec![ContextMenuSection::new("groups", children)])),
            ..ContextMenuItem::default()
        });
    }
    let mut sections = vec![ContextMenuSection::new("kinds", kinds)];
    let mut more = Vec::new();
    if let Some(paste) = props.on_paste_contexts.clone() {
        let count = props.paste_count;
        more.push(ContextMenuItem {
            disabled: count == 0,
            title: Some(if count > 0 { t("把复制的消息插入到分割线处", "Insert the copied messages at the line") } else { t("还没有复制任何消息", "No messages have been copied yet") }.to_owned()),
            hint: Some(timeline_clipboard_shortcut(TimelineClipboardAction::Paste, IS_APPLE_PLATFORM)),
            ..menu_row("paste", t("粘贴", "Paste"), move |app| {
                if count > 0 {
                    paste(app, index, Vec::new());
                }
            })
        });
    }
    if let Some(fork) = props.on_fork_at.clone() {
        let reason = props.fork_disabled_reason.clone();
        more.push(ContextMenuItem {
            disabled: index == 0 || reason.is_some(),
            title: reason.or_else(|| (index == 0).then(|| t("分割线上方没有可分叉的上下文", "There is nothing above the line to fork").to_owned())),
            ..menu_row("fork", t("分叉会话", "Fork conversation"), move |app| fork(app, index))
        });
    }
    if !more.is_empty() {
        sections.push(ContextMenuSection::new("more", more));
    }
    sections
}

/// A right click with contexts picked out offers only what can be done to all of them at once.
fn selection_menu_sections(props: &Rc<ConversationViewProps>, shared: &Shared, context_order: Rc<HashMap<u64, u32>>) -> Vec<ContextMenuSection> {
    let ordered = {
        let shared = shared.clone();
        move || ordered_selection(&shared.borrow().selection, &context_order)
    };
    let mut first = Vec::new();
    if let Some(copy) = props.on_copy_contexts.clone() {
        let ordered = ordered.clone();
        first.push(ContextMenuItem { hint: Some(timeline_clipboard_shortcut(TimelineClipboardAction::Copy, IS_APPLE_PLATFORM)), ..menu_row("copy", t("复制", "Copy"), move |app| copy(app, ordered())) });
    }
    if let Some(paste) = props.on_paste_contexts.clone() {
        let count = props.paste_count;
        let ordered = ordered.clone();
        let positions = shared.clone();
        first.push(ContextMenuItem {
            disabled: count == 0,
            title: Some(if count > 0 { t("用复制的消息替换所选消息", "Replace the selected messages with the copied ones") } else { t("还没有复制任何消息", "No messages have been copied yet") }.to_owned()),
            hint: Some(timeline_clipboard_shortcut(TimelineClipboardAction::Paste, IS_APPLE_PLATFORM)),
            ..menu_row("paste", t("粘贴", "Paste"), move |app| {
                let ids = ordered();
                // The selection is not let go here: what the paste takes out leaves it by leaving the timeline, and a paste
                // that could not be made leaves it picked to try again.
                let index = ids.first().and_then(|id| positions.borrow().context_index.get(&hash_id(id)).copied()).unwrap_or(u32::MAX);
                if count > 0 {
                    paste(app, index as usize, ids);
                }
            })
        });
    }
    let delete = props.on_delete_contexts.clone();
    let selection = shared.clone();
    let delete_row = menu_row("delete", t("删除", "Delete"), move |app| {
        let ids = ordered();
        selection.borrow_mut().selection.clear();
        if let Some(delete) = &delete {
            delete(app, ids);
        }
    });
    let mut sections = Vec::new();
    if !first.is_empty() {
        sections.push(ContextMenuSection::new("clipboard", first));
    }
    sections.push(ContextMenuSection::new("delete", vec![delete_row]));
    sections
}

/// What is picked out, in timeline order.
fn ordered_selection(selection: &HashSet<String>, order: &HashMap<u64, u32>) -> Vec<String> {
    let mut ids: Vec<String> = selection.iter().cloned().collect();
    ids.sort_by_key(|id| order.get(&hash_id(id)).copied().unwrap_or(u32::MAX));
    ids
}

/// One row of the virtual list: a node, then what follows it on screen.
fn row_view(f: &Frame<'_>, position: usize) -> E {
    let state = f.shared.borrow();
    let nodes = state.nodes.clone();
    let node_key = state.node_keys[position];
    let changes = state.changes_after.get(&position).cloned().unwrap_or_default();
    let notices = state.projection.notices_after_node.get(&position).cloned().unwrap_or_default();
    let turns = state.turns.clone();
    let (gap, extras_only) = (row_gap(&state, position), state.extras_only.contains(&position));
    let extras_slide = state.slides.get(&extras_key(node_key)).copied().filter(|_| f.salt == 0);
    // The window as the node's own top sees it.
    let top = state.tops.get(position).copied().unwrap_or(0.0) + gap;
    let window = f.window.map(|(from, to)| (from - top, to - top));
    drop(state);
    // A row that comes out as it did last frame keeps last frame's layout: a token streamed into the last row lays out that row.
    let mut row = col().key(row_key(f.salt, node_key)).w_full().min_w(0.0).shrink0().memo_layout();
    if !extras_only {
        row = row.pt(gap).child(node_view(f, &nodes[position], window));
    }
    if changes.is_empty() && notices.is_empty() {
        return row;
    }
    let mut extras = col().w_full().min_w(0.0);
    let mut follows_changes = !changes.is_empty();
    for summary in changes {
        extras = extras.child(turn_changes(&summary, f.props.path_base_dir.as_deref()));
    }
    for at in notices {
        extras = extras.children(turn_notices(f, &turns[at], follows_changes));
        follows_changes = false;
    }
    // Only drawn, as a slide of a whole row is: the layout already has them where they are going.
    if let Some(slide) = extras_slide {
        extras = drawn_slide(extras, &slide, Instant::now());
        repaint();
    }
    row.child(extras)
}

/// `renderTimelineNode`. `window` is what is built of it, from its top; `None` builds all of it.
fn node_view(f: &Frame<'_>, node: &RenderNode, window: Option<(f32, f32)>) -> E {
    let mut slot = col().w_full().min_w(0.0);
    match node {
        RenderNode::Block { entries, .. } => {
            // A block draws several contexts as rows, so an insert index anywhere inside it is answered by the one editor drawn ahead of the block.
            let first = entries.iter().map(|entry| entry.index).min().unwrap_or(0);
            let after = entries.iter().map(|entry| entry.index).max().unwrap_or(0) + 1;
            let mut window = window;
            if f.insert_editor_index.is_some_and(|index| index >= first && index < after) {
                slot = slot.child(timeline_insert_editor(f));
                // The editor ahead of the rows moves them by however tall it is.
                window = None;
            }
            // Only the block the line falls in hears where it is.
            let insertion = f.display_insertion_index.filter(|index| *index >= first && *index < after);
            slot.child(timeline_block(f, entries, insertion, window))
        }
        RenderNode::Question { entry, answer, .. } => {
            if f.insert_editor_index == Some(*entry) {
                slot = slot.child(timeline_insert_editor(f));
            }
            if !f.mutation_read_only && f.display_insertion_index == Some(*entry) {
                slot = slot.child(insertion_line_view());
            }
            let context = f.contexts.get(*entry);
            let tool = context.tool().expect("a question node holds a tool card");
            let answer_item = answer.map(|answer| f.contexts.get(answer).item);
            let id = tool.id.to_owned();
            let answer_id = answer_item.map(|item| item.id().to_owned());
            let question_handler = |handler: Option<crate::props::OnQuestion>| -> Option<Handler<App>> {
                let handler = handler?;
                let (props, id, answer_id) = (f.props.clone(), id.clone(), answer_id.clone());
                Some(Rc::new(move |app: &mut App| {
                    let Some(item) = find_item(app, &props, &id) else { return };
                    let answer = answer_id.as_ref().and_then(|id| find_item(app, &props, id));
                    handler(app, &item, answer.as_ref());
                }))
            };
            let unit = f.unit_key(tool.id);
            let dimmed = f.dimmed.contains(tool.id);
            slot.child(f.slid(tool.id, col().key(unit).w_full().when(dimmed, |e| e.opacity(0.6))).child(question_timeline_card(QuestionCardProps {
                item: tool,
                index: *entry,
                answer: answer_item,
                answer_index: *answer,
                read_only: f.mutation_read_only,
                editing: f.editing_question_id == Some(tool.id),
                selected: f.selected.contains(tool.id),
                on_edit: question_handler(f.props.on_edit_question.clone()),
                on_delete: question_handler(f.props.on_delete_question.clone()),
                on_cancel_edit: f.props.on_cancel_edit.clone(),
                on_save_question: f.props.on_save_question.clone(),
                on_open_insert: Some(f.open_insert.clone()),
            })))
        }
        RenderNode::Context { index } => {
            if f.insert_editor_index == Some(*index) {
                slot = slot.child(timeline_insert_editor(f));
            }
            if !f.mutation_read_only && f.display_insertion_index == Some(*index) {
                slot = slot.child(insertion_line_view());
            }
            slot.child(context_card(f, *index))
        }
    }
}

/// A copy of one drag piece: the card it is, under keys of its own and with nothing in it to press.
fn piece_view(f: &Frame<'_>, session: &DragSession, piece: usize) -> E {
    let state = f.shared.borrow();
    let nodes = state.nodes.clone();
    drop(state);
    let piece = &session.pieces[piece];
    match &nodes[piece.node] {
        RenderNode::Block { entries, .. } => {
            let rows: Vec<_> = piece.units.iter().filter_map(|&unit| session.units[unit].row).map(|row| entries[row]).collect();
            // A row lifted out of its block keeps a block around it, with no padding of its own.
            timeline_rows(f, &rows, None, !piece.row, None)
        }
        node => node_view(f, node, None),
    }
}

/// `buildDock`: the pieces in order and side by side, each card a slot of its own, and rows that end up next to each other in one block.
fn dock_view(f: &Frame<'_>, session: &DragSession, key: u64, top: f32) -> E {
    let nodes = f.shared.borrow().nodes.clone();
    let mut dock = col().key(key).absolute().left(0.0).right(0.0).top(top).gap(SLOT_GAP).opacity(0.6).inert().aria_hidden(true);
    for (pieces, _) in session.dock_layout().groups {
        dock = dock.child(if session.pieces[pieces[0]].row {
            let rows: Vec<_> = pieces
                .iter()
                .flat_map(|&piece| &session.pieces[piece].units)
                .filter_map(|&unit| match &nodes[session.units[unit].node] {
                    RenderNode::Block { entries, .. } => session.units[unit].row.map(|row| entries[row]),
                    _ => None,
                })
                .collect();
            timeline_block(f, &rows, None, None)
        } else {
            piece_view(f, session, pieces[0])
        });
    }
    dock
}

/// What is drawn over a conversation's persisted contexts: its run's view (the cards of tasks earlier runs started, then
/// the run's projection), else what its background tasks streamed since the last run settled; the props' own contexts on
/// a timeline that is not a conversation's.
fn live_contexts<'a>(conversation: Option<&str>, run: Option<&'a RunView>, overlay: Option<&'a TaskOverlayView>, props: &'a ConversationViewProps) -> &'a [LiveContext] {
    match (conversation, run, overlay) {
        (None, _, _) => props.contexts.as_slice(),
        (Some(_), Some(run), _) => run.contexts.as_slice(),
        (Some(_), None, Some(overlay)) => overlay.contexts.as_slice(),
        (Some(_), None, None) => &[],
    }
}

/// How many of [`live_contexts`] are drawn in the place of the persisted card each stands for (a task overlay's cards),
/// ahead of those that follow the persisted contexts (a run's projection): [`Resolved::map_with_in_place`].
fn live_in_place(conversation: Option<&str>, run: Option<&RunView>, overlay: Option<&TaskOverlayView>) -> usize {
    match (conversation, run, overlay) {
        (Some(_), Some(run), _) => run.in_place,
        (Some(_), None, Some(overlay)) => overlay.contexts.len(),
        _ => 0,
    }
}

/// What holds [`live_contexts`], for the model to keep until the next update compares against it.
fn live_holder(conversation: Option<&str>, run: Option<&Arc<RunView>>, overlay: Option<&Arc<TaskOverlayView>>, props: &ConversationViewProps) -> LiveHolder {
    match (conversation, run, overlay) {
        (None, _, _) => LiveHolder::Contexts(props.contexts.clone()),
        (Some(_), Some(run), _) => LiveHolder::Run(run.clone()),
        (Some(_), None, Some(overlay)) => LiveHolder::Overlay(overlay.clone()),
        (Some(_), None, None) => LiveHolder::None,
    }
}

/// The overlay of a conversation with no run. With a run, the run's view carries it.
fn task_overlay_of(app: &mut App, conversation: Option<&str>, run: Option<&RunView>) -> Option<Arc<TaskOverlayView>> {
    if run.is_some() {
        return None;
    }
    conversation.and_then(|id| app.task_overlay(id))
}

/// The timeline itself.
pub fn context_stream(app: &mut App, props: Rc<ConversationViewProps>) -> E {
    let t_ = theme();
    let timeline_id = props.timeline_id.clone().unwrap_or_else(|| "timeline".to_owned());
    let shared = timeline_state(app, &timeline_id);
    let scroller = hash_key(("context-scroll", timeline_id.as_str()));
    let read_only = !props.editable;
    let mutation_read_only = read_only || props.timeline_mutation_locked;
    let selecting = props.on_delete_contexts.is_some() && !mutation_read_only;
    let serif = app.appearance_snapshot().serif_messages;
    let (render_user_markdown, collapse_reasoning, single_dollar_math, message_size) = {
        let appearance = app.appearance_snapshot();
        (appearance.render_user_markdown, appearance.collapse_reasoning, appearance.single_dollar_math, mewrk_client::appearance::clamp_message_font_size(f64::from(appearance.message_font_size)) as f32)
    };
    let message_ty = TypeStyle { size: message_size, line: (message_size * 1.58).round(), weight: Weight::Regular, mono: false, italic: false, serif };

    // ---- what to read the contexts from, once for the model and once for the rows, with `&mut app` free in between.
    let conversation = props.conversation_id.clone();
    let run: Option<Arc<RunView>> = conversation.as_deref().and_then(|id| app.run_view(id));
    let overlay: Option<Arc<TaskOverlayView>> = task_overlay_of(app, conversation.as_deref(), run.as_deref());
    let draft: Option<Vec<ContextItem>> = conversation.as_deref().filter(|id| mewrk_client::drafts::is_draft_conversation_id(id)).map(|id| app.timeline_contexts_of(id));

    let frame_view = rect_of(scroller);
    let view = frame_view.unwrap_or(rect(0.0, 0.0, window_size().0, window_size().1));
    let (mut offset, content_h, viewport) = scroll_state(scroller).unwrap_or((0.0, 0.0, view.h));
    let viewport = if viewport > 0.0 { viewport } else { 1200.0 };
    let column_width = column_width(app);
    let width = (view.w - 48.0).clamp(160.0, column_width);
    let mut shift = 0.0f32;

    // A timeline just come back to, or never shown, starts over on what is transient. A selection belongs to the timeline it was drawn on.
    let mounted = use_state(("tl-mounted", scroller), || false);
    if !mounted.get() {
        mounted.set(true);
        let mut state = shared.borrow_mut();
        state.selection.clear();
        state.marquee = None;
        state.marquee_box = None;
        state.drag = None;
        state.drag_target = None;
        state.settling = None;
        state.slides.clear();
        state.unit_slides.clear();
        state.menu = None;
        state.follow.gliding = false;
        // A body is known by its address, and one freed on the way out can hand that on to the next one drawn here.
        state.fingerprint = 0;
        state.memo.forget();
    }

    let (first, last, count_contexts, anchor_position, build_window);
    {
        let persisted: &[ContextItem] = match (&draft, conversation.as_deref()) {
            (Some(draft), _) => draft,
            (None, Some(id)) => app.conversation(id).map_or(&[][..], |conversation| conversation.contexts.as_slice()),
            _ => &[],
        };
        let live: &[LiveContext] = live_contexts(conversation.as_deref(), run.as_deref(), overlay.as_deref(), &props);
        let updated = conversation.as_deref().and_then(|id| app.conversation(id)).map(|conversation| conversation.updated_at.as_str());
        // In parts, so that a commit which only moved the live contexts or the turns can be told from any other change.
        let parts = FingerprintParts {
            persisted: hash_key((persisted.as_ptr() as usize, persisted.len(), updated)),
            live: hash_key((run.as_ref().map(|run| run.revision), overlay.as_ref().map(|overlay| overlay.revision))),
            turns: turns_part(&props),
            rest: rest_part(&props),
        };
        let mut guard = shared.borrow_mut();
        let state = &mut *guard;
        state.turns = props.turns.clone();
        if state.fingerprint != parts.combined() || state.map.is_empty() != (persisted.is_empty() && live.is_empty()) {
            let before_order = state.node_keys.clone();
            let anchor_key = state.anchor_key;
            let anchor_top = anchor_key_top(state, anchor_key);
            let in_place = live_in_place(conversation.as_deref(), run.as_deref(), overlay.as_deref());
            let source = ModelSource { persisted, live: live_holder(conversation.as_deref(), run.as_ref(), overlay.as_ref(), &props), in_place, parts };
            update_model(state, source, &props, width, message_size);
            // A drag measures the cards it found when it began, so a timeline whose order changes under it ends it.
            if before_order != state.node_keys {
                state.drag = None;
                state.drag_target = None;
                // What was laid out last frame was laid out for the drag that just ended, and is not what these rows measure at rest.
                if state.settling.is_some() {
                    state.built.clear();
                }
            }
            // Contexts pasted or removed above the reader move nothing on screen.
            if let (Some(before), Some(after)) = (anchor_top, anchor_key_top(state, anchor_key))
                && !state.follow.pinned
            {
                shift += after - before;
            }
        }
        let contexts = Resolved { persisted, live, map: state.map.clone() };
        let count = contexts.count();

        // ---- what scrolled since last frame
        let follow = &mut state.follow;
        // A glide is the scroller's to carry on, through frames that only scroll, and to end: at the end, or when the wheel or the
        // scrollbar takes over.
        follow.gliding &= scroll_gliding(scroller);
        let same_timeline = follow.scroll_timeline.as_deref() == Some(timeline_id.as_str());
        let distance = content_h - offset - viewport;
        let scrolled = (offset - follow.expected_top).abs() > 0.5 || !same_timeline;
        if scrolled && frame_view.is_some() {
            // Resting at the bottom re-attaches; any upward move detaches, so one wheel notch escapes follow-output instead of being pulled back.
            if distance <= SCROLL_BOTTOM_EPSILON {
                follow.pinned = true;
            } else if !same_timeline {
                follow.pinned = distance < 180.0;
            } else if offset < follow.expected_top {
                follow.pinned = false;
                follow.gliding = false;
                follow.jump = false;
            }
            follow.scroll_timeline = Some(timeline_id.clone());
        }
        // The reader is heading up: let go of the bottom before the next frame pulls them back.
        // Only one that moved the timeline itself: a code block or an output pane with room of its own takes the wheel first.
        if std::mem::take(&mut follow.wheel_up) && offset < follow.expected_top - 0.01 {
            follow.pinned = false;
            follow.gliding = false;
            follow.jump = false;
        }

        // ---- transient state against what can still be acted on
        if !selecting {
            state.selection.clear();
            state.marquee = None;
            state.marquee_box = None;
            state.drag = None;
            state.drag_target = None;
        }
        if mutation_read_only {
            state.menu = None;
        }
        if state.selection.is_empty() && state.menu.as_ref().is_some_and(|menu| menu.view == MenuView::Selection) {
            state.menu = None;
        }
        // The anchor has left: a resize moved it, or what is underneath moved.
        if state.menu.as_ref().is_some_and(|menu| menu.window != window_size() || (menu.offset - offset).abs() > 1.0) {
            state.menu = None;
        }

        // ---- heights measured last frame
        let anchor = state.tops.partition_point(|top| *top <= (offset - PAD_TOP).max(0.0)).saturating_sub(1).min(state.nodes.len().saturating_sub(1));
        let anchor_top_before = state.tops.get(anchor).copied().unwrap_or(0.0);
        let anchor_node_key = state.node_keys.get(anchor).copied().unwrap_or(0);
        let mut changed = false;
        let mut first_measure = false;
        let mut moved: Vec<u64> = Vec::new();
        let rows_key = hash_key((width.round() as i32, message_size.to_bits(), serif, collapse_reasoning, single_dollar_math, render_user_markdown, mewrk_client::i18n::is_english()));
        if state.rows_key != rows_key {
            state.rows_key = rows_key;
            state.rows_measured.clear();
        }
        for position in std::mem::take(&mut state.built) {
            let Some(&key) = state.node_keys.get(position) else { continue };
            let Some(r) = rect_of(row_key(0, key)) else { continue };
            let held = state.heights.get(&key).copied();
            // What a drag has out of a row, or set down in it, is not what the row measures at rest.
            let at_rest = r.h + state.adjust.get(&position).copied().unwrap_or(0.0);
            if held.is_none_or(|held| (held - at_rest).abs() > 0.25) {
                first_measure |= held.is_none();
                state.heights.insert(key, at_rest);
                changed = true;
                moved.push(key);
            }
            match &state.nodes[position] {
                RenderNode::Block { entries, .. } => {
                    for entry in entries {
                        let id = contexts.get(entry.index).id();
                        if let Some(r) = rect_of(unit_key(0, id)) {
                            state.unit_heights.insert(hash_id(id), r.h);
                            state.rows_measured.insert(hash_id(id));
                        }
                    }
                }
                node => {
                    let id = contexts.get(node.first_index()).id();
                    if let Some(r) = rect_of(unit_key(0, id)) {
                        state.unit_heights.insert(hash_id(id), r.h);
                    }
                }
            }
        }
        // The dock as it really came out, which what is laid out beside it has to leave room for.
        if let Some(r) = rect_of(("tl-dock", scroller))
            && state.dock.is_some()
            && state.dock_measured.is_none_or(|known| (known - r.h).abs() > 0.25)
        {
            state.dock_measured = Some(r.h);
            state.layout_dirty = true;
        }
        if changed || state.layout_dirty {
            // The rows above the first one whose height was measured anew stay where they are, unless something else moved them.
            let from = if state.layout_dirty { 0 } else { first_row_of(state, &moved) };
            relayout_from(state, &contexts, width, message_size, from);
            if !state.follow.pinned {
                // What changed above the reader stays above them.
                shift += state.tops.get(anchor).copied().unwrap_or(0.0) - anchor_top_before;
            } else if first_measure {
                state.follow.jump = true;
            }
        }
        state.anchor_key = anchor_node_key;

        // ---- a fold that left the reader below what it folded
        if let Some((toggle, frames)) = state.pending_reveal.take() {
            if frames > 0 {
                state.pending_reveal = Some((toggle, frames - 1));
                repaint();
            } else if let Some(r) = rect_of(toggle) {
                if r.y < view.y {
                    shift += r.y - view.y - 8.0;
                } else if r.bottom() > view.bottom() {
                    shift += r.bottom() - view.bottom() + 8.0;
                }
            }
        }

        // ---- the marquee and the drag
        step_marquee(state, &contexts, view, offset + shift, false, &mut shift);
        let now = Instant::now();
        state.slides.retain(|_, slide| !slide.done(now));
        state.unit_slides.retain(|_, slide| !slide.done(now));
        if let Some(mut settling) = state.settling.take() {
            shift += settle(state, &contexts, &settling, view, offset + shift);
            settling.frames -= 1;
            if settling.frames > 0 {
                state.settling = Some(settling);
            }
            repaint();
        }
        let mut wants_frame = false;
        let drag_shift = with_drag(state, &contexts, view, offset + shift, width, message_size, |drag, geometry| {
            if drag.session.is_none() && drag.crossed_threshold() {
                let units = timeline_units(contexts_of(geometry), &geometry.state.nodes, &geometry.state.selection);
                geometry.spans = unit_spans(geometry.state, &units, None);
                let press = drag.press;
                let grabbed_key = |unit: usize| rect_of(unit_key(0, &units[unit].id)).is_some_and(|r| r.contains(press.0, press.1));
                let unit_box = |unit: usize| rect_of(unit_key(0, &units[unit].id)).map_or((view.x + (view.w - width) * 0.5, width), |r| (r.x, r.w));
                drag.session = start_session(units.clone(), press, geometry, grabbed_key, unit_box, fluent::reduced_motion());
                if let Some(session) = &mut drag.session {
                    // A drag gathers as it begins: it is at the home line, with nothing left to count down.
                    let pointer = drag.pointer;
                    session.set_look(geometry, Look::Gathered, pointer, now);
                    geometry.state.menu = None;
                    wants_frame = true;
                }
            }
            if let Some(session) = &mut drag.session {
                wants_frame |= session.step(geometry, drag.pointer, false, now);
                geometry.state.drag_target = session.drawn;
            }
        });
        shift += drag_shift;
        if wants_frame {
            repaint();
        }

        // ---- follow-output
        let last_context = (count > 0).then(|| contexts.get(count - 1));
        let next = PreviousTimeline { timeline_id: Some(timeline_id.clone()), length: count, last_id: last_context.map(|item| item.id().to_owned()), last_visual_length: context_visual_length(last_context) };
        // Layout rounds every row, so the page ends a pixel or two from where the stored heights say. A frame in which no height
        // changed says by how much; one in which some did says nothing about it.
        if !changed && frame_view.is_some() && content_h > 0.0 {
            state.rounding = (content_h - state.follow.content).clamp(-16.0, 16.0);
        }
        let bottom = (PAD_TOP + state.total + PAD_BOTTOM + state.rounding - viewport).max(0.0);
        let menu_open = state.menu.is_some();
        let dragging = state.drag.as_ref().is_some_and(PressDrag::active);
        let follow = &mut state.follow;
        let previous = std::mem::replace(&mut follow.previous, Some(next.clone()));
        let timeline_changed = previous.as_ref().is_some_and(|previous| previous.timeline_id != next.timeline_id);
        // The owning surface restores its saved position after a switch. Do not reinterpret the new timeline as appended output.
        if timeline_changed || count == 0 || menu_open || !follow.pinned || dragging {
            follow.gliding = false;
            follow.jump = false;
        } else {
            let initial = previous.is_none();
            // Appended means the timeline has a new tail. Contexts pasted or placed further up make it longer too, and following those would carry the reader off.
            let appended = previous.as_ref().is_some_and(|previous| next.length > previous.length && next.last_id != previous.last_id);
            let growing = previous.as_ref().is_some_and(|previous| next.length == previous.length && next.last_id == previous.last_id && next.last_visual_length > previous.last_visual_length);
            let streaming_update = previous.as_ref().is_some_and(|previous| props.streaming && next.length >= previous.length && next.last_id == previous.last_id && *previous != next);
            let changed_contexts = previous.as_ref() != Some(&next);
            if initial || appended || growing || streaming_update {
                // What a stream writes glides, and so does whatever lands while a glide is still moving. Anything else jumps.
                if !initial && (props.streaming || follow.gliding) {
                    follow.gliding = true;
                } else {
                    follow.jump = true;
                }
            } else if changed_contexts {
                // A glide may finish the distance it was already covering, but never past a tail that went away.
                if previous.as_ref().is_some_and(|previous| next.length < previous.length) {
                    follow.gliding = false;
                }
            }
            // Commits are not the only thing that grows a live page: a body opening under its row, the waiting line changing shape.
            let grew = state.total + PAD_TOP + PAD_BOTTOM > follow.content + 0.5;
            if props.streaming && grew && same_timeline {
                follow.gliding = true;
            }
            let start = offset + shift;
            // Pinned to the bottom is at the bottom: what rounding leaves over is not the reader having moved.
            if !follow.gliding && (bottom - start).abs() > 0.5 && (bottom - start).abs() <= SCROLL_BOTTOM_EPSILON {
                follow.jump = true;
            }
            if std::mem::take(&mut follow.jump) || (follow.gliding && fluent::reduced_motion()) {
                follow.gliding = false;
                shift += bottom - start;
            }
        }
        // The glide itself is the scroller's: it carries it on to wherever the end has got to in frames that only scroll, so a
        // reply taller than the window is followed without running the views. Asking for it again goes on at its speed.
        if follow.gliding {
            scroll_request(scroller, ScrollRequest::GlideToEnd { rate: FOLLOW_GLIDE_RATE });
        } else if scroll_gliding(scroller) {
            scroll_request(scroller, ScrollRequest::StopGlide);
        }
        follow.content = state.total + PAD_TOP + PAD_BOTTOM;
        follow.expected_top = (offset + shift).clamp(0.0, bottom.max(offset + shift));
        follow.scroll_timeline = Some(timeline_id.clone());
        offset += shift;

        // ---- which rows to build
        let top = (offset - PAD_TOP - OVERSCAN).max(0.0);
        let bottom_edge = offset - PAD_TOP + viewport + OVERSCAN;
        let nodes_count = state.nodes.len();
        let f_first = state.tops[..nodes_count].partition_point(|y| *y <= top).saturating_sub(1);
        let f_last = state.tops[..nodes_count].partition_point(|y| *y < bottom_edge).max(f_first.min(nodes_count));
        first = f_first.min(nodes_count);
        last = f_last.min(nodes_count);
        build_window = (top, bottom_edge);
        // Only these rows are built, so a frame that only scrolls goes no further than them.
        let covered_top = if top <= 0.0 { f32::NEG_INFINITY } else { PAD_TOP + top };
        let covered_bottom = if bottom_edge >= state.total { f32::INFINITY } else { PAD_TOP + bottom_edge };
        scroll_coverage(scroller, covered_top, covered_bottom);
        count_contexts = count;
        // Rows above this are entirely above the reader.
        anchor_position = state.tops[..nodes_count].partition_point(|y| *y <= (offset - PAD_TOP).max(0.0)).saturating_sub(1);
    }
    if shift.abs() > 0.01 {
        scroll_request(scroller, ScrollRequest::Shift(shift));
    }
    // `contextScrollRef`: the owning surface's record of where this timeline was left.
    if let Some(id) = conversation.as_deref()
        && frame_view.is_some()
    {
        app.set_context_scroll(id, offset);
    }

    // ---- what the rows need from `&mut app`, before the contexts are borrowed again
    let mut explanations: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    {
        let state = shared.borrow();
        let persisted_ids: Vec<(String, Option<String>)> = {
            let persisted: &[ContextItem] = match (&draft, conversation.as_deref()) {
                (Some(draft), _) => draft,
                (None, Some(id)) => app.conversation(id).map_or(&[][..], |conversation| conversation.contexts.as_slice()),
                _ => &[],
            };
            let live: &[LiveContext] = live_contexts(conversation.as_deref(), run.as_deref(), overlay.as_deref(), &props);
            let contexts = Resolved { persisted, live, map: state.map.clone() };
            state.nodes[first..last]
                .iter()
                .flat_map(RenderNode::indexes)
                .filter_map(|index| contexts.get(index).tool().map(|tool| (tool.id.to_owned(), tool.provider_call())))
                .collect()
        };
        drop(state);
        for (id, call) in persisted_ids {
            let explanation = app.tool_explanation(&id, conversation.as_deref(), call.as_deref());
            let error = app.tool_error_explanation(&id);
            if explanation.is_some() || error.is_some() {
                explanations.insert(id, (explanation, error));
            }
        }
    }

    // ---- the rows
    let persisted: &[ContextItem] = match (&draft, conversation.as_deref()) {
        (Some(draft), _) => draft,
        (None, Some(id)) => app.conversation(id).map_or(&[][..], |conversation| conversation.contexts.as_slice()),
        _ => &[],
    };
    let live: &[LiveContext] = live_contexts(conversation.as_deref(), run.as_deref(), overlay.as_deref(), &props);
    let (map, selection, menu, marquee_box, drag_target) = {
        let state = shared.borrow();
        (state.map.clone(), state.selection.clone(), state.menu.clone(), state.marquee_box, state.drag_target)
    };
    let contexts = Resolved { persisted, live, map };
    let insertion_index = drag_target.or(menu.as_ref().filter(|menu| menu.view == MenuView::Insert).map(|menu| menu.index));
    let (rendered, projection_empty, leading, any_running) = {
        let state = shared.borrow();
        (state.rendered.clone(), state.nodes.is_empty(), state.projection.leading_turns.clone(), state.projection.any_visible_running)
    };
    let display_insertion = insertion_index.map(|index| display_insertion_index(&rendered, index, count_contexts));
    let editing_context_id = (!mutation_read_only).then(|| match &props.editor {
        Some(TimelineEditorState::Edit { item, .. }) => Some(item.id().to_owned()),
        _ => None,
    }).flatten();
    let editing_question_id = (!mutation_read_only).then(|| props.question_editor.as_ref().map(|editor| editor.item.id().to_owned())).flatten();
    // The insert editor snaps to the same drawn row the insertion line does, so an index that addresses a projected or undrawn
    // context still lands somewhere visible. Only the editor moves: `editor.index` still decides where the new context is spliced in.
    let insert_editor_index = match &props.editor {
        Some(TimelineEditorState::Insert { index, .. }) if !mutation_read_only && props.on_cancel_edit.is_some() => Some(display_insertion_index(&rendered, *index, count_contexts)),
        _ => None,
    };
    let (lifted, dimmed, session_view) = {
        let state = shared.borrow();
        match state.drag.as_ref().and_then(|drag| drag.session.as_ref()) {
            Some(session) if session.look == Look::Apart => (HashSet::new(), session.lifted_ids.clone(), Some(session.clone())),
            Some(session) => (session.lifted_ids.clone(), HashSet::new(), Some(session.clone())),
            None => (HashSet::new(), HashSet::new(), None),
        }
    };
    let open_insert: OpenInsert = {
        let shared = shared.clone();
        Rc::new(move |_app: &mut App, event: PointerEvent, index: usize| {
            if mutation_read_only {
                return;
            }
            let mut state = shared.borrow_mut();
            let offset = scroll_state(scroller).map_or(0.0, |s| s.0);
            // With contexts picked out, a right-click anywhere is about them, not about inserting beside whatever is under the pointer.
            let view = if selecting && !state.selection.is_empty() { MenuView::Selection } else { MenuView::Insert };
            state.menu = Some(MenuState { x: event.x, y: event.y, index, view, window: window_size(), offset });
            repaint();
        })
    };
    let f = Frame {
        contexts: contexts.clone(),
        props: &props,
        shared: &shared,
        read_only,
        mutation_read_only,
        render_user_markdown,
        collapse_reasoning,
        single_dollar_math,
        message_ty,
        explanations: &explanations,
        selected: &selection,
        editing_context_id: editing_context_id.as_deref(),
        editing_question_id: editing_question_id.as_deref(),
        insert_editor_index,
        display_insertion_index: if mutation_read_only { None } else { display_insertion },
        open_insert: open_insert.clone(),
        column_w: width,
        salt: 0,
        lifted: &lifted,
        dimmed: &dimmed,
        dock_inside: session_view.as_ref().and_then(|session| match shared.borrow().dock {
            Some(DockRoom { at: DockAt::Inside { before, .. }, height, .. }) => Some((session.units[before].index, height)),
            _ => None,
        }),
        window: Some(build_window),
    };

    let insert_editor_exists = matches!(&props.editor, Some(TimelineEditorState::Insert { .. })) && props.on_cancel_edit.is_some();
    let mut column = col().w_full().min_w(0.0).pt(PAD_TOP).with_style(|s| s.min_size.height = taffy::prelude::percent(1.0f32));
    let empty = !insert_editor_exists && projection_empty && leading.is_empty();
    if empty {
        column = column.pb(28.0);
        if let Some(slot) = props.empty_state.clone() {
            // Built with the contexts released: the caller's slot may want the app.
            let waiting = (props.streaming && !any_running).then(|| waiting_indicator(&f, "end"));
            drop(f);
            let slot_view = slot(app);
            let column = column.child(slot_view).child(waiting);
            return finish_stream(&props, &shared, scroller, column, column_width, read_only, selecting, mutation_read_only, menu, marquee_box, open_insert, None, count_contexts);
        }
        column = column.child(stock_empty_state(mutation_read_only));
        if props.streaming && !any_running {
            column = column.child(waiting_indicator(&f, "end"));
        }
    } else {
        column = column.pb(PAD_BOTTOM);
        {
            let state = shared.borrow();
            let turns = state.turns.clone();
            drop(state);
            for at in &leading {
                column = column.children(turn_notices(&f, &turns[*at], false));
            }
        }
        // The spacer standing for the rows above, then the rows, then the spacer for those below.
        let (top_spacer, row_heights): (f32, Vec<f32>) = {
            let state = shared.borrow();
            (state.tops.get(first).copied().unwrap_or(0.0), state.row_h[first..last].to_vec())
        };
        column = column.child(div().h(top_spacer).shrink0());
        let mut built = Vec::with_capacity(last - first);
        let node_hidden = |height: f32| height <= 0.0 && session_view.is_some();
        let dock = shared.borrow().dock;
        let slide_now = Instant::now();
        for (at, position) in (first..last).enumerate() {
            let height = row_heights[at];
            // The room the pieces are set down in; the spacer above already holds it when it opens the window.
            if let Some(room) = dock
                && room.at == DockAt::Before(position)
                && position > first
            {
                column = column.child(div().h(room.height).shrink0());
            }
            if node_hidden(height) {
                continue;
            }
            built.push(position);
            let mut view = row_view(&f, position);
            // A slide is only drawn: the layout already has the row where it is going.
            let key = shared.borrow().node_keys[position];
            // A row never measured stands at an estimate until the next frame measures it and moves what is around it. That has to
            // be a frame that runs the views, not one that only scrolls on to what the estimate got wrong.
            if !shared.borrow().heights.contains_key(&key) {
                repaint();
            }
            let slide = shared.borrow().slides.get(&key).copied();
            if let Some(slide) = slide {
                view = drawn_slide(view, &slide, slide_now);
                repaint();
            }
            // A row entirely above the reader takes up its stored height, whatever it really measures this frame: the
            // correction and the scroll offset that hides it land together, next frame.
            column = column.child(if position < anchor_position {
                div().h(height).clip().shrink0().w_full().child(view)
            } else {
                view
            });
        }
        // An editor scrolled far from the view is still open: what was typed into it lives as long as the view keeps asking for it,
        // and upstream's stays mounted.
        {
            let state = shared.borrow();
            let node_of = |index: usize| state.node_of_context.get(index).copied().filter(|node| *node != u32::MAX).map(|node| node as usize);
            let editing = editing_context_id.as_deref().and_then(|id| state.context_index.get(&hash_id(id))).and_then(|index| node_of(*index as usize));
            let question = editing_question_id.as_deref().and_then(|id| state.context_index.get(&hash_id(id))).and_then(|index| node_of(*index as usize));
            let inserting = insert_editor_index.and_then(node_of);
            let keep: Vec<usize> = [editing, question, inserting].into_iter().flatten().filter(|position| !(first..last).contains(position)).collect();
            drop(state);
            for position in keep {
                let _ = row_view(&f, position);
            }
        }
        if insert_editor_index == Some(count_contexts) && last != shared.borrow().nodes.len() {
            let _ = timeline_insert_editor(&f);
        }
        let (below_spacer, ends_stream) = {
            let state = shared.borrow();
            let drawn_to = state.tops.get(last).copied().unwrap_or(state.total);
            (state.total - drawn_to, last == state.nodes.len())
        };
        shared.borrow_mut().built = built;
        column = column.child(div().h(below_spacer.max(0.0)).shrink0());
        if ends_stream {
            if let Some(room) = dock
                && room.at == DockAt::End
            {
                column = column.child(div().h(room.height).shrink0());
            }
            if props.streaming && !any_running {
                column = column.child(waiting_indicator(&f, "end"));
            }
            if insert_editor_index == Some(count_contexts) {
                column = column.child(timeline_insert_editor(&f));
            }
            if !mutation_read_only && display_insertion == Some(count_contexts) && !projection_empty {
                column = column.child(insertion_line_view());
            }
        }
    }
    // The home line, and the pieces as they ride or as they are set down.
    let mut ghost: Option<Vec<E>> = None;
    if let Some(session) = &session_view {
        let now = Instant::now();
        let y = session.mark_drawn(now);
        let (color, progress) = match session.mark_state {
            MarkState::At => (t_.accent, None),
            MarkState::Away => (t_.accent.fade(0.35), None),
            MarkState::Counting => (t_.accent.fade(0.35), session.dock_progress(now)),
        };
        let mut mark = div().absolute().top((y + PAD_TOP).round() - 2.0).left(0.0).right(0.0).h(2.0).clip().bg(color).pass_through();
        if let Some(progress) = progress {
            mark = mark.child(div().h(2.0).w_pct(progress * 100.0).bg(t_.accent));
        }
        column = column.child(mark);
        let nothing = HashSet::new();
        let copies = |salt: u64| Frame { contexts: contexts.clone(), props: &props, shared: &shared, read_only: true, mutation_read_only: true, render_user_markdown, collapse_reasoning, single_dollar_math, message_ty, explanations: &explanations, selected: &selection, editing_context_id: None, editing_question_id: None, insert_editor_index: None, display_insertion_index: None, open_insert: open_insert.clone(), column_w: width, salt, lifted: &nothing, dimmed: &nothing, dock_inside: None, window: None };
        if session.look == Look::Docked {
            column = column.child(dock_view(&copies(DOCK_SALT), session, hash_key(("tl-dock", scroller)), session.mark_y - view.y + offset));
        }
        if session.ghost_shown(now) {
            let copies = copies(GHOST_SALT);
            // Escape reaches the topmost overlay first, which is the stack; what it is told to dismiss is the drag.
            let cancel_drag: Handler<App> = {
                let shared = shared.clone();
                Rc::new(move |_| call_off_drag(&shared, scroller, Instant::now()))
            };
            let (stack_x, stack_y) = session.layer;
            let mut layers = Vec::new();
            for entry in &session.ghost {
                let ((x, y, height), fade) = entry.frame(now);
                let cancel = cancel_drag.clone();
                let foot = entry.clipped.then(|| div().absolute().left(0.0).right(0.0).bottom(0.0).h(30.0).gradient(t_.flyout.fade(0.0), t_.flyout).pass_through());
                layers.push(col().w(session.piece_boxes[entry.piece].width).h(height.max(1.0)).clip().radius(10.0).when(fade.is_some(), |e| e.opacity(fade.unwrap_or(1.0))).bg(t_.flyout).border(t_.flyout_stroke).inert().aria_hidden(true).child(piece_view(&copies, session, entry.piece)).child(foot).popover_at(stack_x + x, stack_y + y, move |app| cancel(app)));
            }
            if session.ghost_more > 0
                && let Some(last_entry) = session.ghost.last()
            {
                let cancel = cancel_drag.clone();
                layers.push(row().px(8.0).radius(999.0).bg(t_.accent).pass_through().aria_hidden(true).child(text(format!("+{}", session.ghost_more)).ty(ty_small()).color(t_.text_on_accent).nowrap()).popover_at(stack_x + last_entry.x, stack_y + last_entry.y + last_entry.height + 6.0, move |app| cancel(app)));
            }
            ghost = Some(layers);
        }
        // Nothing under a drag answers the pointer: no card lights up as the stack passes over it.
        column = column.inert();
    }
    drop(f);
    finish_stream(&props, &shared, scroller, column, column_width, read_only, selecting, mutation_read_only, menu, marquee_box, open_insert, ghost, count_contexts)
}

fn ty_small() -> TypeStyle {
    TypeStyle { size: 11.0, line: 18.0, weight: Weight::Semibold, mono: false, italic: false, serif: false }
}

fn contexts_of<'a, 'b>(geometry: &Geometry<'a, 'b>) -> &'a Resolved<'b> {
    geometry.contexts
}

/// Where the row a key names starts, when it is still on the timeline.
fn anchor_key_top(state: &TimelineState, key: u64) -> Option<f32> {
    let position = state.node_keys.iter().position(|held| *held == key)?;
    state.tops.get(position).copied()
}

/// `stepMarquee`: redraws the selection box from where the drag began to where the pointer is, and picks out what it touches.
/// Held against the top or bottom edge, the drag scrolls the timeline on, so a box can reach past what was on screen when it began.
fn step_marquee(state: &mut TimelineState, contexts: &Resolved<'_>, view: Rect, offset: f32, final_step: bool, shift: &mut f32) {
    let Some(mut drag) = state.marquee.take() else { return };
    let start_y_now = |offset: f32| drag.start_content_y - offset + view.y;
    // Until the press has travelled it is a click, which picks at its release and draws nothing.
    drag.moved |= (drag.x - drag.start_x).hypot(drag.y - start_y_now(offset)) >= MARQUEE_THRESHOLD;
    if !drag.moved {
        state.marquee = Some(drag);
        return;
    }
    let step = if final_step {
        0.0
    } else if drag.y < view.y + MARQUEE_SCROLL_EDGE {
        -MARQUEE_SCROLL_STEP.min(view.y + MARQUEE_SCROLL_EDGE - drag.y)
    } else if drag.y > view.bottom() - MARQUEE_SCROLL_EDGE {
        MARQUEE_SCROLL_STEP.min(drag.y - (view.bottom() - MARQUEE_SCROLL_EDGE))
    } else {
        0.0
    };
    let max_offset = (PAD_TOP + state.total + PAD_BOTTOM - view.h).max(0.0);
    let next = (offset + step).clamp(0.0, max_offset);
    *shift += next - offset;
    if (next - offset).abs() > 0.01 {
        repaint();
    }
    let start_y = start_y_now(next);
    let (left, right) = (drag.start_x.min(drag.x), drag.start_x.max(drag.x));
    let (top, bottom) = (start_y.min(drag.y), start_y.max(drag.y));
    let shown_left = left.max(view.x);
    let shown_top = top.max(view.y);
    state.marquee_box = Some(rect(shown_left, shown_top, (right.min(view.right()) - shown_left).max(0.0), (bottom.min(view.bottom()) - shown_top).max(0.0)));
    // The modifier is held for every box, so a box adds to what was picked out before it.
    let hit = ids_in_box(state, contexts, view, next, left, right, top, bottom);
    let mut picked = drag.base.clone();
    picked.extend(hit);
    state.selection = picked;
    state.marquee = Some(drag);
}

/// `idsInBox`: the contexts whose cards or rows a rectangle in window coordinates touches. What is built answers exactly; what
/// is not is reckoned from the stored heights, across the whole column.
#[allow(clippy::too_many_arguments)]
fn ids_in_box(state: &TimelineState, contexts: &Resolved<'_>, view: Rect, offset: f32, left: f32, right: f32, top: f32, bottom: f32) -> HashSet<String> {
    let units = timeline_units(contexts, &state.nodes, &HashSet::new());
    let spans = unit_spans(state, &units, None);
    let mut ids = HashSet::new();
    for (unit, (span_top, span_bottom)) in units.iter().zip(spans) {
        let hit = match rect_of(unit_key(0, &unit.id)) {
            Some(r) if r.w > 0.0 || r.h > 0.0 => !(r.right() < left || r.x > right || r.bottom() < top || r.y > bottom),
            _ => {
                let (y0, y1) = (view.y + span_top - offset, view.y + span_bottom - offset);
                !(y1 < top || y0 > bottom)
            }
        };
        if hit {
            ids.extend(unit.ids.iter().cloned());
        }
    }
    ids
}

/// `toggleAt`: a click with the selection modifier held picks the context under it out, or lets it go if it was picked
/// already, and leaves the rest of the selection as it was: how contexts that do not sit together are picked.
fn toggle_at(state: &mut TimelineState, contexts: &Resolved<'_>, view: Rect, offset: f32, drag: &MarqueeDrag) {
    let hit = ids_in_box(state, contexts, view, offset, drag.x, drag.x, drag.y, drag.y);
    if hit.is_empty() {
        return;
    }
    let let_go = hit.iter().all(|id| drag.base.contains(id));
    let mut picked = drag.base.clone();
    for id in hit {
        if let_go {
            picked.remove(&id);
        } else {
            picked.insert(id);
        }
    }
    state.selection = picked;
}

/// The scroller and all that hangs from it.
#[allow(clippy::too_many_arguments)]
fn finish_stream(props: &Rc<ConversationViewProps>, shared: &Shared, scroller: u64, column: E, wide: f32, read_only: bool, selecting: bool, mutation_read_only: bool, menu: Option<MenuState>, marquee_box: Option<Rect>, open_insert: OpenInsert, ghost: Option<Vec<E>>, count_contexts: usize) -> E {
    let t_ = theme();
    let context_order = Rc::new(shared.borrow().context_index.clone());
    let can_undo = props.on_undo.is_some() || props.on_redo.is_some() || selecting;
    let body = row().w_full().justify_center().px(24.0).child(column.with_style(|s| s.max_size.width = taffy::prelude::length(wide)));
    let mut scroll = col().key(scroller).flex1().min_w(0.0).min_h(0.0).scroll_manual().child(body);
    if shared.borrow().drag.as_ref().is_some_and(PressDrag::active) {
        scroll = scroll.cursor(fluent::CursorKind::Grabbing);
    }
    if !read_only && can_undo {
        scroll = scroll.focusable().skip_tab(true);
    }
    // A press with the modifier is the box's; one on what is picked out may be the start of dragging it; any other lets the selection go.
    {
        let shared = shared.clone();
        let props = props.clone();
        scroll = scroll.on_press_capture(move |_: &mut App, event: PressEvent| {
            let mut state = shared.borrow_mut();
            // A press on the scrollbar is scrolling.
            let on_scrollbar = event.x > event.rect.right() - 12.0;
            if selecting && event.modifier && !event.on_editable && !on_scrollbar {
                clear_text_selection();
                state.menu = None;
                let offset = scroll_state(scroller).map_or(0.0, |s| s.0);
                let base = state.selection.clone();
                state.marquee = Some(MarqueeDrag { start_x: event.x, start_content_y: event.y - event.rect.y + offset, x: event.x, y: event.y, base, moved: false });
                state.marquee_box = None;
                return true;
            }
            if state.selection.is_empty() {
                return false;
            }
            if selecting && props.on_move_contexts.is_some() && !event.on_editable {
                let over_picked = state.selection.iter().any(|id| rect_of(unit_key(0, id)).is_some_and(|r| r.contains(event.x, event.y)));
                if over_picked {
                    clear_text_selection();
                    state.drag = Some(PressDrag::new(event.x, event.y));
                    return true;
                }
            }
            state.selection.clear();
            repaint();
            false
        });
    }
    // What the press became, followed to its release.
    {
        let shared = shared.clone();
        let props = props.clone();
        scroll = scroll.on_pointer(move |app: &mut App, event: PointerEvent| {
            let (contexts_snapshot, persisted);
            let conversation = props.conversation_id.clone();
            let draft = conversation.as_deref().filter(|id| mewrk_client::drafts::is_draft_conversation_id(id)).map(|id| app.timeline_contexts_of(id));
            let run = conversation.as_deref().and_then(|id| app.run_view(id));
            let overlay = task_overlay_of(app, conversation.as_deref(), run.as_deref());
            persisted = match (&draft, conversation.as_deref()) {
                (Some(draft), _) => draft.as_slice(),
                (None, Some(id)) => app.conversation(id).map_or(&[][..], |conversation| conversation.contexts.as_slice()),
                _ => &[],
            };
            let live: &[LiveContext] = live_contexts(conversation.as_deref(), run.as_deref(), overlay.as_deref(), &props);
            let mut state = shared.borrow_mut();
            contexts_snapshot = Resolved { persisted, live, map: state.map.clone() };
            let (offset, _, _) = scroll_state(scroller).unwrap_or((0.0, 0.0, 0.0));
            let view = event.rect;
            if let Some(marquee) = &mut state.marquee {
                marquee.x = event.x;
                marquee.y = event.y;
                repaint();
                if event.phase == Phase::Up {
                    step_marquee(&mut state, &contexts_snapshot, view, offset, true, &mut 0.0);
                    let drag = state.marquee.clone().expect("the step puts it back");
                    if !drag.moved {
                        toggle_at(&mut state, &contexts_snapshot, view, offset, &drag);
                    }
                    state.marquee = None;
                    state.marquee_box = None;
                }
                return;
            }
            let Some(drag) = &mut state.drag else { return };
            drag.pointer = (event.x, event.y);
            repaint();
            if event.phase != Phase::Up {
                return;
            }
            let width = (view.w - 48.0).clamp(160.0, wide);
            let mut outcome = DragOutcome::Click;
            let now = Instant::now();
            let shift = with_drag(&mut state, &contexts_snapshot, view, offset, width, 13.0, |drag, geometry| {
                if let Some(session) = &mut drag.session {
                    let pointer = drag.pointer;
                    session.step(geometry, pointer, true, now);
                    outcome = session.outcome();
                }
            });
            if let Some(session) = state.drag.take().and_then(|drag| drag.session) {
                let settling = settling_of(&state, &session, event.y, view, offset + shift, now);
                state.settling = Some(settling);
            }
            state.drag_target = None;
            state.layout_dirty = true;
            if shift.abs() > 0.01 {
                scroll_request(scroller, ScrollRequest::Shift(shift));
            }
            let ordered = ordered_selection(&state.selection, &state.context_index);
            match outcome {
                DragOutcome::Click => state.selection.clear(),
                DragOutcome::Cancel => {}
                DragOutcome::Drop(index) => {
                    drop(state);
                    if let Some(on_move) = &props.on_move_contexts {
                        on_move(app, ordered, index);
                    }
                }
            }
        });
    }
    if !mutation_read_only {
        let open = open_insert.clone();
        let positions = shared.clone();
        // A right-click on the timeline's own ground places against the nearest gap.
        scroll = scroll.on_context_menu(move |app: &mut App, event: PointerEvent| {
            let index = {
                let state = positions.borrow();
                let (offset, _, _) = scroll_state(scroller).unwrap_or((0.0, 0.0, 0.0));
                let content_y = event.y - event.rect.y + offset - PAD_TOP;
                let nearest = (0..state.nodes.len()).find(|&position| content_y < state.tops[position] + state.row_h[position] * 0.5);
                nearest.map_or(count_contexts, |position| state.nodes[position].first_index())
            };
            open(app, event, index);
        });
    }
    // Keys answer only while focus is somewhere in the timeline: the same keys typed into a field are the field's.
    let keys = {
        let shared = shared.clone();
        let props = props.clone();
        move |app: &mut App, press: &KeyPress| -> bool {
            let menu_open = shared.borrow().menu.is_some();
            if !menu_open && (!last_press_inside(scroller) || text_input_focused()) {
                return false;
            }
            let dragging = shared.borrow().drag.as_ref().is_some_and(PressDrag::active);
            if dragging {
                if press.key == Key::Escape {
                    call_off_drag(&shared, scroller, Instant::now());
                    return true;
                }
                return false;
            }
            let name = key_name(press.key);
            let timeline_press = name.as_deref().map(|key| TimelineKeyPress { key, ctrl: if IS_APPLE_PLATFORM { press.ctrl } else { press.cmd }, meta: if IS_APPLE_PLATFORM { press.cmd } else { false }, alt: press.alt, shift: press.shift });
            let clipboard_action = if selecting { timeline_press.and_then(|press| timeline_clipboard_key(press, IS_APPLE_PLATFORM)) } else { None };
            let (selection_empty, menu_index) = {
                let state = shared.borrow();
                (state.selection.is_empty(), state.menu.as_ref().filter(|menu| menu.view == MenuView::Insert).map(|menu| menu.index))
            };
            // Words selected in a message are what the copy keys copy; the contexts picked out are next.
            if clipboard_action == Some(TimelineClipboardAction::Copy) && !selection_empty && !has_text_selection() {
                if let Some(copy) = &props.on_copy_contexts {
                    let ordered = {
                        let mut state = shared.borrow_mut();
                        state.menu = None;
                        ordered_selection(&state.selection, &state.context_index)
                    };
                    copy(app, ordered);
                    return true;
                }
            }
            if clipboard_action == Some(TimelineClipboardAction::Paste) && props.paste_count > 0 {
                if let Some(paste) = &props.on_paste_contexts {
                    // Over what is picked out; failing that, at the line an open insert menu has drawn.
                    if !selection_empty {
                        let (index, ordered) = {
                            let mut state = shared.borrow_mut();
                            state.menu = None;
                            let ordered = ordered_selection(&state.selection, &state.context_index);
                            let index = ordered.first().and_then(|id| state.context_index.get(&hash_id(id)).copied()).unwrap_or(u32::MAX) as usize;
                            (index, ordered)
                        };
                        paste(app, index, ordered);
                        return true;
                    }
                    if let Some(index) = menu_index {
                        shared.borrow_mut().menu = None;
                        paste(app, index, Vec::new());
                        return true;
                    }
                }
            }
            if !read_only && let Some(press_) = timeline_press && let Some(action) = timeline_history_key(press_, IS_APPLE_PLATFORM) {
                let run = if action == TimelineHistoryAction::Undo { &props.on_undo } else { &props.on_redo };
                let Some(run) = run else { return false };
                run(app);
                return true;
            }
            if press.key == Key::Escape && !selection_empty && !menu_open {
                shared.borrow_mut().selection.clear();
                repaint();
                return true;
            }
            if !press.cmd && !press.alt && let Some((at, content, viewport)) = scroll_state(scroller) && let Some(delta) = scroll_key_delta(press.key, viewport, content) {
                scroll_request(scroller, ScrollRequest::By(delta));
                if delta < 0.0 && at > 0.0 {
                    let mut state = shared.borrow_mut();
                    state.follow.pinned = false;
                    state.follow.gliding = false;
                }
                return true;
            }
            false
        }
    };

    let mut root = col().flex1().min_w(0.0).min_h(0.0).child(clipboard_leaf()).on_key_global(keys);
    if let Some(label) = &props.aria_label {
        root = root.role(Role::Region).aria_label(label.as_str());
    }
    {
        // The wheel is only readable while painting; a turn upward has to let go of the bottom before the next frame pulls the reader back.
        let shared = shared.clone();
        root = root.child(
            custom::<App>(move |_, ui, _| {
                if ui.input.scroll > 0.0
                    && let Some(view) = rect_of(scroller)
                    && ui.input.pointer.is_some_and(|(x, y)| view.contains(x, y))
                {
                    shared.borrow_mut().follow.wheel_up = true;
                }
            })
            .size(1.0, 1.0)
            .absolute()
            .pass_through(),
        );
    }
    root = root.child(scroll);
    if let Some(r) = marquee_box {
        // `.timeline-marquee`: fixed to the window, and never in the way of the pointer.
        root = root.child(div().absolute().left(0.0).top(0.0).size(r.w, r.h).border(t_.accent).radius(3.0).pass_through().popover_at(r.x, r.y, |_| {}));
    }
    root = root.children(ghost.unwrap_or_default());
    if !mutation_read_only && let Some(menu) = menu {
        let closing = shared.clone();
        let on_close = move |_: &mut App| {
            closing.borrow_mut().menu = None;
            repaint();
        };
        let (sections, label) = match menu.view {
            MenuView::Insert => (insert_menu_sections(props, menu.index), t("添加上下文", "Add context")),
            MenuView::Selection => (selection_menu_sections(props, shared, context_order), t("所选消息", "Selected messages")),
        };
        root = root.child(context_menu(("tl-menu", scroller), ContextMenuProps::new(ContextMenuAnchor::Point { x: menu.x, y: menu.y }, label, on_close, sections)));
    }
    root
}
