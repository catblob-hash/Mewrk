//! What every card and row reads while one frame of a timeline is built, and the timeline's retained state.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mewrk_client::model::ContextItem;
use mewrk_client::prelude::*;
use mewrk_client::runs::{ConversationTurn, LiveContext};

use crate::drag::{DockAt, Mover, PressDrag, Slide};
use crate::inline_text_editor::InsertKind;
use crate::item::Item;
use crate::nodes::{Contexts, RenderNode, TurnProjection};
use crate::props::{ConversationViewProps, OpenInsert};
use crate::tool_view::ToolPresentation;
use crate::turn_changes::TurnChangeSummary;

const LIVE_BIT: u32 = 1 << 31;

/// The rendered contexts of one frame: indexes through `map`, so reading them never copies the persisted timeline.
#[derive(Clone)]
pub struct Resolved<'a> {
    pub persisted: &'a [ContextItem],
    pub live: &'a [LiveContext],
    pub map: Rc<Vec<u32>>,
}

impl<'a> Resolved<'a> {
    pub fn count(&self) -> usize {
        self.map.len()
    }

    pub fn get(&self, index: usize) -> Item<'a> {
        let at = self.map[index];
        if at & LIVE_BIT != 0 { Item::live(&self.live[(at & !LIVE_BIT) as usize]) } else { Item::settled(&self.persisted[at as usize]) }
    }

    /// A map for the persisted contexts, those a live projection supersedes left out, then the live ones.
    pub fn map_of(persisted: &[ContextItem], live: &[LiveContext]) -> Vec<u32> {
        // Streaming contexts win for duplicate ids because they carry the latest delta and streaming state.
        let streaming: HashSet<&str> = live.iter().map(LiveContext::id).collect();
        let mut map = Vec::with_capacity(persisted.len() + live.len());
        for (at, item) in persisted.iter().enumerate() {
            if streaming.is_empty() || !streaming.contains(item.id()) {
                map.push(at as u32);
            }
        }
        map.extend((0..live.len() as u32).map(|at| at | LIVE_BIT));
        map
    }

    /// A map for the persisted contexts with each live one drawn in the place of the persisted context it stands for, and
    /// none added: what a background task's overlay is, as against a run's projection.
    pub fn map_in_place(persisted: &[ContextItem], live: &[LiveContext]) -> Vec<u32> {
        let over: HashMap<&str, u32> = live.iter().enumerate().map(|(at, context)| (context.id(), at as u32)).collect();
        persisted.iter().enumerate().map(|(at, item)| over.get(item.id()).map_or(at as u32, |live| live | LIVE_BIT)).collect()
    }
}

impl<'a> Contexts<'a> for &Resolved<'a> {
    fn count(&self) -> usize {
        Resolved::count(self)
    }
    fn at(&self, index: usize) -> Item<'a> {
        self.get(index)
    }
}

/// `ContextMenuState.view`: which panel the one menu is showing.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuView {
    Insert,
    Selection,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MenuState {
    pub x: f32,
    pub y: f32,
    pub index: usize,
    pub view: MenuView,
    /// The window when it opened: a resize moves what it was anchored to, so it closes.
    pub window: (f32, f32),
    /// Where the timeline was when it opened: moving what is underneath closes it too.
    pub offset: f32,
}

/// A selection drag in progress: where it began in the timeline's own coordinates, and where the pointer is.
#[derive(Clone, Debug)]
pub struct MarqueeDrag {
    pub start_x: f32,
    /// Measured from the top of the scrolled content, so scrolling carries the box's far edge, not its start.
    pub start_content_y: f32,
    pub x: f32,
    pub y: f32,
    /// What was picked out when the press began, which the box adds to.
    pub base: HashSet<String>,
    /// Whether the press has travelled: a box once it has, a click on one context until then.
    pub moved: bool,
}

/// `lib/followGlide.ts`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FollowGlide {
    pub position: f32,
    /// Pixels per second, positive downward.
    pub velocity: f32,
    pub time: Option<std::time::Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviousTimeline {
    pub timeline_id: Option<String>,
    pub length: usize,
    pub last_id: Option<String>,
    pub last_visual_length: usize,
}

