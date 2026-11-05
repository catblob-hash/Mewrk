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
use mewrk_client::runs::{ConversationTurnStatus, LiveContext, RunView, TimelineHistoryAction};
use mewrk_client::task_tools::is_preview_tool_name;
use mewrk_ui_common::context_menu::{context_menu, ContextMenuAnchor, ContextMenuItem, ContextMenuProps, ContextMenuSection};
use mewrk_ui_common::markdown::{clear_text_selection, estimate_markdown_height, has_text_selection};

use crate::cards::{context_card, insertion_line_view, stock_empty_state, timeline_insert_editor, turn_notices, waiting_indicator};
use crate::drag::{start_session, DockAt, DragGeometry, DragOutcome, DragSession, Look, MarkState, Mover, PressDrag, Slide, Unit, BLOCK_PAD_BOTTOM, BLOCK_PAD_TOP, SLOT_GAP};
use crate::frame::{drawn_slide, extras_key, hash_id, row_key, timeline_state, unit_key, DockRoom, FollowGlide, Frame, MarqueeDrag, MenuState, MenuView, PreviousTimeline, Resolved, Settling, Shared, TimelineState};
use crate::inline_text_editor::InsertKind;
use crate::item::Item;
use crate::kit::clipboard_leaf;
use crate::nodes::{build_context_render_nodes, context_visual_length, display_insertion_index, project_turns, rendered_context_indexes, RenderNode};
use crate::orchestration::is_host_authored_user_context;
use crate::props::{ConversationViewProps, OpenInsert, TimelineEditorState};
use crate::question_card::{question_timeline_card, QuestionCardProps};
use crate::rows::{find_item, timeline_block, timeline_rows};
use crate::turn_changes::{message_change_spans, summarize_span_changes, turn_changes};

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

/// `FOLLOW_GLIDE_RATE`: a line-high step is 90 % covered in about 200 ms.
const FOLLOW_GLIDE_RATE: f32 = 18.0;
const MAX_STEP_SECONDS: f32 = 0.05;

/// `stepFollowGlide`: one frame of a critically damped spring pulled toward the bottom. It never overshoots, so from rest it
/// only ever moves down: an upward step is how the scroller recognises the reader leaving the bottom.
pub fn step_follow_glide(glide: FollowGlide, target: f32, now: Instant) -> (FollowGlide, bool) {
    let seconds = glide.time.map_or(1.0 / 60.0, |time| now.saturating_duration_since(time).as_secs_f32().clamp(0.0, MAX_STEP_SECONDS));
    let offset = glide.position - target;
    let rate = FOLLOW_GLIDE_RATE;
    let decay = (-rate * seconds).exp();
    let drive = glide.velocity + rate * offset;
    let next_offset = (offset + drive * seconds) * decay;
    let velocity = (glide.velocity - rate * drive * seconds) * decay;
    if next_offset.abs() < 0.5 && velocity.abs() < 20.0 {
        return (FollowGlide { position: target, velocity: 0.0, time: Some(now) }, true);
    }
    (FollowGlide { position: target.min(glide.position.max(target + next_offset)), velocity, time: Some(now) }, false)
}

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

/// Rebuilds what is derived from the contexts, when they changed.
fn rebuild_model(state: &mut TimelineState, contexts: &Resolved<'_>, props: &ConversationViewProps, width: f32, message_size: f32) {
    let count = contexts.count();
    let mut context_index = HashMap::with_capacity(count);
    for index in 0..count {
        context_index.insert(hash_id(contexts.get(index).id()), index as u32);
    }
    let turn_anchors: HashSet<usize> = props.turns.iter().filter_map(|turn| context_index.get(&hash_id(&turn.anchor_context_id)).map(|index| *index as usize)).collect();
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
    let user_nodes: HashSet<&str> = nodes
        .iter()
        .filter_map(|node| match node {
            RenderNode::Context { index } => {
                let item = contexts.get(*index);
                (matches!(item.item, ContextItem::User { .. }) && !is_host_authored_user_context(item.item)).then(|| item.id())
            }
            _ => None,
        })
        .collect();
    let owned;
    let spans = match &props.change_spans {
        Some(spans) => spans.as_slice(),
        None => {
            let open = props.streaming || props.turns.iter().any(|turn| turn.status == ConversationTurnStatus::Running);
            owned = message_change_spans(&items, |item| user_nodes.contains(item.id()), open);
            owned.as_slice()
        }
    };
    for span in spans {
        let Some(summary) = summarize_span_changes(span, |id| index_of(id).map(|index| contexts.get(index))) else { continue };
        // The drawn node holding the newest of `ids`, which is where a span ends on screen.
        let Some(last) = span.context_ids.iter().filter_map(|id| index_of(id)).filter_map(node_of).max() else { continue };
        state.changes_after.entry(last).or_default().push(Rc::new(summary));
    }
    state.rendered = Rc::new(rendered_context_indexes(&nodes));
    state.nodes = Rc::new(nodes);
    state.node_keys = node_keys;
    state.context_index = context_index;
    state.node_of_context = node_of_context;
    state.question_answers = question_answers;
    state.selection.retain(|id| state.context_index.contains_key(&hash_id(id)));
    state.layout_dirty = true;
    relayout(state, contexts, width, message_size);
}