/// `scrollPinnedRef` and its company.
#[derive(Clone, Debug)]
pub struct Follow {
    pub pinned: bool,
    /// Where the page was left at the end of last frame, requests included.
    pub expected_top: f32,
    pub scroll_timeline: Option<String>,
    pub previous: Option<PreviousTimeline>,
    pub glide: Option<FollowGlide>,
    pub jump: bool,
    /// The content's height when last looked at, standing in for a `ResizeObserver` while streaming.
    pub content: f32,
    /// A wheel turned up over the timeline during last frame's paint.
    pub wheel_up: bool,
}

impl Default for Follow {
    fn default() -> Self {
        Self { pinned: true, expected_top: 0.0, scroll_timeline: None, previous: None, glide: None, jump: false, content: 0.0, wheel_up: false }
    }
}

/// What a tool row derives from its card, kept because some of it parses the card's whole output.
pub struct ToolFacts {
    pub presentation: ToolPresentation,
    pub tokens: u64,
    pub status: crate::rows::RowStatus,
    pub agent_route: Option<String>,
}

/// One timeline's retained state: it lives as long as the app, because a card scrolled out of view keeps its height and
/// its disclosure, and a conversation switched away from and back keeps where it was.
#[derive(Default)]
pub struct TimelineState {
    pub fingerprint: u64,
    pub map: Rc<Vec<u32>>,
    pub nodes: Rc<Vec<RenderNode>>,
    /// `renderNodeKey`, hashed, by node position.
    pub node_keys: Vec<u64>,
    /// A context id's hash to its index.
    pub context_index: HashMap<u64, u32>,
    /// A context index to the node drawing it, `u32::MAX` for an undrawn one.
    pub node_of_context: Vec<u32>,
    pub rendered: Rc<Vec<usize>>,
    pub turns: Rc<Vec<ConversationTurn>>,
    pub projection: TurnProjection,
    pub changes_after: HashMap<usize, Vec<Rc<TurnChangeSummary>>>,
    /// An answered question is one card for two contexts, so picking the card picks both.
    pub question_answers: HashMap<String, String>,
    /// Measured heights, by node key. An absent key is still an estimate.
    pub heights: HashMap<u64, f32>,
    /// Measured heights of cards and rows, by context id hash, for drags and boxes over what is not built.
    pub unit_heights: HashMap<u64, f32>,
    pub row_h: Vec<f32>,
    pub tops: Vec<f32>,
    pub total: f32,
    pub layout_dirty: bool,
    pub anchor_key: u64,
    pub built: Vec<usize>,
    pub follow: Follow,
    pub selection: HashSet<String>,
    pub marquee: Option<MarqueeDrag>,
    pub marquee_box: Option<Rect>,
    pub drag: Option<PressDrag>,
    pub drag_target: Option<usize>,
    pub menu: Option<MenuState>,
    pub expanded: HashMap<u64, bool>,
    pub facts: HashMap<u64, (u64, Rc<ToolFacts>)>,
    /// A fold toggle to bring back into view, once this many more frames have laid it out.
    pub pending_reveal: Option<(u64, u8)>,
    /// How far the real page's end is from where the stored heights put it.
    pub rounding: f32,
    /// Where the pieces docked at the home line take room.
    pub dock: Option<DockRoom>,
    /// The dock as last drawn: what is laid out beside it is told how tall it really came out.
    pub dock_measured: Option<f32>,
    /// A row with nothing left above it in a drag, which stands at the head of the column without the gap it keeps from the one before it.
    pub flush: Option<usize>,
    /// How much less room a row takes than it does at rest, while a drag has things out of it or set down in it.
    pub adjust: HashMap<usize, f32>,
    /// Rows a drag lifted whole that still draw what follows them: a list of changed files, a turn's notice.
    pub extras_only: HashSet<usize>,
    /// Rows a change of layout is sliding into place, by node key.
    pub slides: HashMap<u64, Slide>,
    /// Cards and rows on their way to their places, by context id hash.
    pub unit_slides: HashMap<u64, Slide>,
    /// How the page is put back in order once the timeline has redrawn for however a drag ended.
    pub settling: Option<Settling>,
}

/// Room taken by the pieces docked at the home line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockRoom {
    pub at: DockAt,
    /// Margins included.
    pub height: f32,
    /// The margin above the first piece.
    pub lead: f32,
}

/// `settle`: what a drag leaves to be done once the timeline has redrawn.
#[derive(Clone, Debug)]
pub struct Settling {
    /// What stayed, and where it was.
    pub tops: Vec<(Mover, f32)>,
    /// What was dragged, by context id hash: where its copy was, in the stack or at the home line, and its own left.
    pub held: Vec<(u64, (f32, f32), f32)>,
    /// Whatever sits just above the drop stays where it is while the rest falls into place.
    pub steady: Option<(String, f32)>,
    pub still: bool,
    /// Frames this still has to answer for: a block the move made is an estimate until it has been laid out once.
    pub frames: u8,
    pub started: std::time::Instant,
}

pub type Shared = Rc<RefCell<TimelineState>>;

/// The retained states, by timeline.
#[derive(Default)]
pub struct Timelines {
    states: HashMap<String, Shared>,
    order: Vec<String>,
}

/// How many timelines keep their heights and disclosures.
const RETAINED_TIMELINES: usize = 32;

pub fn timeline_state(app: &mut App, id: &str) -> Shared {
    let timelines = app.local::<Timelines>();
    if let Some(state) = timelines.states.get(id) {
        return state.clone();
    }
    timelines.order.push(id.to_owned());
    if timelines.order.len() > RETAINED_TIMELINES {
        let gone = timelines.order.remove(0);
        timelines.states.remove(&gone);
    }
    let state: Shared = Rc::default();
    timelines.states.insert(id.to_owned(), state.clone());
    state
}

/// Everything a card or row reads while a frame is built.
pub struct Frame<'a> {
    pub contexts: Resolved<'a>,
    pub props: &'a Rc<ConversationViewProps>,
    pub shared: &'a Shared,
    pub read_only: bool,
    pub mutation_read_only: bool,
    pub render_user_markdown: bool,
    pub collapse_reasoning: bool,
    pub single_dollar_math: bool,
    pub message_ty: TypeStyle,
    pub explanations: &'a HashMap<String, (Option<String>, Option<String>)>,
    pub selected: &'a HashSet<String>,
    pub editing_context_id: Option<&'a str>,
    pub editing_question_id: Option<&'a str>,
    pub insert_editor_index: Option<usize>,
    pub display_insertion_index: Option<usize>,
    pub open_insert: OpenInsert,
    /// The message column's width. A percentage of it as `max-width` would not do: layout first measures a child with percentages it
    /// cannot resolve yet, so a message would take the height it has at the full width, and paint taller.
    pub column_w: f32,
    /// Copies drawn under the pointer or at the home line have keys of their own.
    pub salt: u64,
    /// What a drag has out of the timeline, by context id.
    pub lifted: &'a HashSet<String>,
    /// What a drag dims, set down where it sat.
    pub dimmed: &'a HashSet<String>,
    /// The room docked pieces take among a block's rows: ahead of this context's row, and how much.
    pub dock_inside: Option<(usize, f32)>,
}

impl Frame<'_> {
    pub fn unit_key(&self, id: &str) -> u64 {
        unit_key(self.salt, id)
    }

    pub fn expanded(&self, key: u64, default: bool) -> bool {
        self.shared.borrow().expanded.get(&key).copied().unwrap_or(default)
    }

    /// `data-timeline-draggable`: what is picked out shows it can be picked up.
    pub fn picked_cursor(&self, id: &str, view: E) -> E {
        let draggable = !self.mutation_read_only && self.props.on_delete_contexts.is_some() && self.props.on_move_contexts.is_some();
        if draggable && self.selected.contains(id) { view.cursor(fluent::CursorKind::Grab) } else { view }
    }

    /// A card or row drawn where a drag's slide has it: on its way, above what it passes.
    pub fn slid(&self, id: &str, view: E) -> E {
        if self.salt != 0 {
            return view;
        }
        let key = hash_id(id);
        let Some(slide) = self.shared.borrow().unit_slides.get(&key).copied() else { return view };
        let now = std::time::Instant::now();
        if slide.done(now) {
            self.shared.borrow_mut().unit_slides.remove(&key);
            return view;
        }
        repaint();
        drawn_slide(view, &slide, now)
    }
}

/// A slide as drawn: only that. The layout already has what it moves where it is going.
pub fn drawn_slide(view: E, slide: &Slide, now: std::time::Instant) -> E {
    let (dx, dy) = slide.offset(now);
    let view = view.left(dx).top(dy).raised();
    match slide.fade_from {
        Some(from) => view.opacity(from + (1.0 - from) * slide.progress(now)),
        None => view,
    }
}

pub fn unit_key(salt: u64, id: &str) -> u64 {
    hash_key(("tl-unit", salt, id))
}