/// `nodeLayout`'s positions: the stored or estimated height of every row, and where each starts. While a drag has pieces out of the
/// timeline it closes up over them, keeping what follows them on screen, and pieces docked at the home line take room where they stand.
fn relayout(state: &mut TimelineState, contexts: &Resolved<'_>, width: f32, message_size: f32) {
    let count = state.nodes.len();
    state.row_h.clear();
    state.row_h.reserve(count);
    state.tops.clear();
    state.tops.reserve(count + 1);
    state.adjust.clear();
    state.extras_only.clear();
    // Out of the state while it is read, as everything below writes to the state.
    let drag = state.drag.take();
    let session = drag.as_ref().and_then(|drag| drag.session.as_ref());
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
    let mut y = 0.0;
    for position in 0..count {
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
                gap + extras + estimate_node(contexts, &state.nodes[position], width, message_size)
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
    drop(state);
    let mut row = col().key(row_key(f.salt, node_key)).w_full().min_w(0.0).shrink0();
    if !extras_only {
        row = row.pt(gap).child(node_view(f, &nodes[position]));
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

/// `renderTimelineNode`.
fn node_view(f: &Frame<'_>, node: &RenderNode) -> E {
    let mut slot = col().w_full().min_w(0.0);
    match node {
        RenderNode::Block { entries, .. } => {
            // A block draws several contexts as rows, so an insert index anywhere inside it is answered by the one editor drawn ahead of the block.
            let first = entries.iter().map(|entry| entry.index).min().unwrap_or(0);
            let after = entries.iter().map(|entry| entry.index).max().unwrap_or(0) + 1;
            if f.insert_editor_index.is_some_and(|index| index >= first && index < after) {
                slot = slot.child(timeline_insert_editor(f));
            }
            // Only the block the line falls in hears where it is.
            let insertion = f.display_insertion_index.filter(|index| *index >= first && *index < after);
            slot.child(timeline_block(f, entries, insertion))
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
            timeline_rows(f, &rows, None, !piece.row)
        }
        node => node_view(f, node),
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
            timeline_block(f, &rows, None)
        } else {
            piece_view(f, session, pieces[0])
        });
    }
    dock
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
        state.follow.glide = None;
        // A body is known by its address, and one freed on the way out can hand that on to the next one drawn here.
        state.fingerprint = 0;
    }

    let (first, last, count_contexts, anchor_position);
    {
        let persisted: &[ContextItem] = match (&draft, conversation.as_deref()) {
            (Some(draft), _) => draft,
            (None, Some(id)) => app.conversation(id).map_or(&[][..], |conversation| conversation.contexts.as_slice()),
            _ => &[],
        };
        let live: &[LiveContext] = if conversation.is_some() { run.as_ref().map_or(&[][..], |run| run.contexts.as_slice()) } else { props.contexts.as_slice() };
        let updated = conversation.as_deref().and_then(|id| app.conversation(id)).map(|conversation| conversation.updated_at.as_str());
        let mut workflow_keys: Vec<&str> = props.workflow_run_by_call.keys().map(String::as_str).collect();
        workflow_keys.sort_unstable();
        let fingerprint = hash_key((persisted.as_ptr() as usize, persisted.len(), updated, run.as_ref().map(|run| run.revision), (!props.contexts.is_empty()).then(|| Rc::as_ptr(&props.contexts) as usize), (!props.turns.is_empty()).then(|| Rc::as_ptr(&props.turns) as usize), props.turns.len(), &workflow_keys, props.change_spans.as_ref().map(|spans| Rc::as_ptr(spans) as usize), props.streaming, mewrk_client::i18n::is_english()));
        let mut guard = shared.borrow_mut();
        let state = &mut *guard;
        state.turns = props.turns.clone();
        if state.fingerprint != fingerprint || state.map.is_empty() != (persisted.is_empty() && live.is_empty()) {
            state.fingerprint = fingerprint;
            state.map = Rc::new(Resolved::map_of(persisted, live));
            let contexts = Resolved { persisted, live, map: state.map.clone() };
            let before_order = state.node_keys.clone();
            let anchor_key = state.anchor_key;
            let anchor_top = anchor_key_top(state, anchor_key);
            rebuild_model(state, &contexts, &props, width, message_size);
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
                follow.glide = None;
                follow.jump = false;
            }
            follow.scroll_timeline = Some(timeline_id.clone());
        }
        // The reader is heading up: let go of the bottom before the next frame pulls them back.
        // Only one that moved the timeline itself: a code block or an output pane with room of its own takes the wheel first.
        if std::mem::take(&mut follow.wheel_up) && offset < follow.expected_top - 0.01 {
            follow.pinned = false;
            follow.glide = None;
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
            }
            match &state.nodes[position] {
                RenderNode::Block { entries, .. } => {
                    for entry in entries {
                        let id = contexts.get(entry.index).id();
                        if let Some(r) = rect_of(unit_key(0, id)) {
                            state.unit_heights.insert(hash_id(id), r.h);
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
            relayout(state, &contexts, width, message_size);
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
            follow.glide = None;
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
                if !initial && (props.streaming || follow.glide.is_some()) {
                    follow.glide.get_or_insert(FollowGlide { position: offset, velocity: 0.0, time: None });
                } else {
                    follow.jump = true;
                }
            } else if changed_contexts {
                // A glide may finish the distance it was already covering, but never past a tail that went away.
                if previous.as_ref().is_some_and(|previous| next.length < previous.length) {
                    follow.glide = None;
                }
            }
            // Commits are not the only thing that grows a live page: a body opening under its row, the waiting line changing shape.
            let grew = state.total + PAD_TOP + PAD_BOTTOM > follow.content + 0.5;
            if props.streaming && grew && same_timeline {
                follow.glide.get_or_insert(FollowGlide { position: offset, velocity: 0.0, time: None });
            }
            let start = offset + shift;
            // Pinned to the bottom is at the bottom: what rounding leaves over is not the reader having moved.
            if follow.glide.is_none() && (bottom - start).abs() > 0.5 && (bottom - start).abs() <= SCROLL_BOTTOM_EPSILON {
                follow.jump = true;
            }
            if std::mem::take(&mut follow.jump) || (follow.glide.is_some() && fluent::reduced_motion()) {
                follow.glide = None;
                shift += bottom - start;
            } else if let Some(glide) = follow.glide {
                // Something else moved the page, so go on from there.
                let position = if (start - glide.position).abs() > 1.0 { start } else { glide.position };
                let (glided, done) = step_follow_glide(FollowGlide { position, ..glide }, bottom, now);
                shift += glided.position - start;
                follow.glide = if done { None } else { Some(glided) };
                repaint();
            }
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
            let live: &[LiveContext] = if conversation.is_some() { run.as_ref().map_or(&[][..], |run| run.contexts.as_slice()) } else { props.contexts.as_slice() };
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
    let live: &[LiveContext] = if conversation.is_some() { run.as_ref().map_or(&[][..], |run| run.contexts.as_slice()) } else { props.contexts.as_slice() };
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
        let copies = |salt: u64| Frame { contexts: contexts.clone(), props: &props, shared: &shared, read_only: true, mutation_read_only: true, render_user_markdown, collapse_reasoning, single_dollar_math, message_ty, explanations: &explanations, selected: &selection, editing_context_id: None, editing_question_id: None, insert_editor_index: None, display_insertion_index: None, open_insert: open_insert.clone(), column_w: width, salt, lifted: &nothing, dimmed: &nothing, dock_inside: None };
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
            persisted = match (&draft, conversation.as_deref()) {
                (Some(draft), _) => draft.as_slice(),
                (None, Some(id)) => app.conversation(id).map_or(&[][..], |conversation| conversation.contexts.as_slice()),
                _ => &[],
            };
            let live: &[LiveContext] = if conversation.is_some() { run.as_ref().map_or(&[][..], |run| run.contexts.as_slice()) } else { props.contexts.as_slice() };
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
                    state.follow.glide = None;
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