pub fn row_key(salt: u64, node_key: u64) -> u64 {
    hash_key(("tl-row", salt, node_key))
}

pub fn hash_id(id: &str) -> u64 {
    hash_key(id)
}

/// What the slide of a row's `TurnChanges` and notices is filed under, apart from a slide of the whole row.
pub fn extras_key(node_key: u64) -> u64 {
    hash_key(("tl-extras", node_key))
}

pub fn insert_kind_of(item: &ContextItem) -> InsertKind {
    match item {
        ContextItem::System { .. } => InsertKind::System,
        ContextItem::User { .. } => InsertKind::User,
        ContextItem::Assistant { .. } => InsertKind::Assistant,
        ContextItem::Reasoning { .. } => InsertKind::Reasoning,
        ContextItem::Tool { .. } => InsertKind::Tool,
    }
}

/// What a timeline is doing right now, for tests and a debugger: everything here is otherwise only drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineSnapshot {
    pub selection: Vec<String>,
    pub menu: Option<MenuView>,
    pub menu_index: Option<usize>,
    pub marquee: bool,
    pub drag_active: bool,
    pub look: Option<crate::drag::Look>,
    pub target: Option<usize>,
    pub line: Option<usize>,
    pub mark: Option<crate::drag::MarkState>,
    pub mark_y: Option<f32>,
    pub ghosts: usize,
    /// Where docked pieces take room, while they do.
    pub dock: Option<DockRoom>,
    /// Rows sliding into place, and cards and rows on their way to their places.
    pub sliding: usize,
    pub landing: usize,
    pub pinned: bool,
    pub gliding: bool,
    pub built: Vec<usize>,
    pub nodes: usize,
    pub total: f32,
}

/// A card's or row's slide, for tests.
pub fn unit_slide(app: &mut App, timeline: &str, id: &str) -> Option<Slide> {
    timeline_state(app, timeline).borrow().unit_slides.get(&hash_id(id)).copied()
}

/// A row's slide by its context's node key, for tests.
pub fn node_slide(app: &mut App, timeline: &str, node_key: &str) -> Option<Slide> {
    timeline_state(app, timeline).borrow().slides.get(&hash_id(node_key)).copied()
}

/// The title a tool row's facts hold now, for tests.
pub fn row_title(app: &mut App, timeline: &str, id: &str) -> Option<String> {
    timeline_state(app, timeline).borrow().facts.get(&hash_id(id)).map(|(_, facts)| facts.presentation.title.clone())
}

/// The state a tool row's facts hold now, for tests.
pub fn row_status_label(app: &mut App, timeline: &str, id: &str) -> Option<String> {
    timeline_state(app, timeline).borrow().facts.get(&hash_id(id)).map(|(_, facts)| facts.status.label.clone())
}

/// The slide of what follows a row's card, by that row's node key, for tests.
pub fn extras_slide(app: &mut App, timeline: &str, node_key: &str) -> Option<Slide> {
    timeline_state(app, timeline).borrow().slides.get(&extras_key(hash_id(node_key))).copied()
}

pub fn timeline_snapshot(app: &mut App, id: &str) -> TimelineSnapshot {
    let shared = timeline_state(app, id);
    let state = shared.borrow();
    let session = state.drag.as_ref().and_then(|drag| drag.session.as_ref());
    let mut selection: Vec<String> = state.selection.iter().cloned().collect();
    selection.sort_by_key(|id| state.context_index.get(&hash_id(id)).copied().unwrap_or(u32::MAX));
    TimelineSnapshot {
        selection,
        menu: state.menu.as_ref().map(|menu| menu.view.clone()),
        menu_index: state.menu.as_ref().map(|menu| menu.index),
        marquee: state.marquee_box.is_some(),
        drag_active: session.is_some(),
        look: session.map(|session| session.look),
        target: session.and_then(|session| session.target),
        line: state.drag_target,
        mark: session.map(|session| session.mark_state),
        mark_y: session.map(|session| session.mark_y),
        ghosts: session.map_or(0, |session| session.ghost.len()),
        dock: state.dock,
        sliding: state.slides.len(),
        landing: state.unit_slides.len(),
        pinned: state.follow.pinned,
        gliding: state.follow.glide.is_some(),
        built: state.built.clone(),
        nodes: state.nodes.len(),
        total: state.total,
    }
}
