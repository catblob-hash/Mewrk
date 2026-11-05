//! Side pane layouts (lib/sidePanes.ts): which panes each conversation has open beside the chat, where they sit, and
//! its preview pages.
//!
//! Upstream's reducer told "unchanged" by handing back the same object. Here [`side_panes_reducer`] returns `None`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::ui_store::UiStore;
use crate::App;

/// A pane: `terminal`, `review`, `files`, `tasks`, `plan`, `settings`, `subagent`, `preview:<session>` or `history:<id>`.
pub type SidePaneId = String;

/// What sort of pane an id is: the part before its colon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SidePaneKind {
    Terminal,
    Review,
    Preview,
    Files,
    Tasks,
    Plan,
    History,
    Settings,
    Subagent,
    /// An id none of the above; upstream's types rule it out, so only a bug makes one. It ranks below every kind when
    /// room is needed and is never stored, which is what upstream's casts came to.
    Unknown,
}

impl SidePaneKind {
    /// The name upstream's ids and stored widths use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Review => "review",
            Self::Preview => "preview",
            Self::Files => "files",
            Self::Tasks => "tasks",
            Self::Plan => "plan",
            Self::History => "history",
            Self::Settings => "settings",
            Self::Subagent => "subagent",
            Self::Unknown => "unknown",
        }
    }
}

/// The `tasks` pane's two pages, as its tabs: the conversation's tasks and its history. Neither closes, so the pane is
/// open exactly while one of them is shown; the tasks tab is the left one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TasksPaneTab {
    Tasks,
    History,
}

/// One conversation's side area.
#[derive(Clone, Debug, PartialEq)]
pub struct SidePaneLayout {
    /// Every open pane, in the order it was opened.
    pub panes: Vec<SidePaneId>,
    /// Where those panes sit: the columns left to right, each listing its panes top to bottom. Every open pane is in
    /// exactly one column and no column is empty.
    pub columns: Vec<Vec<SidePaneId>>,
    /// The whole side area's width weight against the chat's [`CHAT_TILE_FLEX`].
    pub side_flex: f64,
    /// The columns' widths relative to one another, index for index.
    pub column_flex: Vec<f64>,
    /// Each pane's height relative to the others in its column.
    pub pane_flex: BTreeMap<String, f64>,
    pub focused: Option<SidePaneId>,
    /// The agents the `subagent` pane has a tab for, in tab order. Empty exactly while that pane is closed: closing the
    /// pane drops its tabs, and the last tab leaving closes the pane.
    pub subagent_tabs: Vec<String>,
    /// The tab the `subagent` pane shows: one of `subagent_tabs`, or `None` while there are none.
    pub active_subagent: Option<String>,
    /// The tab the `tasks` pane shows. Kept while the pane is closed; every way in names its tab.
    pub tasks_tab: TasksPaneTab,
    /// The pane shown alone over the whole workspace, chat column included. Never persisted, and dropped by anything
    /// that would leave the user staring at a pane they did not ask for.
    pub expanded: Option<SidePaneId>,
}

impl Default for SidePaneLayout {
    fn default() -> Self {
        EMPTY_LAYOUT.clone()
    }
}

/// Every conversation's side area. Held by the app as `app.local::<SidePanesState>()`; see [`App::side_panes`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SidePanesState {
    pub layout_by_conversation: BTreeMap<String, SidePaneLayout>,
    /// Each conversation's preview pages, in tab order. A page is a native browser session: the conversation's own page
    /// is keyed by the conversation id, every further page by `<conversation>#<token>`.
    pub preview_sessions: BTreeMap<String, Vec<String>>,
    /// Which of its conversation's workspaces (1-based, the model's numbering) each page belongs to, by session id: the
    /// workspace whose `.mewrk/launch.json` its start page lists and whose servers it runs. Absent is workspace 1.
    pub preview_workspaces: BTreeMap<String, u32>,
    pub last_side_flex_by_kind: BTreeMap<SidePaneKind, f64>,
}

/// What can happen to the side panes.
#[derive(Clone, Debug, PartialEq)]
pub enum SidePanesAction {
    Open { conversation_id: String, pane: SidePaneId },
    Toggle { conversation_id: String, pane: SidePaneId },
    Close { conversation_id: String, pane: SidePaneId },
    CloseLast { conversation_id: String },
    /// Puts `replacement` where `pane` is (same column, height and focus) for a pane whose subject turned out to go by
    /// another id. Closing and reopening would move it to wherever a new pane goes. When `replacement` is already open,
    /// `pane` only closes.
    Replace { conversation_id: String, pane: SidePaneId, replacement: SidePaneId },
    Focus { conversation_id: String, pane: SidePaneId },
    ToggleExpand { conversation_id: String, pane: SidePaneId },
    SetSideFlex { conversation_id: String, side_flex: f64 },
    SetColumnFlex { conversation_id: String, column_flex: Vec<f64> },
    SetPaneFlex { conversation_id: String, pane_flex: BTreeMap<String, f64> },
    /// Shows an agent's transcript: gives it a tab in the `subagent` pane, after the others unless it has one already,
    /// selects that tab, and opens the pane where a new pane goes if it isn't open yet.
    OpenSubagent { conversation_id: String, subagent_id: String },
    /// Takes an agent's tab away. Closing the shown tab shows the one to its right, or to its left off the end; closing
    /// the last one closes the pane.
    CloseSubagent { conversation_id: String, subagent_id: String },
    /// Points an agent's tab at the id the agent turned out to go by, a call id settling into the stable name id, in the
    /// same place in the strip. When the new id has a tab already, the old one only closes.
    RetargetSubagent { conversation_id: String, from: String, to: String },
    /// Puts the tabs in the order they were dragged into; ids the strip doesn't hold are ignored.
    ReorderSubagents { conversation_id: String, subagent_ids: Vec<String> },
    /// Selects one of the `tasks` pane's tabs, opening the pane where a new pane goes if it's closed.
    ShowTasksTab { conversation_id: String, tab: TasksPaneTab },
    /// Adds a page to the conversation's roster. `workspace` records which workspace it belongs to; without one a page
    /// keeps what it had, and a new page belongs to workspace 1.
    RegisterPreview { conversation_id: String, session_id: String, workspace: Option<u32> },
    ForgetPreview { conversation_id: String, session_id: String },
    /// Moves a page to another workspace: the one whose server it turned out to be showing.
    SetPreviewWorkspace { conversation_id: String, session_id: String, workspace: u32 },
    /// Puts the conversation's pages in the order their tabs were dragged into. An id the roster doesn't hold is ignored,
    /// and a page the request leaves out keeps its relative place after the rest.
    ReorderPreviews { conversation_id: String, session_ids: Vec<String> },
    /// Moves the layout a draft accumulated under its placeholder id onto the real conversation it just became, the way
    /// the composer's text and attachments move. Without it a pane opened in a draft is dropped on send and left behind
    /// to reappear, unasked, in the next draft. The draft's preview roster comes along too: its page is already named
    /// after the conversation it became.
    AdoptConversation { conversation_id: String, from: String },
    RemoveConversation { conversation_id: String },
}

impl SidePanesAction {
    /// The conversation the action is about.
    pub fn conversation_id(&self) -> &str {
        match self {
            Self::Open { conversation_id, .. }
            | Self::Toggle { conversation_id, .. }
            | Self::Close { conversation_id, .. }
            | Self::CloseLast { conversation_id }
            | Self::Replace { conversation_id, .. }
            | Self::Focus { conversation_id, .. }
            | Self::ToggleExpand { conversation_id, .. }
            | Self::SetSideFlex { conversation_id, .. }
            | Self::SetColumnFlex { conversation_id, .. }
            | Self::SetPaneFlex { conversation_id, .. }
            | Self::OpenSubagent { conversation_id, .. }
            | Self::CloseSubagent { conversation_id, .. }
            | Self::RetargetSubagent { conversation_id, .. }
            | Self::ReorderSubagents { conversation_id, .. }
            | Self::ShowTasksTab { conversation_id, .. }
            | Self::RegisterPreview { conversation_id, .. }
            | Self::ForgetPreview { conversation_id, .. }
            | Self::SetPreviewWorkspace { conversation_id, .. }
            | Self::ReorderPreviews { conversation_id, .. }
            | Self::AdoptConversation { conversation_id, .. }
            | Self::RemoveConversation { conversation_id } => conversation_id,
        }
    }
}

pub const CHAT_TILE_FLEX: f64 = 2.0;
pub const MIN_SIDE_FLEX: f64 = 0.25;
pub const MAX_SIDE_FLEX: f64 = 8.0;
/// How much wider the side area grows for each column opened after the first: half the chat's weight.
pub const NEW_COLUMN_FLEX: f64 = CHAT_TILE_FLEX / 2.0;
/// A conversation shows at most this many side panes; asking for another closes one first.
pub const MAX_SIDE_PANES: usize = 9;
/// The tallest a column grows: two columns hold six panes, three hold all nine.
pub const MAX_PANES_PER_COLUMN: usize = 3;
/// The order panes are kept in when the side area is full, most kept first: opening a pane past [`MAX_SIDE_PANES`]
/// closes the open pane furthest down this list, the oldest of its kind. Each of the first eight kinds opens at most one
/// pane (agents share the `subagent` pane as its tabs, and the conversation's history is a tab of the `tasks` pane), so
/// nine open panes always include an agent's history, and none of the first eight is ever closed to make room.
pub const PANE_KEEP_ORDER: [SidePaneKind; 9] = [
    SidePaneKind::Settings,
    SidePaneKind::Plan,
    SidePaneKind::Preview,
    SidePaneKind::Terminal,
    SidePaneKind::Review,
    SidePaneKind::Files,
    SidePaneKind::Tasks,
    SidePaneKind::Subagent,
    SidePaneKind::History,
];

static EMPTY_LAYOUT: SidePaneLayout = SidePaneLayout {
    panes: Vec::new(),
    columns: Vec::new(),
    side_flex: 1.0,
    column_flex: Vec::new(),
    pane_flex: BTreeMap::new(),
    focused: None,
    subagent_tabs: Vec::new(),
    active_subagent: None,
    tasks_tab: TasksPaneTab::Tasks,
    expanded: None,
};
const STORAGE_KEY: &str = "mewrk.sidePanes.sideFlexByKind";
const KINDS: [SidePaneKind; 9] = [
    SidePaneKind::Terminal,
    SidePaneKind::Review,
    SidePaneKind::Preview,
    SidePaneKind::Files,
    SidePaneKind::Tasks,
    SidePaneKind::Plan,
    SidePaneKind::History,
    SidePaneKind::Settings,
    SidePaneKind::Subagent,
];

/// The state nothing has happened to yet.
pub fn initial_side_panes_state() -> SidePanesState {
    SidePanesState::default()
}

/// A conversation's layout. No conversation, and one with nothing open, share the empty one.
pub fn side_pane_layout_for<'a>(state: &'a SidePanesState, conversation_id: Option<&str>) -> &'a SidePaneLayout {
    conversation_id.filter(|id| !id.is_empty()).and_then(|id| state.layout_by_conversation.get(id)).unwrap_or(&EMPTY_LAYOUT)
}

/// A conversation's preview pages, in tab order.
pub fn preview_sessions_for<'a>(state: &'a SidePanesState, conversation_id: Option<&str>) -> &'a [String] {
    conversation_id.filter(|id| !id.is_empty()).and_then(|id| state.preview_sessions.get(id)).map_or(&[], Vec::as_slice)
}

/// The workspace a page belongs to, 1-based. A page nobody placed belongs to workspace 1.
pub fn preview_workspace_of(state: &SidePanesState, session_id: &str) -> u32 {
    state.preview_workspaces.get(session_id).copied().filter(|workspace| *workspace >= 1).unwrap_or(1)
}

/// What sort of pane an id is.
pub fn pane_kind(id: &str) -> SidePaneKind {
    let name = id.split(':').next().unwrap_or(id);
    KINDS.into_iter().find(|kind| kind.as_str() == name).unwrap_or(SidePaneKind::Unknown)
}

/// What a pane is of: everything after the first colon, or `None` for a pane without one.
pub fn pane_target(id: &str) -> Option<&str> {
    id.split_once(':').map(|(_, target)| target)
}

/// The pane of a preview page.
pub fn preview_pane_id(session_id: &str) -> SidePaneId {
    format!("preview:{session_id}")
}

/// One agent's history. Always targeted: a conversation's history holds each agent it spawned apart from its own, and
/// the conversation's own is the `tasks` pane's history tab, so the two open side by side.
pub fn subagent_history_pane_id(id: &str) -> SidePaneId {
    format!("history:{id}")
}

/// Whether a pane is open.
pub fn pane_is_open(layout: &SidePaneLayout, id: &str) -> bool {
    layout.panes.iter().any(|pane| pane == id)
}

/// The `tasks` pane's tab on show, or `None` while the pane is closed.
pub fn shown_tasks_tab(layout: &SidePaneLayout) -> Option<TasksPaneTab> {
    pane_is_open(layout, "tasks").then_some(layout.tasks_tab)
}

/// The agent whose transcript the `subagent` pane is showing, or `None` while the pane is closed.
pub fn shown_subagent(layout: &SidePaneLayout) -> Option<&str> {
    if pane_is_open(layout, "subagent") { layout.active_subagent.as_deref() } else { None }
}

/// The session of the open preview pane.
pub fn open_preview_session(layout: &SidePaneLayout) -> Option<&str> {
    layout.panes.iter().find(|id| pane_kind(id) == SidePaneKind::Preview).and_then(|pane| pane_target(pane))
}

/// The focused pane, falling back from missing or stale focus to the last pane opened.
pub fn focused_pane(layout: &SidePaneLayout) -> Option<&str> {
    layout.focused.as_deref().filter(|focused| pane_is_open(layout, focused)).or(layout.panes.last().map(String::as_str))
}

/// The expanded pane, or `None` once it's no longer open. Closing a pane un-expands it for free.
pub fn expanded_pane(layout: &SidePaneLayout) -> Option<&str> {
    layout.expanded.as_deref().filter(|expanded| pane_is_open(layout, expanded))
}

/// The width a kind of pane opens the side area at, when no width is remembered for it.
pub fn default_side_flex_for_kind(kind: SidePaneKind) -> f64 {
    // Conversation settings is a two-column page of its own, so it opens as wide as the surfaces that carry a document
    // rather than as narrow as the stacked list panes.
    match kind {
        SidePaneKind::Review | SidePaneKind::Preview | SidePaneKind::Files | SidePaneKind::Settings => 3.0,
        _ => 1.0,
    }
}

/// An id safe anywhere one is needed, unique per pane: its code points in base 36.
pub fn side_pane_dom_id(id: &str) -> String {
    let encoded: Vec<String> = id.chars().map(|character| base_36(u32::from(character))).collect();
    format!("side-pane-{}", encoded.join("-"))
}

fn base_36(mut value: u32) -> String {
    let mut digits = Vec::new();
    loop {
        digits.push(char::from_digit(value % 36, 36).expect("under 36"));
        value /= 36;
        if value == 0 {
            return digits.iter().rev().collect();
        }
    }
}

/// The session of a further page of a conversation.
pub fn preview_tab_session_id(conversation_id: &str, token: &str) -> String {
    format!("{conversation_id}#{token}")
}

/// The session a new page opens under: the owner's own page when the roster doesn't hold it, as that's the page the
/// model's preview tools drive and so the first to fill, and otherwise a fresh tab. `owner_id` is the host identity the
/// pages are named after (for the draft, the id it will materialize as), not always the key its roster is filed under.
/// `token` is only consulted for a tab, and has to be one the host accepts after `#`: letters, digits, `-` and `_`.
pub fn new_preview_page_session_id(roster: &[String], owner_id: &str, token: &str) -> String {
    if !roster.iter().any(|session| session == owner_id) {
        return owner_id.to_owned();
    }
    preview_tab_session_id(owner_id, token)
}

/// The tab that takes over from `open` when `closing` go: the next one along as a browser's tabs do, and back from the
/// end. `None`: none is left, and the pane is on its start page.
pub fn preview_tab_after_closing(tabs: &[String], open: &str, closing: &[String]) -> Option<String> {
    let survives = |tab: &&String| !closing.contains(tab) && *tab != open;
    let Some(index) = tabs.iter().position(|tab| tab == open) else { return tabs.iter().find(survives).cloned() };
    tabs.iter().skip(index + 1).find(survives).or_else(|| tabs.iter().take(index).rev().find(survives)).cloned()
}

/// Whether a session is one of a conversation's pages.
pub fn preview_session_belongs_to_conversation(session_id: &str, conversation_id: &str) -> bool {
    session_id.strip_prefix(conversation_id).is_some_and(|rest| rest.is_empty() || rest.starts_with('#'))
}

/// Whether a session is the conversation's own page.
pub fn is_primary_preview_session(session_id: &str, conversation_id: &str) -> bool {
    session_id == conversation_id
}

fn clamp_flex(value: f64) -> f64 {
    if value.is_nan() { MIN_SIDE_FLEX } else { value.max(MIN_SIDE_FLEX).min(MAX_SIDE_FLEX) }
}

/// How many columns `count` side panes may spread over: two, and a third once there are more than six.
pub fn side_column_limit(count: usize) -> usize {
    if count > 2 * MAX_PANES_PER_COLUMN { 3 } else { 2 }
}

fn keep_rank(pane: &str) -> usize {
    let kind = pane_kind(pane);
    PANE_KEEP_ORDER.iter().position(|kept| *kept == kind).unwrap_or(PANE_KEEP_ORDER.len())
}

/// The pane a full side area gives up for a new one: the least kept kind, and of those the first opened.
pub fn pane_closed_for_room(layout: &SidePaneLayout) -> Option<&str> {
    let mut chosen: Option<&str> = None;
    for pane in &layout.panes {
        if chosen.is_none_or(|current| keep_rank(pane) > keep_rank(current)) {
            chosen = Some(pane);
        }
    }
    chosen
}

/// The column with the fewest panes; the rightmost of equals, next to where a new column would open.
fn shortest_column(columns: &[Vec<SidePaneId>]) -> usize {
    let mut shortest = 0;
    for (index, column) in columns.iter().enumerate() {
        if column.len() <= columns[shortest].len() {
            shortest = index;
        }
    }
    shortest
}

/// Scales weights to average one, so they stay comparable to the clamp range however columns come and go.
fn normalized_weights(weights: &[f64]) -> Vec<f64> {
    let total: f64 = weights.iter().sum();
    let count = weights.len() as f64;
    if total > 0.0 { weights.iter().map(|weight| weight * count / total).collect() } else { vec![1.0; weights.len()] }
}

fn column_weights(layout: &SidePaneLayout) -> Vec<f64> {
    (0..layout.columns.len()).map(|index| layout.column_flex.get(index).copied().filter(|weight| weight.is_finite() && *weight > 0.0).unwrap_or(1.0)).collect()
}

/// Opens a column on the right. The chat gives up the room, the side area growing by [`NEW_COLUMN_FLEX`], and the whole
/// side area is then split evenly between the columns, whatever widths they had: weighing the new column against the old
/// ones left it a sliver beside a document-wide first column.
fn add_column(layout: &mut SidePaneLayout, pane: SidePaneId) {
    layout.columns.push(vec![pane]);
    layout.column_flex = vec![1.0; layout.columns.len()];
    layout.side_flex = clamp_flex(layout.side_flex + NEW_COLUMN_FLEX);
}

/// Removes a column; the others close up over its room in proportion. The chat takes back what opening a column took
/// from it, or the column's whole width once the area has been narrowed below that, so a column opened and closed again
/// leaves the side area as it was. Shrinking by the column's own width wouldn't: opening splits the area evenly, so every
/// open and close would leave it narrower.
fn remove_column(layout: &mut SidePaneLayout, index: usize) {
    let mut weights = column_weights(layout);
    let total: f64 = weights.iter().sum();
    let width = layout.side_flex * weights[index] / total;
    weights.remove(index);
    layout.columns.remove(index);
    layout.column_flex = normalized_weights(&weights);
    // With nothing left the width is kept, the way closing the last pane always kept it.
    if !layout.columns.is_empty() {
        layout.side_flex = clamp_flex(layout.side_flex - NEW_COLUMN_FLEX.min(width));
    }
}

/// Places a newly opened pane. The first opens the side area; after that the reference shell's rule: a column holding a
/// single pane takes the new one beneath it, and a column already holding more is left alone for a new column beside it.
/// Once the columns allowed for this many panes are all open, the pane joins the shortest.
fn place_pane(layout: &mut SidePaneLayout, pane: SidePaneId, first_side_flex: f64) {
    layout.panes.push(pane.clone());
    if layout.columns.is_empty() {
        layout.columns = vec![vec![pane]];
        layout.column_flex = vec![1.0];
        layout.side_flex = first_side_flex;
        return;
    }
    let last = layout.columns.len() - 1;
    if layout.columns[last].len() == 1 {
        layout.columns[last].push(pane);
    } else if layout.columns.len() < side_column_limit(layout.panes.len()) {
        add_column(layout, pane);
    } else {
        let target = shortest_column(&layout.columns);
        layout.columns[target].push(pane);
    }
}

/// Takes a pane out of the side area. Its column closes once empty, and when fewer panes no longer earn the columns open,
/// the column holding the fewest is dissolved into the shortest others: a third column only lasts while more than six
/// panes are open.
fn remove_pane(layout: &mut SidePaneLayout, pane: &str) {
    if !pane_is_open(layout, pane) {
        return;
    }
    layout.panes.retain(|id| id != pane);
    layout.pane_flex.remove(pane);
    for column in &mut layout.columns {
        column.retain(|id| id != pane);
    }
    if let Some(emptied) = layout.columns.iter().position(Vec::is_empty) {
        remove_column(layout, emptied);
    }
    while layout.columns.len() > side_column_limit(layout.panes.len()) {
        let dissolved = shortest_column(&layout.columns);
        let moving = layout.columns[dissolved].clone();
        remove_column(layout, dissolved);
        for id in moving {
            // A height weighed against its old neighbours means nothing among new ones.
            layout.pane_flex.remove(&id);
            let into = shortest_column(&layout.columns);
            layout.columns[into].push(id);
        }
    }
    // The tabs are the pane's: they go with it, and opening it again starts from the one asked for.
    if pane == "subagent" {
        layout.subagent_tabs.clear();
        layout.active_subagent = None;
    }
}

/// Gives `from`'s place (column, height, focus, expansion) to `to`, which must not be open.
fn rename_pane(layout: &mut SidePaneLayout, from: &str, to: &str) {
    let rename = |id: &mut SidePaneId| {
        if id == from {
            to.clone_into(id);
        }
    };
    layout.panes.iter_mut().for_each(rename);
    layout.columns.iter_mut().flatten().for_each(rename);
    layout.focused.iter_mut().for_each(rename);
    layout.expanded.iter_mut().for_each(rename);
    if let Some(flex) = layout.pane_flex.remove(from) {
        layout.pane_flex.insert(to.to_owned(), flex);
    }
}

/// `items` put into the order `ids` names, for a list the user has just rearranged by hand (lib/reorder.ts).
///
/// The request comes from a view that can be a render behind the list: a tab that closed while another was dragged, or
/// one that opened meanwhile. So an id the list doesn't hold is ignored, a repeated id counts once, and an item the
/// request leaves out keeps its place relative to the other left-out items, after every named one: a stale request can
/// neither drop nor invent an item. `None` when the order wouldn't change.
fn arrange_by_ids(items: &[String], ids: &[String]) -> Option<Vec<String>> {
    let mut next: Vec<String> = Vec::with_capacity(items.len());
    for id in ids {
        if items.contains(id) && !next.contains(id) {
            next.push(id.clone());
        }
    }
    let placed = next.len();
    for item in items {
        if !next[..placed].contains(item) {
            next.push(item.clone());
        }
    }
    if next.iter().zip(items).all(|(arranged, item)| arranged == item) { None } else { Some(next) }
}

/// Stores a conversation's layout. False when that's what it already has.
fn set_layout(state: &mut SidePanesState, conversation_id: &str, layout: SidePaneLayout) -> bool {
    if *side_pane_layout_for(state, Some(conversation_id)) == layout {
        return false;
    }
    state.layout_by_conversation.insert(conversation_id.to_owned(), layout);
    true
}

fn open(state: &mut SidePanesState, conversation_id: &str, pane: &str) -> bool {
    let layout = side_pane_layout_for(state, Some(conversation_id)).clone();
    let kind = pane_kind(pane);
    // A subagent pane without a tab has no transcript to show; `OpenSubagent` opens it.
    if pane == "subagent" && layout.subagent_tabs.is_empty() {
        return false;
    }
    let registered = kind == SidePaneKind::Preview && register_preview(state, conversation_id, pane_target(pane).unwrap_or_default(), None);
    if pane_is_open(&layout, pane) {
        // Asking for a pane that some other pane is currently covering means asking to see it.
        let expanded = layout.expanded.clone().filter(|expanded| expanded == pane);
        if layout.focused.as_deref() == Some(pane) && layout.expanded == expanded {
            return registered;
        }
        return set_layout(state, conversation_id, SidePaneLayout { focused: Some(pane.to_owned()), expanded, ..layout }) || registered;
    }
    // A conversation shows one page at a time, so a second one takes the first one's place, its expansion included:
    // switching pages or adding one inside a maximized preview keeps it maximized. A page opened while another pane
    // covers the preview is uncovered instead.
    let previous = if kind == SidePaneKind::Preview { layout.panes.iter().find(|id| pane_kind(id) == SidePaneKind::Preview).cloned() } else { None };
    let mut next = layout;
    if let Some(previous) = previous {
        let expanded = next.expanded.as_deref() == Some(previous.as_str());
        rename_pane(&mut next, &previous, pane);
        next.focused = Some(pane.to_owned());
        next.expanded = expanded.then(|| pane.to_owned());
        return set_layout(state, conversation_id, next) || registered;
    }
    while next.panes.len() >= MAX_SIDE_PANES {
        let Some(closed) = pane_closed_for_room(&next).map(str::to_owned) else { break };
        remove_pane(&mut next, &closed);
    }
    let first_side_flex = state.last_side_flex_by_kind.get(&kind).copied().unwrap_or_else(|| default_side_flex_for_kind(kind));
    place_pane(&mut next, pane.to_owned(), first_side_flex);
    next.focused = Some(pane.to_owned());
    next.expanded = None;
    set_layout(state, conversation_id, next) || registered
}

fn close(state: &mut SidePanesState, conversation_id: &str, pane: &str) -> bool {
    let mut layout = side_pane_layout_for(state, Some(conversation_id)).clone();
    if !pane_is_open(&layout, pane) {
        return false;
    }
    remove_pane(&mut layout, pane);
    layout.focused = focused_pane(&layout).map(str::to_owned);
    set_layout(state, conversation_id, layout)
}

fn set_preview_workspace(state: &mut SidePanesState, session_id: &str, workspace: u32) -> bool {
    if workspace < 1 || state.preview_workspaces.get(session_id) == Some(&workspace) {
        return false;
    }
    state.preview_workspaces.insert(session_id.to_owned(), workspace);
    true
}

fn register_preview(state: &mut SidePanesState, conversation_id: &str, session_id: &str, workspace: Option<u32>) -> bool {
    let placed = workspace.is_some_and(|workspace| set_preview_workspace(state, session_id, workspace));
    let sessions = preview_sessions_for(state, Some(conversation_id));
    if sessions.iter().any(|session| session == session_id) {
        return placed;
    }
    let mut sessions = sessions.to_vec();
    sessions.push(session_id.to_owned());
    state.preview_sessions.insert(conversation_id.to_owned(), sessions);
    true
}

/// Applies an action in place, and says whether it changed anything.
fn reduce(state: &mut SidePanesState, action: &SidePanesAction) -> bool {
    use SidePanesAction as Action;
    let conversation_id = action.conversation_id();
    let layout = side_pane_layout_for(state, Some(conversation_id)).clone();
    match action {
        Action::Open { pane, .. } => open(state, conversation_id, pane),
        Action::Toggle { pane, .. } => {
            if pane_is_open(&layout, pane) {
                close(state, conversation_id, pane)
            } else {
                open(state, conversation_id, pane)
            }
        }
        Action::Close { pane, .. } => close(state, conversation_id, pane),
        Action::CloseLast { .. } => match focused_pane(&layout) {
            Some(pane) => close(state, conversation_id, pane),
            None => false,
        },
        Action::Replace { pane, replacement, .. } => {
            if !pane_is_open(&layout, pane) || pane == replacement {
                return false;
            }
            // The subagent pane's subject is its shown tab, which `RetargetSubagent` moves; the pane itself is never
            // another pane, and no other pane turns into it without a tab to show.
            if pane == "subagent" || replacement == "subagent" {
                return false;
            }
            if pane_is_open(&layout, replacement) {
                return close(state, conversation_id, pane);
            }
            let mut next = layout;
            rename_pane(&mut next, pane, replacement);
            set_layout(state, conversation_id, next)
        }
        Action::Focus { pane, .. } => {
            if !pane_is_open(&layout, pane) || layout.focused.as_ref() == Some(pane) {
                return false;
            }
            set_layout(state, conversation_id, SidePaneLayout { focused: Some(pane.clone()), ..layout })
        }
        Action::ToggleExpand { pane, .. } => {
            if !pane_is_open(&layout, pane) {
                return false;
            }
            let expanded = if layout.expanded.as_ref() == Some(pane) { None } else { Some(pane.clone()) };
            set_layout(state, conversation_id, SidePaneLayout { focused: Some(pane.clone()), expanded, ..layout })
        }
        Action::SetSideFlex { side_flex, .. } => {
            let side_flex = clamp_flex(*side_flex);
            let mut changed = false;
            // Only a single column's width says how wide that kind of pane likes to open; several columns' combined
            // width would open the next lone pane far too wide.
            if layout.columns.len() == 1 {
                if let Some(first) = layout.panes.first() {
                    let kind = pane_kind(first);
                    if state.last_side_flex_by_kind.get(&kind) != Some(&side_flex) {
                        state.last_side_flex_by_kind.insert(kind, side_flex);
                        changed = true;
                    }
                }
            }
            if layout.side_flex == side_flex {
                return changed;
            }
            set_layout(state, conversation_id, SidePaneLayout { side_flex, ..layout }) || changed
        }
        Action::SetColumnFlex { column_flex, .. } => {
            let next: Vec<f64> = (0..layout.columns.len())
                .map(|index| clamp_flex(column_flex.get(index).or(layout.column_flex.get(index)).copied().unwrap_or(1.0)))
                .collect();
            if next == layout.column_flex {
                return false;
            }
            set_layout(state, conversation_id, SidePaneLayout { column_flex: next, ..layout })
        }
        Action::SetPaneFlex { pane_flex, .. } => {
            let next: BTreeMap<String, f64> = pane_flex.iter().map(|(id, value)| (id.clone(), clamp_flex(*value))).collect();
            if next == layout.pane_flex {
                return false;
            }
            set_layout(state, conversation_id, SidePaneLayout { pane_flex: next, ..layout })
        }
        Action::OpenSubagent { subagent_id, .. } => {
            let known = layout.subagent_tabs.contains(subagent_id);
            let mut selected = false;
            if !known || layout.active_subagent.as_ref() != Some(subagent_id) {
                let mut next = layout;
                if !known {
                    next.subagent_tabs.push(subagent_id.clone());
                }
                next.active_subagent = Some(subagent_id.clone());
                selected = set_layout(state, conversation_id, next);
            }
            open(state, conversation_id, "subagent") || selected
        }
        Action::CloseSubagent { subagent_id, .. } => {
            let Some(index) = layout.subagent_tabs.iter().position(|id| id == subagent_id) else { return false };
            let mut next = layout;
            next.subagent_tabs.retain(|id| id != subagent_id);
            if next.subagent_tabs.is_empty() {
                return close(state, conversation_id, "subagent");
            }
            // Focus falls to the right, as a browser's does, and to the left off the end.
            if next.active_subagent.as_ref() == Some(subagent_id) {
                next.active_subagent = Some(next.subagent_tabs[index.min(next.subagent_tabs.len() - 1)].clone());
            }
            set_layout(state, conversation_id, next)
        }
        Action::RetargetSubagent { from, to, .. } => {
            if from == to || !layout.subagent_tabs.contains(from) {
                return false;
            }
            let mut next = layout;
            if next.subagent_tabs.contains(to) {
                next.subagent_tabs.retain(|id| id != from);
            } else {
                for id in &mut next.subagent_tabs {
                    if id == from {
                        id.clone_from(to);
                    }
                }
            }
            if next.active_subagent.as_ref() == Some(from) {
                next.active_subagent = Some(to.clone());
            }
            set_layout(state, conversation_id, next)
        }
        Action::ReorderSubagents { subagent_ids, .. } => match arrange_by_ids(&layout.subagent_tabs, subagent_ids) {
            Some(subagent_tabs) => set_layout(state, conversation_id, SidePaneLayout { subagent_tabs, ..layout }),
            None => false,
        },
        Action::ShowTasksTab { tab, .. } => {
            let selected = layout.tasks_tab != *tab && set_layout(state, conversation_id, SidePaneLayout { tasks_tab: *tab, ..layout });
            open(state, conversation_id, "tasks") || selected
        }
        Action::RegisterPreview { session_id, workspace, .. } => register_preview(state, conversation_id, session_id, *workspace),
        Action::ForgetPreview { session_id, .. } => {
            let closed = close(state, conversation_id, &preview_pane_id(session_id));
            let sessions = preview_sessions_for(state, Some(conversation_id));
            if !sessions.contains(session_id) && !state.preview_workspaces.contains_key(session_id) {
                return closed;
            }
            let remaining: Vec<String> = sessions.iter().filter(|session| *session != session_id).cloned().collect();
            if remaining.is_empty() {
                state.preview_sessions.remove(conversation_id);
            } else {
                state.preview_sessions.insert(conversation_id.to_owned(), remaining);
            }
            state.preview_workspaces.remove(session_id);
            true
        }
        Action::SetPreviewWorkspace { session_id, workspace, .. } => set_preview_workspace(state, session_id, *workspace),
        Action::ReorderPreviews { session_ids, .. } => match arrange_by_ids(preview_sessions_for(state, Some(conversation_id)), session_ids) {
            Some(sessions) => {
                state.preview_sessions.insert(conversation_id.to_owned(), sessions);
                true
            }
            None => false,
        },
        Action::AdoptConversation { from, .. } => {
            if from == conversation_id {
                return false;
            }
            let Some(adopted) = state.layout_by_conversation.remove(from) else { return false };
            // The draft's page is keyed by the id it materializes as, so its pane follows. A session id minted under the
            // placeholder id names nothing the conversation owns and stays behind.
            let follows = |session: &str| preview_session_belongs_to_conversation(session, conversation_id);
            let mut carried = SidePaneLayout { expanded: None, ..adopted };
            for pane in carried.panes.clone() {
                if pane_kind(&pane) == SidePaneKind::Preview && !follows(pane_target(&pane).unwrap_or_default()) {
                    remove_pane(&mut carried, &pane);
                }
            }
            carried.focused = focused_pane(&carried).map(str::to_owned);
            state.layout_by_conversation.insert(conversation_id.to_owned(), carried);
            let roster: Vec<String> = preview_sessions_for(state, Some(from)).iter().filter(|session| follows(session)).cloned().collect();
            state.preview_sessions.remove(from);
            if !roster.is_empty() {
                let mut sessions: Vec<String> = preview_sessions_for(state, Some(conversation_id)).iter().filter(|session| !roster.contains(session)).cloned().collect();
                sessions.extend(roster);
                state.preview_sessions.insert(conversation_id.to_owned(), sessions);
            }
            true
        }
        Action::RemoveConversation { .. } => {
            if !state.layout_by_conversation.contains_key(conversation_id) && !state.preview_sessions.contains_key(conversation_id) {
                return false;
            }
            for session in preview_sessions_for(state, Some(conversation_id)).to_vec() {
                state.preview_workspaces.remove(&session);
            }
            state.layout_by_conversation.remove(conversation_id);
            state.preview_sessions.remove(conversation_id);
            true
        }
    }
}

/// The state after an action, or `None` when it changes nothing (upstream's returning the same object).
pub fn side_panes_reducer(state: &SidePanesState, action: SidePanesAction) -> Option<SidePanesState> {
    let mut next = state.clone();
    reduce(&mut next, &action).then_some(next)
}

/// The widths worth keeping, as stored: known kinds only, clamped, without NaN.
fn stored_flex(widths: impl Fn(SidePaneKind) -> Option<f64>) -> BTreeMap<SidePaneKind, f64> {
    KINDS.into_iter().filter_map(|kind| Some((kind, clamp_flex(widths(kind).filter(|flex| !flex.is_nan())?)))).collect()
}

/// The state a start begins from: only the remembered widths outlive the app. A missing or malformed entry reads as none.
pub fn load_side_panes_state(store: &UiStore) -> SidePanesState {
    let stored: Option<Map<String, Value>> = store.get_json::<Value>(STORAGE_KEY).and_then(|value| match value {
        Value::Object(map) => Some(map),
        _ => None,
    });
    let last_side_flex_by_kind = stored.map_or_else(BTreeMap::new, |map| stored_flex(|kind| map.get(kind.as_str()).and_then(Value::as_f64)));
    SidePanesState { last_side_flex_by_kind, ..SidePanesState::default() }
}

/// Stores the remembered widths.
pub fn persist_side_panes_state(store: &mut UiStore, state: &SidePanesState) {
    let widths = stored_flex(|kind| state.last_side_flex_by_kind.get(&kind).copied());
    let by_name: BTreeMap<&str, f64> = widths.into_iter().map(|(kind, flex)| (kind.as_str(), flex)).collect();
    store.set_json(STORAGE_KEY, &by_name);
}

/// Whether [`App::side_panes`] has read the store yet.
#[derive(Default)]
struct SidePanesLoaded(bool);

impl App {
    /// The side panes' state, read from the store the first time.
    pub fn side_panes(&mut self) -> &SidePanesState {
        if !self.local::<SidePanesLoaded>().0 {
            self.local::<SidePanesLoaded>().0 = true;
            let loaded = load_side_panes_state(self.ui_store());
            *self.local::<SidePanesState>() = loaded;
        }
        self.local::<SidePanesState>()
    }

    /// `dispatchSidePanes`: applies an action, persisting only when the remembered widths changed. Consecutive dispatches
    /// observe each other. Asks for no frame: a handler already gets one, and anywhere else the caller asks.
    pub fn dispatch_side_panes(&mut self, action: SidePanesAction) {
        self.side_panes();
        let state = self.local::<SidePanesState>();
        let remembered = state.last_side_flex_by_kind.clone();
        if !reduce(state, &action) || state.last_side_flex_by_kind == remembered {
            return;
        }
        let widths = SidePanesState { last_side_flex_by_kind: state.last_side_flex_by_kind.clone(), ..SidePanesState::default() };
        self.ui_store_update(|store| persist_side_panes_state(store, &widths));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    const CONVERSATION_ID: &str = "conversation-1";
    const DRAFT: &str = "__draft__";

    fn cid() -> String {
        CONVERSATION_ID.to_owned()
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn apply(state: &SidePanesState, action: SidePanesAction) -> SidePanesState {
        side_panes_reducer(state, action).unwrap_or_else(|| state.clone())
    }

    /// Upstream's `expect(apply(state, action)).toBe(state)`.
    fn assert_unchanged(state: &SidePanesState, action: SidePanesAction) {
        let description = format!("{action:?}");
        assert_eq!(side_panes_reducer(state, action), None, "{description}");
    }

    fn open_in(state: &SidePanesState, pane: &str, id: &str) -> SidePanesState {
        apply(state, SidePanesAction::Open { conversation_id: id.to_owned(), pane: pane.to_owned() })
    }

    fn open_pane(state: &SidePanesState, pane: &str) -> SidePanesState {
        open_in(state, pane, CONVERSATION_ID)
    }

    fn open_action(pane: &str) -> SidePanesAction {
        SidePanesAction::Open { conversation_id: cid(), pane: pane.to_owned() }
    }

    fn toggle_action(pane: &str) -> SidePanesAction {
        SidePanesAction::Toggle { conversation_id: cid(), pane: pane.to_owned() }
    }

    fn close_action(pane: &str) -> SidePanesAction {
        SidePanesAction::Close { conversation_id: cid(), pane: pane.to_owned() }
    }

    fn close_last_action() -> SidePanesAction {
        SidePanesAction::CloseLast { conversation_id: cid() }
    }

    fn replace_action(pane: &str, replacement: &str) -> SidePanesAction {
        SidePanesAction::Replace { conversation_id: cid(), pane: pane.to_owned(), replacement: replacement.to_owned() }
    }

    fn focus_action(pane: &str) -> SidePanesAction {
        SidePanesAction::Focus { conversation_id: cid(), pane: pane.to_owned() }
    }

    fn toggle_expand_action(pane: &str) -> SidePanesAction {
        SidePanesAction::ToggleExpand { conversation_id: cid(), pane: pane.to_owned() }
    }

    fn side_flex_action(side_flex: f64) -> SidePanesAction {
        SidePanesAction::SetSideFlex { conversation_id: cid(), side_flex }
    }

    fn column_flex_action(column_flex: &[f64]) -> SidePanesAction {
        SidePanesAction::SetColumnFlex { conversation_id: cid(), column_flex: column_flex.to_vec() }
    }

    fn flexes(entries: &[(&str, f64)]) -> BTreeMap<String, f64> {
        entries.iter().map(|(id, flex)| ((*id).to_owned(), *flex)).collect()
    }

    fn pane_flex_action(entries: &[(&str, f64)]) -> SidePanesAction {
        SidePanesAction::SetPaneFlex { conversation_id: cid(), pane_flex: flexes(entries) }
    }

    fn close_subagent_action(subagent_id: &str) -> SidePanesAction {
        SidePanesAction::CloseSubagent { conversation_id: cid(), subagent_id: subagent_id.to_owned() }
    }

    fn retarget_action(from: &str, to: &str) -> SidePanesAction {
        SidePanesAction::RetargetSubagent { conversation_id: cid(), from: from.to_owned(), to: to.to_owned() }
    }

    fn reorder_subagents_action(ids: &[&str]) -> SidePanesAction {
        SidePanesAction::ReorderSubagents { conversation_id: cid(), subagent_ids: strs(ids) }
    }

    fn register_action(session_id: &str, workspace: Option<u32>) -> SidePanesAction {
        SidePanesAction::RegisterPreview { conversation_id: cid(), session_id: session_id.to_owned(), workspace }
    }

    fn forget_action(session_id: &str) -> SidePanesAction {
        SidePanesAction::ForgetPreview { conversation_id: cid(), session_id: session_id.to_owned() }
    }

    fn set_workspace_action(session_id: &str, workspace: u32) -> SidePanesAction {
        SidePanesAction::SetPreviewWorkspace { conversation_id: cid(), session_id: session_id.to_owned(), workspace }
    }

    fn reorder_previews_action(ids: &[&str]) -> SidePanesAction {
        SidePanesAction::ReorderPreviews { conversation_id: cid(), session_ids: strs(ids) }
    }

    fn adopt_action(from: &str) -> SidePanesAction {
        SidePanesAction::AdoptConversation { conversation_id: cid(), from: from.to_owned() }
    }

    fn remove_action() -> SidePanesAction {
        SidePanesAction::RemoveConversation { conversation_id: cid() }
    }

    fn layout(state: &SidePanesState) -> &SidePaneLayout {
        side_pane_layout_for(state, Some(CONVERSATION_ID))
    }

    fn roster(state: &SidePanesState) -> &[String] {
        preview_sessions_for(state, Some(CONVERSATION_ID))
    }

    fn initial() -> SidePanesState {
        initial_side_panes_state()
    }

    fn stack(panes: &[&str]) -> SidePanesState {
        panes.iter().fold(initial(), |state, pane| open_pane(&state, pane))
    }

    fn open_agent(state: &SidePanesState, subagent_id: &str) -> SidePanesState {
        apply(state, SidePanesAction::OpenSubagent { conversation_id: cid(), subagent_id: subagent_id.to_owned() })
    }

    fn agents(state: SidePanesState, ids: &[&str]) -> SidePanesState {
        ids.iter().fold(state, |state, id| open_agent(&state, id))
    }

    fn show_tab(state: &SidePanesState, tab: TasksPaneTab) -> SidePanesState {
        apply(state, SidePanesAction::ShowTasksTab { conversation_id: cid(), tab })
    }

    /// `toBeCloseTo` at its default precision.
    #[track_caller]
    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 0.005, "{actual} is not close to {expected}");
    }

    fn columns_of(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter().map(|row| strs(row)).collect()
    }

    /// Every open pane in exactly one column, and no more columns, rows or panes than allowed.
    #[track_caller]
    fn expect_well_formed(pane: &SidePaneLayout) {
        let mut placed: Vec<String> = pane.columns.iter().flatten().cloned().collect();
        placed.sort();
        let mut open = pane.panes.clone();
        open.sort();
        assert_eq!(placed, open);
        open.dedup();
        assert_eq!(open.len(), pane.panes.len());
        assert!(pane.panes.len() <= MAX_SIDE_PANES);
        assert!(pane.columns.len() <= if pane.panes.is_empty() { 0 } else { side_column_limit(pane.panes.len()) });
        for column in &pane.columns {
            assert!(!column.is_empty());
            assert!(column.len() <= MAX_PANES_PER_COLUMN);
        }
        assert_eq!(pane.column_flex.len(), pane.columns.len());
        for weight in &pane.column_flex {
            assert!(*weight > 0.0);
        }
        assert!(pane.side_flex >= MIN_SIDE_FLEX);
        assert!(pane.side_flex <= MAX_SIDE_FLEX);
        // The subagent pane is open exactly while it has a tab, and it shows one of them.
        assert_eq!(!pane.subagent_tabs.is_empty(), pane_is_open(pane, "subagent"));
        let mut tabs = pane.subagent_tabs.clone();
        tabs.sort();
        tabs.dedup();
        assert_eq!(tabs.len(), pane.subagent_tabs.len());
        if pane.subagent_tabs.is_empty() {
            assert_eq!(pane.active_subagent, None);
        } else {
            assert!(pane.subagent_tabs.contains(pane.active_subagent.as_ref().unwrap()));
        }
    }

    /// Each column's width in the chat's units, the way the tiles are drawn.
    fn column_widths(pane: &SidePaneLayout) -> Vec<f64> {
        let total: f64 = pane.column_flex.iter().sum();
        pane.column_flex.iter().map(|weight| weight / total * pane.side_flex).collect()
    }

    const SEVEN: [&str; 7] = ["settings", "plan", "preview:a", "terminal", "review", "files", "tasks"];

    fn with_seven(more: &[&str]) -> Vec<String> {
        strs(&SEVEN).into_iter().chain(strs(more)).collect()
    }

    fn stack_seven(more: &[&str]) -> SidePanesState {
        let panes = with_seven(more);
        panes.iter().fold(initial(), |state, pane| open_pane(&state, pane))
    }

    // side pane helpers

    #[test]
    fn shares_a_deeply_frozen_absent_layout_across_conversations_and_null() {
        let state = initial();
        let empty = layout(&state);
        assert!(ptr::eq(empty, side_pane_layout_for(&state, Some("other"))));
        assert!(ptr::eq(empty, side_pane_layout_for(&state, None)));
        assert!(empty.panes.is_empty());
    }

    #[test]
    fn shares_a_frozen_empty_roster_but_returns_an_existing_roster_verbatim() {
        let empty = initial();
        assert!(roster(&empty).is_empty());
        assert!(preview_sessions_for(&empty, None).is_empty());
        assert!(preview_sessions_for(&empty, Some("other")).is_empty());
        let state = stack(&["preview:a"]);
        assert!(ptr::eq(roster(&state), state.preview_sessions[CONVERSATION_ID].as_slice()));
        assert!(ptr::eq(layout(&state), &state.layout_by_conversation[CONVERSATION_ID]));
    }

    #[test]
    fn does_not_resolve_prototype_properties_as_layouts_or_rosters() {
        let state = initial();
        assert!(ptr::eq(side_pane_layout_for(&state, Some("constructor")), layout(&state)));
        assert!(preview_sessions_for(&state, Some("__proto__")).is_empty());
    }

    #[test]
    fn defaults_each_kind_to_its_flex() {
        for (kind, flex) in [
            (SidePaneKind::Terminal, 1.0),
            (SidePaneKind::Review, 3.0),
            (SidePaneKind::Preview, 3.0),
            (SidePaneKind::Files, 3.0),
            (SidePaneKind::Settings, 3.0),
            (SidePaneKind::Tasks, 1.0),
            (SidePaneKind::Plan, 1.0),
            (SidePaneKind::History, 1.0),
            (SidePaneKind::Subagent, 1.0),
        ] {
            assert_eq!(default_side_flex_for_kind(kind), flex, "{kind:?}");
        }
        assert_eq!(CHAT_TILE_FLEX, 2.0);
        assert_eq!(MIN_SIDE_FLEX, 0.25);
        assert_eq!(MAX_SIDE_FLEX, 8.0);
    }

    #[test]
    fn constructs_and_splits_resource_ids_without_dropping_colons_in_the_target() {
        assert_eq!(preview_pane_id("a:b"), "preview:a:b");
        assert_eq!(pane_kind("preview:a:b"), SidePaneKind::Preview);
        assert_eq!(pane_target("preview:a:b"), Some("a:b"));
        assert_eq!(pane_target("preview:"), Some(""));
        assert_eq!(pane_kind("terminal"), SidePaneKind::Terminal);
        assert_eq!(pane_target("terminal"), None);
    }

    #[test]
    fn reports_open_panes_and_the_current_native_preview_session() {
        let state = stack(&["terminal", "preview:a:b", "tasks"]);
        let value = layout(&state);
        assert!(pane_is_open(value, "terminal"));
        assert!(!pane_is_open(value, "review"));
        assert_eq!(open_preview_session(value), Some("a:b"));
        assert_eq!(open_preview_session(layout(&stack(&["terminal"]))), None);
    }

    #[test]
    fn falls_back_from_missing_or_stale_focus_to_the_last_pane_then_null() {
        let state = stack(&["terminal", "tasks"]);
        let value = layout(&state);
        assert_eq!(focused_pane(&SidePaneLayout { focused: Some("terminal".to_owned()), ..value.clone() }), Some("terminal"));
        assert_eq!(focused_pane(&SidePaneLayout { focused: Some("review".to_owned()), ..value.clone() }), Some("tasks"));
        assert_eq!(focused_pane(&SidePaneLayout { focused: None, ..value.clone() }), Some("tasks"));
        assert_eq!(focused_pane(&SidePaneLayout { panes: Vec::new(), ..value.clone() }), None);
    }

    #[test]
    fn preserves_preview_tab_ownership_and_primary_session_semantics() {
        assert_eq!(preview_tab_session_id("c", "a"), "c#a");
        assert!(preview_session_belongs_to_conversation("c", "c"));
        assert!(preview_session_belongs_to_conversation("c#a", "c"));
        assert!(!preview_session_belongs_to_conversation("cc#a", "c"));
        assert!(!preview_session_belongs_to_conversation("d#a", "c"));
        assert!(is_primary_preview_session("c", "c"));
        assert!(!is_primary_preview_session("c#a", "c"));
    }

    #[test]
    fn encodes_dom_ids_by_base_36_unicode_code_points_with_an_unambiguous_prefix() {
        assert_eq!(side_pane_dom_id("preview:A😀"), "side-pane-34-36-2t-3a-2x-2t-3b-1m-1t-2r5s");
        assert_ne!(side_pane_dom_id("preview:a-b"), side_pane_dom_id("preview:a:b"));
        assert_eq!(side_pane_dom_id("terminal"), side_pane_dom_id("terminal"));
        let id = side_pane_dom_id("history: /#中");
        let encoded = id.strip_prefix("side-pane-").unwrap();
        assert!(!encoded.is_empty() && encoded.chars().all(|c| matches!(c, '0'..='9' | 'a'..='z' | '-')));
    }

    // sidePanesReducer

    #[test]
    fn appends_panes_focuses_the_newest_and_keeps_the_first_panes_column_ratio() {
        let state = stack(&["terminal", "review"]);
        assert_eq!(
            *layout(&state),
            SidePaneLayout {
                panes: strs(&["terminal", "review"]),
                columns: columns_of(&[&["terminal", "review"]]),
                focused: Some("review".to_owned()),
                side_flex: 1.0,
                column_flex: vec![1.0],
                pane_flex: BTreeMap::new(),
                subagent_tabs: Vec::new(),
                active_subagent: None,
                tasks_tab: TasksPaneTab::Tasks,
                expanded: None,
            }
        );
        assert_eq!(layout(&stack(&["review", "terminal"])).side_flex, 3.0);
        assert!(initial().layout_by_conversation.is_empty());
    }

    #[test]
    fn keeps_conversations_independent_with_structural_sharing() {
        let first = stack(&["review"]);
        let next = open_in(&first, "terminal", "other");
        assert_eq!(layout(&next), layout(&first));
        assert_eq!(side_pane_layout_for(&next, Some("other")).panes, ["terminal"]);
        assert_eq!(next.preview_sessions, first.preview_sessions);
    }

    #[test]
    fn opening_an_open_pane_only_refocuses_it_and_preserves_stack_and_flex_identity() {
        let state = stack(&["terminal", "tasks"]);
        let next = open_pane(&state, "terminal");
        assert_ne!(next, state);
        assert_eq!(layout(&next).focused.as_deref(), Some("terminal"));
        assert_eq!(layout(&next).panes, layout(&state).panes);
        assert_eq!(layout(&next).pane_flex, layout(&state).pane_flex);
        assert_unchanged(&next, open_action("terminal"));
    }

    #[test]
    fn toggles_a_missing_pane_on_and_an_open_pane_off() {
        let state = apply(&initial(), toggle_action("preview:a"));
        assert_eq!(layout(&state).panes, ["preview:a"]);
        let closed = apply(&state, toggle_action("preview:a"));
        assert!(layout(&closed).panes.is_empty());
        assert_eq!(roster(&closed), ["a"]);
    }

    #[test]
    fn closes_a_pane_removes_its_vertical_slot_and_retains_column_ratio() {
        let state = apply(&stack(&["review"]), pane_flex_action(&[("review", 2.0)]));
        let next = apply(&state, close_action("review"));
        assert_eq!(*layout(&next), SidePaneLayout { side_flex: 3.0, ..SidePaneLayout::default() });
        assert_eq!(layout(&state).pane_flex, flexes(&[("review", 2.0)]));
    }

    #[test]
    fn closing_an_unfocused_pane_keeps_focus_and_closing_focus_falls_back_to_last() {
        let state = stack(&["terminal", "review", "tasks"]);
        let next = apply(&state, close_action("review"));
        assert_eq!(layout(&next).focused.as_deref(), Some("tasks"));
        assert_eq!(layout(&apply(&next, close_action("tasks"))).focused.as_deref(), Some("terminal"));
    }

    #[test]
    fn close_last_removes_the_focused_pane_rather_than_necessarily_the_last() {
        let state = apply(&stack(&["terminal", "review", "tasks"]), focus_action("terminal"));
        let next = apply(&state, close_last_action());
        assert_eq!(layout(&next).panes, ["review", "tasks"]);
        assert_eq!(layout(&next).focused.as_deref(), Some("tasks"));
    }

    #[test]
    fn close_last_falls_back_to_the_final_pane_for_stale_focus() {
        let mut stale = stack(&["terminal", "tasks"]);
        stale.layout_by_conversation.get_mut(CONVERSATION_ID).unwrap().focused = Some("review".to_owned());
        assert_eq!(layout(&apply(&stale, close_last_action())).panes, ["terminal"]);
    }

    #[test]
    fn focus_changes_only_for_an_open_pane_with_a_different_focus() {
        let state = stack(&["terminal", "review"]);
        assert_unchanged(&state, focus_action("tasks"));
        assert_unchanged(&state, focus_action("review"));
        let next = apply(&state, focus_action("terminal"));
        assert_eq!(layout(&next).focused.as_deref(), Some("terminal"));
        assert_eq!(layout(&next).panes, layout(&state).panes);
    }

    #[test]
    fn preserves_identity_for_absent_close_empty_close_last_and_absent_removal() {
        for action in [close_action("terminal"), close_last_action(), remove_action(), focus_action("terminal"), forget_action("unknown")] {
            assert_unchanged(&initial(), action);
        }
        let closed = apply(&stack(&["terminal"]), close_last_action());
        assert_unchanged(&closed, close_last_action());
        assert_eq!(apply(&stack(&["tasks"]), close_action("terminal")).layout_by_conversation[CONVERSATION_ID].panes, ["tasks"]);
    }

    #[test]
    fn replaces_previews_in_place_renames_their_flex_slot_and_keeps_both_sessions() {
        let state = apply(&stack(&["terminal", "preview:a", "tasks"]), pane_flex_action(&[("terminal", 2.0), ("preview:a", 4.0), ("tasks", 3.0)]));
        let next = open_pane(&state, "preview:b");
        assert_eq!(
            *layout(&next),
            SidePaneLayout {
                panes: strs(&["terminal", "preview:b", "tasks"]),
                columns: columns_of(&[&["terminal", "preview:b"], &["tasks"]]),
                side_flex: 2.0,
                column_flex: vec![1.0, 1.0],
                focused: Some("preview:b".to_owned()),
                pane_flex: flexes(&[("terminal", 2.0), ("preview:b", 4.0), ("tasks", 3.0)]),
                ..SidePaneLayout::default()
            }
        );
        assert_eq!(roster(&next), ["a", "b"]);
        assert_eq!(layout(&state).pane_flex["preview:a"], 4.0);
        assert_unchanged(&next, open_action("preview:b"));
    }

    #[test]
    fn replaces_a_preview_without_inventing_an_explicit_default_flex_slot() {
        let next = open_pane(&stack(&["preview:a", "review"]), "preview:b");
        assert_eq!(layout(&next).panes, ["preview:b", "review"]);
        assert!(layout(&next).pane_flex.is_empty());
        assert_eq!(layout(&next).side_flex, 3.0);
    }

    #[test]
    fn uses_remembered_kind_widths_on_first_open_and_after_closing_the_last_pane() {
        let state = SidePanesState { last_side_flex_by_kind: BTreeMap::from([(SidePaneKind::Review, 5.0), (SidePaneKind::Terminal, 2.0)]), ..initial() };
        let shown = open_pane(&state, "review");
        assert_eq!(layout(&shown).side_flex, 5.0);
        assert_eq!(layout(&open_pane(&shown, "terminal")).side_flex, 5.0);
        let closed = apply(&shown, close_last_action());
        assert_eq!(layout(&open_pane(&closed, "terminal")).side_flex, 2.0);
        assert_eq!(layout(&open_pane(&closed, "tasks")).side_flex, 1.0);
    }

    #[test]
    fn clamps_column_flex_and_remembers_it_under_the_first_pane_rather_than_focus() {
        let state = stack(&["preview:a", "tasks"]);
        let max = apply(&state, side_flex_action(20.0));
        assert_eq!(layout(&max).side_flex, 8.0);
        assert_eq!(max.last_side_flex_by_kind, BTreeMap::from([(SidePaneKind::Preview, 8.0)]));
        let min = apply(&max, side_flex_action(-1.0));
        assert_eq!(layout(&min).side_flex, 0.25);
        assert_eq!(min.last_side_flex_by_kind, BTreeMap::from([(SidePaneKind::Preview, 0.25)]));
        assert_unchanged(&min, side_flex_action(-9.0));
    }

    #[test]
    fn records_a_resize_even_when_equal_to_the_default_then_stops_identity_churn() {
        let state = stack(&["terminal"]);
        let next = apply(&state, side_flex_action(1.0));
        assert_eq!(next.last_side_flex_by_kind, BTreeMap::from([(SidePaneKind::Terminal, 1.0)]));
        assert_eq!(layout(&next), layout(&state));
        assert_unchanged(&next, side_flex_action(1.0));
    }

    #[test]
    fn sets_column_flex_without_remembering_a_kind_when_no_panes_are_open() {
        let next = apply(&initial(), side_flex_action(4.0));
        assert_eq!(layout(&next).side_flex, 4.0);
        assert_eq!(next.last_side_flex_by_kind, initial().last_side_flex_by_kind);
        assert_eq!(layout(&open_pane(&next, "terminal")).side_flex, 1.0);
    }

    #[test]
    fn replaces_rather_than_merges_pane_flex_and_compares_clamped_map_values() {
        let state = apply(&stack(&["terminal", "tasks"]), pane_flex_action(&[("terminal", 2.0), ("tasks", 3.0)]));
        let next = apply(&state, pane_flex_action(&[("tasks", 100.0), ("review", -5.0)]));
        assert_eq!(layout(&next).pane_flex, flexes(&[("tasks", 8.0), ("review", 0.25)]));
        assert_unchanged(&next, pane_flex_action(&[("review", -1.0), ("tasks", 9.0)]));
        assert!(layout(&apply(&next, pane_flex_action(&[]))).pane_flex.is_empty());
    }

    #[test]
    fn keeps_invalid_numeric_flex_from_poisoning_layout_math() {
        let next = apply(&stack(&["terminal"]), pane_flex_action(&[("terminal", f64::NAN), ("tasks", f64::INFINITY), ("review", f64::NEG_INFINITY)]));
        assert_eq!(layout(&next).pane_flex, flexes(&[("terminal", 0.25), ("tasks", 8.0), ("review", 0.25)]));
        assert_eq!(layout(&apply(&next, side_flex_action(f64::NAN))).side_flex, 0.25);
    }

    #[test]
    fn registers_previews_in_arrival_order_without_changing_any_layout() {
        let first = apply(&stack(&["terminal"]), register_action("b", None));
        let second = apply(&first, register_action("a", None));
        assert_eq!(second.layout_by_conversation, first.layout_by_conversation);
        assert_eq!(roster(&second), ["b", "a"]);
        assert_unchanged(&second, register_action("a", None));
        assert_eq!(roster(&open_pane(&second, "preview:a")), roster(&second));
    }

    #[test]
    fn forgets_an_open_preview_deletes_an_empty_roster_and_focuses_a_surviving_pane() {
        let next = apply(&stack(&["tasks", "preview:a"]), forget_action("a"));
        assert_eq!(layout(&next).panes, ["tasks"]);
        assert_eq!(layout(&next).focused.as_deref(), Some("tasks"));
        assert!(!next.preview_sessions.contains_key(CONVERSATION_ID));
    }

    #[test]
    fn forgetting_a_hidden_preview_leaves_the_current_layout_identity_alone() {
        let state = stack(&["preview:a", "preview:b"]);
        let next = apply(&state, forget_action("a"));
        assert_eq!(layout(&next), layout(&state));
        assert_eq!(roster(&next), ["b"]);
        assert_unchanged(&next, forget_action("unknown"));
    }

    #[test]
    fn forgetting_an_unregistered_but_referenced_preview_still_closes_its_pane() {
        let state = SidePanesState { preview_sessions: BTreeMap::new(), ..stack(&["preview:a"]) };
        let next = apply(&state, forget_action("a"));
        assert!(layout(&next).panes.is_empty());
        assert_eq!(next.preview_sessions, state.preview_sessions);
    }

    #[test]
    fn removes_a_conversation_layout_and_roster_but_retains_others_and_remembered_widths() {
        let state = apply(&open_in(&stack(&["preview:a"]), "review", "other"), side_flex_action(5.0));
        let next = apply(&state, remove_action());
        assert!(!next.layout_by_conversation.contains_key(CONVERSATION_ID));
        assert!(!next.preview_sessions.contains_key(CONVERSATION_ID));
        assert_eq!(side_pane_layout_for(&next, Some("other")), side_pane_layout_for(&state, Some("other")));
        assert_eq!(next.last_side_flex_by_kind, state.last_side_flex_by_kind);
        assert_unchanged(&next, remove_action());
    }

    #[test]
    fn removes_a_roster_only_conversation() {
        let state = apply(&initial(), register_action("a", None));
        assert_eq!(apply(&state, remove_action()), initial());
    }

    #[test]
    fn carries_a_drafts_layout_onto_the_conversation_it_becomes_leaving_nothing_behind() {
        // A settings pane opened in a draft must follow the draft into the real conversation, and must not reappear in
        // the next brand-new one.
        let state = open_in(&initial(), "settings", DRAFT);
        let next = apply(&state, adopt_action(DRAFT));
        assert!(!next.layout_by_conversation.contains_key(DRAFT));
        assert_eq!(layout(&next).panes, ["settings"]);
        assert_eq!(layout(&next).focused.as_deref(), Some("settings"));
        assert_eq!(layout(&next).side_flex, 3.0);
    }

    #[test]
    fn adopts_nothing_when_the_draft_never_opened_a_pane_and_never_adopts_onto_itself() {
        assert_unchanged(&initial(), adopt_action(DRAFT));
        let state = open_in(&initial(), "settings", DRAFT);
        assert_unchanged(&state, SidePanesAction::AdoptConversation { conversation_id: DRAFT.to_owned(), from: DRAFT.to_owned() });
    }

    #[test]
    fn drops_a_preview_pane_while_adopting_so_a_draft_minted_session_id_cannot_follow() {
        // A page named after the placeholder id belongs to no conversation, so carrying its pane would leave a tile
        // pointing at a session nothing owns.
        let state = open_in(&open_in(&initial(), "settings", DRAFT), "preview:__draft__#1", DRAFT);
        let next = apply(&state, adopt_action(DRAFT));
        assert_eq!(layout(&next).panes, ["settings"]);
        assert_eq!(layout(&next).focused.as_deref(), Some("settings"));
        assert_eq!(layout(&next).expanded, None);
        assert!(next.preview_sessions.is_empty());
    }

    #[test]
    fn carries_the_drafts_own_preview_page_which_is_already_named_after_the_conversation() {
        // The draft opens its page under the id it will materialize as, so the page and its roster entry are the
        // conversation's the moment it is real.
        let state = open_in(&initial(), &preview_pane_id(CONVERSATION_ID), DRAFT);
        assert_eq!(preview_sessions_for(&state, Some(DRAFT)), [CONVERSATION_ID]);
        let next = apply(&state, adopt_action(DRAFT));
        assert_eq!(layout(&next).panes, [preview_pane_id(CONVERSATION_ID)]);
        assert_eq!(roster(&next), [CONVERSATION_ID]);
        assert!(!next.preview_sessions.contains_key(DRAFT));
    }

    // side pane columns

    #[test]
    fn pairs_panes_the_way_the_reference_shell_does_and_opens_a_third_column_only_past_six() {
        let opened = ["settings", "plan", "preview:a", "terminal", "review", "files", "tasks", "history:0", "history:1"];
        let expected: [&[&[&str]]; 9] = [
            &[&["settings"]],
            &[&["settings", "plan"]],
            &[&["settings", "plan"], &["preview:a"]],
            &[&["settings", "plan"], &["preview:a", "terminal"]],
            &[&["settings", "plan"], &["preview:a", "terminal", "review"]],
            &[&["settings", "plan", "files"], &["preview:a", "terminal", "review"]],
            &[&["settings", "plan", "files"], &["preview:a", "terminal", "review"], &["tasks"]],
            &[&["settings", "plan", "files"], &["preview:a", "terminal", "review"], &["tasks", "history:0"]],
            &[&["settings", "plan", "files"], &["preview:a", "terminal", "review"], &["tasks", "history:0", "history:1"]],
        ];
        let mut state = initial();
        for (index, pane) in opened.iter().enumerate() {
            state = open_pane(&state, pane);
            assert_eq!(layout(&state).columns, columns_of(expected[index]));
            expect_well_formed(layout(&state));
        }
    }

    #[test]
    fn allows_two_columns_up_to_six_panes_and_three_beyond() {
        assert_eq!([1, 2, 6, 7, 9].map(side_column_limit), [2, 2, 2, 3, 3]);
    }

    #[test]
    fn widens_the_side_area_by_half_the_chat_for_a_new_column_and_gives_it_back_when_it_closes() {
        let paired = stack(&["review", "terminal"]);
        assert_eq!(layout(&paired).side_flex, 3.0);
        let widened = open_pane(&paired, "tasks");
        assert_eq!(layout(&widened).side_flex, 4.0);
        // The widened side area is split evenly, whatever width the first column had.
        assert_close(column_widths(layout(&widened))[0], 2.0);
        assert_close(column_widths(layout(&widened))[1], 2.0);
        let narrowed = apply(&widened, close_action("tasks"));
        assert_eq!(layout(&narrowed).columns, columns_of(&[&["review", "terminal"]]));
        assert_close(layout(&narrowed).side_flex, 3.0);
        assert_eq!(layout(&narrowed).column_flex, [1.0]);
        // However often a column comes and goes, the area doesn't creep narrower.
        let mut cycled = narrowed;
        for _ in 0..3 {
            cycled = apply(&open_pane(&cycled, "tasks"), close_action("tasks"));
        }
        assert_close(layout(&cycled).side_flex, 3.0);
    }

    #[test]
    fn gives_the_chat_back_no_more_than_a_closing_columns_own_width() {
        let squeezed = apply(&open_pane(&stack(&["review", "terminal"]), "tasks"), side_flex_action(1.2));
        let closed = apply(&squeezed, close_action("tasks"));
        // The remaining column keeps the 0.6 it was drawn at.
        assert_close(layout(&closed).side_flex, 0.6);
    }

    #[test]
    fn splits_the_side_area_evenly_whenever_a_column_opens_whatever_widths_the_columns_had() {
        // A terminal opens narrower than a document pane, and the second column still gets half.
        let narrow = open_pane(&stack(&["terminal", "review"]), "tasks");
        assert_eq!(layout(&narrow).columns, columns_of(&[&["terminal", "review"], &["tasks"]]));
        assert_eq!(layout(&narrow).column_flex, [1.0, 1.0]);
        // Widths dragged between two columns give way to thirds once a third column opens.
        let six = apply(&stack(&["terminal", "review", "tasks", "files", "plan", "settings"]), column_flex_action(&[3.0, 1.0]));
        assert_eq!(layout(&six).columns.len(), 2);
        let seven = open_pane(&six, "history:0");
        assert_eq!(layout(&seven).columns.len(), 3);
        assert_eq!(layout(&seven).column_flex, [1.0, 1.0, 1.0]);
        expect_well_formed(layout(&seven));
    }

    #[test]
    fn keeps_a_new_columns_share_when_the_side_area_is_already_at_its_widest() {
        let state = apply(&stack(&["review", "terminal"]), side_flex_action(MAX_SIDE_FLEX));
        let next = open_pane(&state, "tasks");
        assert_eq!(layout(&next).side_flex, MAX_SIDE_FLEX);
        expect_well_formed(layout(&next));
    }

    #[test]
    fn leaves_the_other_panes_where_they_are_when_one_closes_and_fills_the_gap_it_left() {
        let state = stack(&["terminal", "review", "tasks", "files"]);
        assert_eq!(layout(&state).columns, columns_of(&[&["terminal", "review"], &["tasks", "files"]]));
        let closed = apply(&state, close_action("terminal"));
        assert_eq!(layout(&closed).columns, columns_of(&[&["review"], &["tasks", "files"]]));
        // Two columns are all four panes get; the pane joins the shorter one.
        assert_eq!(layout(&open_pane(&closed, "plan")).columns, columns_of(&[&["review", "plan"], &["tasks", "files"]]));
    }

    #[test]
    fn drops_an_emptied_column_rather_than_keeping_an_empty_one() {
        let state = stack(&["terminal", "review", "tasks"]);
        let closed = apply(&state, close_action("tasks"));
        assert_eq!(layout(&closed).columns, columns_of(&[&["terminal", "review"]]));
        assert_eq!(layout(&closed).column_flex, [1.0]);
        assert_close(layout(&closed).side_flex, 1.0);
    }

    #[test]
    fn folds_the_third_column_back_into_two_once_six_panes_remain_moving_the_fewest() {
        let state = apply(
            &stack(&["settings", "plan", "preview:a", "terminal", "review", "files", "tasks", "history:0"]),
            pane_flex_action(&[("tasks", 2.0), ("history:0", 3.0), ("files", 4.0)]),
        );
        assert_eq!(layout(&state).columns, columns_of(&[&["settings", "plan", "files"], &["preview:a", "terminal", "review"], &["tasks", "history:0"]]));
        let seven = apply(&state, close_action("plan"));
        assert_eq!(layout(&seven).columns, columns_of(&[&["settings", "files"], &["preview:a", "terminal", "review"], &["tasks", "history:0"]]));
        let six = apply(&seven, close_action("settings"));
        // The column holding the fewest panes is dissolved and its pane joins the shortest column left; a moved pane's
        // height starts over among its new neighbours.
        assert_eq!(layout(&six).columns, columns_of(&[&["preview:a", "terminal", "review"], &["tasks", "history:0", "files"]]));
        assert_eq!(layout(&six).pane_flex, flexes(&[("tasks", 2.0), ("history:0", 3.0)]));
        assert!(layout(&six).side_flex < layout(&seven).side_flex);
        expect_well_formed(layout(&six));
        let last = apply(&stack(&SEVEN), close_action("settings"));
        assert_eq!(layout(&last).columns, columns_of(&[&["plan", "files", "tasks"], &["preview:a", "terminal", "review"]]));
    }

    #[test]
    fn closes_the_least_kept_pane_the_oldest_of_its_kind_to_open_a_tenth() {
        let full = stack_seven(&["history:1", "history:2"]);
        assert_eq!(layout(&full).panes.len(), MAX_SIDE_PANES);
        // The new pane is never the one given up, even when it is the least kept kind.
        let next = open_pane(&full, "history:3");
        assert_eq!(layout(&next).panes, with_seven(&["history:2", "history:3"]));
        assert_eq!(layout(&next).focused.as_deref(), Some("history:3"));
        expect_well_formed(layout(&next));
        let agent = open_agent(&next, "a");
        assert_eq!(layout(&agent).panes, with_seven(&["history:3", "subagent"]));
        expect_well_formed(layout(&agent));
    }

    #[test]
    fn ranks_an_agents_history_below_everything_the_list_names() {
        assert_eq!(PANE_KEEP_ORDER.map(SidePaneKind::as_str), ["settings", "plan", "preview", "terminal", "review", "files", "tasks", "subagent", "history"]);
        let full = stack_seven(&["history:0", "history:1"]);
        assert_eq!(pane_closed_for_room(layout(&full)), Some("history:0"));
        assert_eq!(layout(&open_agent(&full, "a")).panes, with_seven(&["history:1", "subagent"]));
        assert_eq!(pane_closed_for_room(layout(&stack(&["history:agent", "history:other"]))), Some("history:agent"));
        assert_eq!(pane_closed_for_room(layout(&initial())), None);
    }

    #[test]
    fn never_closes_one_of_the_first_eight_kinds_to_make_room() {
        let eight = with_seven(&["subagent"]);
        let mut state = open_agent(&stack(&SEVEN), "a");
        let more = ["history:1", "history:0", "history:2", "history:x", "history:3", "history:y"];
        for (index, pane) in more.iter().enumerate() {
            state = open_agent(&open_pane(&state, pane), &format!("agent-{index}"));
            for kept in &eight {
                assert!(pane_is_open(layout(&state), kept), "{kept}");
            }
            assert!(pane_is_open(layout(&state), pane));
            expect_well_formed(layout(&state));
        }
        // Every agent opened along the way is a tab of the one pane, not a pane of its own.
        let mut tabs = strs(&["a"]);
        tabs.extend((0..more.len()).map(|index| format!("agent-{index}")));
        assert_eq!(layout(&state).subagent_tabs, tabs);
    }

    #[test]
    fn does_not_close_anything_for_a_pane_that_is_already_open_or_a_page_that_replaces_another() {
        let full = open_agent(&stack_seven(&["history:1"]), "a");
        assert_eq!(layout(&open_agent(&full, "a")).panes, layout(&full).panes);
        // Another agent is another tab of the pane already open.
        assert_eq!(layout(&open_agent(&full, "b")).panes, layout(&full).panes);
        let replaced = open_pane(&full, "preview:b");
        assert_eq!(layout(&replaced).panes.len(), MAX_SIDE_PANES);
        assert!(layout(&replaced).columns.iter().flatten().any(|pane| pane == "preview:b"));
    }

    #[test]
    fn stays_well_formed_through_any_mix_of_opening_closing_and_replacing() {
        let pool = with_seven(&["preview:b", "subagent", "history:0", "history:1", "history:2"]);
        let agent_ids = ["a", "b", "c"];
        // Doubles, as upstream's generator is: the product overflows 2^53 and rounds, and integers would walk elsewhere.
        let mut seed = 7.0_f64;
        let mut next = move || {
            seed = (seed * 1103515245.0 + 12345.0) % 2147483648.0;
            seed / 2147483648.0
        };
        let mut state = initial();
        for _ in 0..2000 {
            let pane = &pool[(next() * pool.len() as f64).floor() as usize];
            let agent = agent_ids[(next() * agent_ids.len() as f64).floor() as usize];
            let roll = next();
            let before = layout(&state).clone();
            state = if roll < 0.4 {
                open_pane(&state, pane)
            } else if roll < 0.55 {
                open_agent(&state, agent)
            } else if roll < 0.75 {
                apply(&state, close_action(pane))
            } else if roll < 0.85 {
                apply(&state, close_subagent_action(agent))
            } else if roll < 0.9 {
                apply(&state, retarget_action(agent, agent_ids[(next() * agent_ids.len() as f64).floor() as usize]))
            } else {
                apply(&state, replace_action(pane, &pool[(next() * pool.len() as f64).floor() as usize]))
            };
            let after = layout(&state);
            expect_well_formed(after);
            if roll < 0.4 && before.panes.len() == MAX_SIDE_PANES {
                for kept in before.panes.iter().filter(|id| SEVEN.contains(&id.as_str()) || *id == "preview:b") {
                    if pane_kind(kept) == SidePaneKind::Preview && pane_kind(pane) == SidePaneKind::Preview {
                        continue;
                    }
                    assert!(pane_is_open(after, kept), "{kept}");
                }
            }
        }
    }

    // replacing a pane

    #[test]
    fn puts_the_replacement_in_the_panes_column_height_and_focus() {
        let state = apply(&apply(&stack(&["terminal", "history:call", "tasks"]), pane_flex_action(&[("history:call", 3.0)])), focus_action("history:call"));
        let next = apply(&state, replace_action("history:call", "history:name"));
        assert_eq!(
            *layout(&next),
            SidePaneLayout {
                panes: strs(&["terminal", "history:name", "tasks"]),
                columns: columns_of(&[&["terminal", "history:name"], &["tasks"]]),
                pane_flex: flexes(&[("history:name", 3.0)]),
                focused: Some("history:name".to_owned()),
                ..layout(&state).clone()
            }
        );
    }

    #[test]
    fn carries_an_expansion_over_and_leaves_an_unrelated_focus_alone() {
        let state = apply(&stack(&["history:call", "tasks"]), toggle_expand_action("history:call"));
        let focused_elsewhere = apply(&state, focus_action("tasks"));
        let next = apply(&focused_elsewhere, replace_action("history:call", "history:name"));
        assert_eq!(expanded_pane(layout(&next)), Some("history:name"));
        assert_eq!(layout(&next).focused.as_deref(), Some("tasks"));
    }

    #[test]
    fn only_closes_the_pane_when_its_replacement_is_already_open() {
        let state = stack(&["history:call", "history:name", "tasks"]);
        let next = apply(&state, replace_action("history:call", "history:name"));
        assert_eq!(layout(&next).panes, ["history:name", "tasks"]);
        expect_well_formed(layout(&next));
    }

    #[test]
    fn does_nothing_for_a_pane_that_is_not_open_or_a_replacement_that_is_itself() {
        let state = stack(&["tasks"]);
        assert_unchanged(&state, replace_action("history:a", "history:b"));
        assert_unchanged(&state, replace_action("tasks", "tasks"));
    }

    // subagent tabs

    #[test]
    fn opens_one_pane_for_every_agent_each_a_tab_of_it() {
        let first = open_agent(&stack(&["tasks"]), "a");
        assert_eq!(layout(&first).panes, ["tasks", "subagent"]);
        assert_eq!(layout(&first).focused.as_deref(), Some("subagent"));
        assert_eq!(shown_subagent(layout(&first)), Some("a"));
        let second = open_agent(&apply(&first, focus_action("tasks")), "b");
        assert_eq!(layout(&second).panes, ["tasks", "subagent"]);
        assert_eq!(layout(&second).subagent_tabs, ["a", "b"]);
        assert_eq!(shown_subagent(layout(&second)), Some("b"));
        // Asking for an agent is asking to see it, so the pane takes the focus back.
        assert_eq!(layout(&second).focused.as_deref(), Some("subagent"));
        expect_well_formed(layout(&second));
    }

    #[test]
    fn selects_an_agents_tab_where_it_is_rather_than_adding_another() {
        let state = agents(initial(), &["a", "b", "c"]);
        let back = open_agent(&state, "a");
        assert_eq!(layout(&back).subagent_tabs, ["a", "b", "c"]);
        assert_eq!(shown_subagent(layout(&back)), Some("a"));
        assert_unchanged(&back, SidePanesAction::OpenSubagent { conversation_id: cid(), subagent_id: "a".to_owned() });
    }

    #[test]
    fn shows_the_tab_to_the_right_of_a_closed_one_to_the_left_off_the_end_and_closes_the_pane_with_the_last() {
        let mut state = agents(stack(&["tasks"]), &["a", "b", "c"]);
        state = open_agent(&state, "b");
        state = apply(&state, close_subagent_action("b"));
        assert_eq!(layout(&state).subagent_tabs, ["a", "c"]);
        assert_eq!(shown_subagent(layout(&state)), Some("c"));
        state = apply(&state, close_subagent_action("c"));
        assert_eq!(shown_subagent(layout(&state)), Some("a"));
        expect_well_formed(layout(&state));
        state = apply(&state, close_subagent_action("a"));
        assert_eq!(layout(&state).panes, ["tasks"]);
        assert_eq!(shown_subagent(layout(&state)), None);
        expect_well_formed(layout(&state));
        assert_unchanged(&state, close_subagent_action("a"));
    }

    #[test]
    fn leaves_the_shown_tab_alone_when_another_one_closes() {
        let state = agents(initial(), &["a", "b", "c"]);
        let next = apply(&state, close_subagent_action("a"));
        assert_eq!(layout(&next).subagent_tabs, ["b", "c"]);
        assert_eq!(shown_subagent(layout(&next)), Some("c"));
    }

    #[test]
    fn drops_the_tabs_with_the_pane_so_opening_it_again_starts_from_the_agent_asked_for() {
        let state = agents(stack(&["tasks"]), &["a", "b"]);
        let closed = apply(&state, close_action("subagent"));
        assert!(layout(&closed).subagent_tabs.is_empty());
        assert_eq!(shown_subagent(layout(&closed)), None);
        expect_well_formed(layout(&closed));
        let reopened = open_agent(&closed, "c");
        assert_eq!(layout(&reopened).subagent_tabs, ["c"]);
        // The pane given up for room takes its tabs with it too.
        let full = open_agent(&stack_seven(&["history:0"]), "a");
        let room_made = open_pane(&full, "history:1");
        assert!(pane_is_open(layout(&room_made), "subagent"));
        let crowded = open_pane(&open_pane(&stack(&SEVEN), "history:1"), "history:2");
        expect_well_formed(layout(&open_agent(&crowded, "a")));
    }

    #[test]
    fn points_a_tab_at_the_id_its_agent_settled_into_in_place_and_merges_it_into_a_tab_that_id_already_has() {
        let state = agents(initial(), &["call-a", "b", "call-a"]);
        let moved = apply(&state, retarget_action("call-a", "a"));
        assert_eq!(layout(&moved).subagent_tabs, ["a", "b"]);
        assert_eq!(shown_subagent(layout(&moved)), Some("a"));
        assert_eq!(layout(&moved).panes, layout(&state).panes);
        let merged = apply(&open_agent(&moved, "call-b"), retarget_action("call-b", "b"));
        assert_eq!(layout(&merged).subagent_tabs, ["a", "b"]);
        assert_eq!(shown_subagent(layout(&merged)), Some("b"));
        expect_well_formed(layout(&merged));
        assert_unchanged(&merged, retarget_action("gone", "a"));
    }

    #[test]
    fn puts_the_tabs_in_the_order_they_were_dragged_into() {
        let state = agents(initial(), &["a", "b", "c"]);
        let next = apply(&state, reorder_subagents_action(&["c", "a", "b", "ghost"]));
        assert_eq!(layout(&next).subagent_tabs, ["c", "a", "b"]);
        assert_eq!(shown_subagent(layout(&next)), Some("c"));
        assert_unchanged(&next, reorder_subagents_action(&["c", "a", "b"]));
    }

    #[test]
    fn opens_no_subagent_pane_without_a_tab_and_never_swaps_one_with_another_pane() {
        let state = stack(&["tasks"]);
        assert_unchanged(&state, open_action("subagent"));
        let with_agent = open_agent(&state, "a");
        assert_unchanged(&with_agent, replace_action("subagent", "history:1"));
        assert_unchanged(&with_agent, replace_action("tasks", "subagent"));
    }

    #[test]
    fn carries_a_drafts_tabs_onto_the_conversation_it_becomes() {
        let open_draft_agent = |state: &SidePanesState, id: &str| apply(state, SidePanesAction::OpenSubagent { conversation_id: DRAFT.to_owned(), subagent_id: id.to_owned() });
        let draft = open_draft_agent(&open_draft_agent(&initial(), "a"), "b");
        let next = apply(&draft, adopt_action(DRAFT));
        assert_eq!(layout(&next).subagent_tabs, ["a", "b"]);
        assert_eq!(shown_subagent(layout(&next)), Some("b"));
    }

    // tasks pane tabs

    #[test]
    fn opens_the_one_pane_on_the_tab_asked_for_and_selects_the_other_tab_in_place() {
        let history = show_tab(&stack(&["terminal"]), TasksPaneTab::History);
        assert_eq!(layout(&history).panes, ["terminal", "tasks"]);
        assert_eq!(shown_tasks_tab(layout(&history)), Some(TasksPaneTab::History));
        assert_eq!(layout(&history).focused.as_deref(), Some("tasks"));
        let tasks = show_tab(&apply(&history, focus_action("terminal")), TasksPaneTab::Tasks);
        // The other tab is the same pane: nothing moves, and the pane comes forward.
        assert_eq!(layout(&tasks).panes, layout(&history).panes);
        assert_eq!(layout(&tasks).columns, layout(&history).columns);
        assert_eq!(shown_tasks_tab(layout(&tasks)), Some(TasksPaneTab::Tasks));
        assert_eq!(layout(&tasks).focused.as_deref(), Some("tasks"));
        assert_unchanged(&tasks, SidePanesAction::ShowTasksTab { conversation_id: cid(), tab: TasksPaneTab::Tasks });
    }

    #[test]
    fn shows_no_tab_while_the_pane_is_closed_and_reopens_on_the_tab_the_way_in_names() {
        assert_eq!(shown_tasks_tab(layout(&initial())), None);
        let closed = apply(&show_tab(&initial(), TasksPaneTab::History), close_action("tasks"));
        assert_eq!(shown_tasks_tab(layout(&closed)), None);
        assert_eq!(shown_tasks_tab(layout(&show_tab(&closed, TasksPaneTab::Tasks))), Some(TasksPaneTab::Tasks));
        assert_eq!(shown_tasks_tab(layout(&show_tab(&closed, TasksPaneTab::History))), Some(TasksPaneTab::History));
    }

    #[test]
    fn brings_an_expanded_panes_view_back_to_the_tasks_pane_when_another_pane_covers_it() {
        let expanded = apply(&stack(&["tasks", "terminal"]), toggle_expand_action("terminal"));
        let next = show_tab(&expanded, TasksPaneTab::History);
        assert_eq!(expanded_pane(layout(&next)), None);
        assert_eq!(shown_tasks_tab(layout(&next)), Some(TasksPaneTab::History));
    }

    // column widths

    #[test]
    fn clamps_each_weight_follows_the_columns_open_and_says_nothing_changed_for_the_same_weights() {
        let state = stack(&["terminal", "review", "tasks"]);
        let next = apply(&state, column_flex_action(&[20.0, -1.0, 5.0]));
        assert_eq!(layout(&next).column_flex, [MAX_SIDE_FLEX, MIN_SIDE_FLEX]);
        assert_unchanged(&next, column_flex_action(&[9.0, 0.0]));
        assert_eq!(layout(&apply(&next, column_flex_action(&[2.0]))).column_flex, [2.0, MIN_SIDE_FLEX]);
        assert_eq!(layout(&next).side_flex, layout(&state).side_flex);
    }

    #[test]
    fn remembers_a_kinds_width_only_while_the_side_area_is_one_column() {
        let paired = apply(&stack(&["review", "terminal"]), side_flex_action(5.0));
        assert_eq!(paired.last_side_flex_by_kind, BTreeMap::from([(SidePaneKind::Review, 5.0)]));
        let columns = apply(&open_pane(&paired, "tasks"), side_flex_action(7.0));
        assert_eq!(layout(&columns).side_flex, 7.0);
        assert_eq!(columns.last_side_flex_by_kind, BTreeMap::from([(SidePaneKind::Review, 5.0)]));
    }

    #[test]
    fn carries_a_drafts_columns_over_without_the_page_that_cannot_follow() {
        let mut state = initial();
        for pane in ["terminal", "preview:__draft__#x", "tasks"] {
            state = open_in(&state, pane, DRAFT);
        }
        assert_eq!(side_pane_layout_for(&state, Some(DRAFT)).columns, columns_of(&[&["terminal", "preview:__draft__#x"], &["tasks"]]));
        let adopted = apply(&state, adopt_action(DRAFT));
        assert_eq!(layout(&adopted).columns, columns_of(&[&["terminal"], &["tasks"]]));
        expect_well_formed(layout(&adopted));
    }

    // expanded pane

    #[test]
    fn toggles_a_pane_on_and_off_and_focuses_it_while_expanding() {
        let state = stack(&["terminal", "review"]);
        let expanded = apply(&state, toggle_expand_action("terminal"));
        assert_eq!(expanded_pane(layout(&expanded)), Some("terminal"));
        assert_eq!(layout(&expanded).focused.as_deref(), Some("terminal"));
        assert_eq!(expanded_pane(layout(&apply(&expanded, toggle_expand_action("terminal")))), None);
    }

    #[test]
    fn moves_the_expansion_straight_to_another_open_pane() {
        let state = apply(&stack(&["terminal", "review"]), toggle_expand_action("terminal"));
        assert_eq!(expanded_pane(layout(&apply(&state, toggle_expand_action("review")))), Some("review"));
    }

    #[test]
    fn ignores_a_pane_that_is_not_open() {
        assert_unchanged(&stack(&["terminal"]), toggle_expand_action("tasks"));
    }

    #[test]
    fn reports_no_expansion_once_the_expanded_pane_is_closed() {
        let state = apply(&stack(&["terminal", "review"]), toggle_expand_action("review"));
        let closed = apply(&state, close_action("review"));
        assert_eq!(closed.layout_by_conversation[CONVERSATION_ID].expanded.as_deref(), Some("review"));
        assert_eq!(expanded_pane(layout(&closed)), None);
    }

    #[test]
    fn drops_an_expansion_that_would_hide_the_pane_the_user_just_asked_for() {
        let state = apply(&stack(&["terminal", "review"]), toggle_expand_action("terminal"));
        assert_eq!(expanded_pane(layout(&open_pane(&state, "tasks"))), None);
        assert_eq!(expanded_pane(layout(&open_pane(&state, "review"))), None);
        assert_eq!(expanded_pane(layout(&open_pane(&state, "terminal"))), Some("terminal"));
    }

    #[test]
    fn keeps_a_maximized_preview_maximized_when_it_switches_to_or_adds_another_page() {
        let state = apply(&stack(&["preview:a", "review"]), toggle_expand_action("preview:a"));
        let switched = open_pane(&state, "preview:a#2");
        assert_eq!(expanded_pane(layout(&switched)), Some("preview:a#2"));
        assert_eq!(layout(&switched).focused.as_deref(), Some("preview:a#2"));
        assert_eq!(expanded_pane(layout(&open_pane(&switched, "preview:a"))), Some("preview:a"));
    }

    #[test]
    fn uncovers_a_page_opened_while_another_pane_is_maximized() {
        let state = apply(&stack(&["preview:a", "review"]), toggle_expand_action("review"));
        assert_eq!(expanded_pane(layout(&open_pane(&state, "preview:a#2"))), None);
    }

    #[test]
    fn survives_a_resize_and_does_not_reach_other_conversations() {
        let state = apply(&stack(&["review"]), toggle_expand_action("review"));
        let resized = apply(&state, side_flex_action(5.0));
        assert_eq!(expanded_pane(layout(&resized)), Some("review"));
        assert_eq!(side_pane_layout_for(&resized, Some("other")).expanded, None);
    }

    // side pane persistence

    fn storage(raw: Option<&str>) -> UiStore {
        let mut store = UiStore::default();
        if let Some(raw) = raw {
            store.set(STORAGE_KEY, raw);
        }
        store
    }

    #[test]
    fn round_trips_only_remembered_widths_through_the_exact_storage_key() {
        assert_eq!(STORAGE_KEY, "mewrk.sidePanes.sideFlexByKind");
        let mut store = storage(None);
        let state = apply(&stack(&["preview:a", "tasks"]), side_flex_action(4.5));
        persist_side_panes_state(&mut store, &state);
        assert_eq!(store.get(STORAGE_KEY), Some(r#"{"preview":4.5}"#));
        assert_eq!(load_side_panes_state(&store), SidePanesState { last_side_flex_by_kind: BTreeMap::from([(SidePaneKind::Preview, 4.5)]), ..initial() });
    }

    #[test]
    fn clamps_stored_values_and_ignores_unknown_kinds_and_nonnumbers() {
        let store = storage(Some(r#"{"review":100,"terminal":-2,"files":2.5,"preview":"3","tasks":null,"unknown":4}"#));
        assert_eq!(
            load_side_panes_state(&store).last_side_flex_by_kind,
            BTreeMap::from([(SidePaneKind::Review, 8.0), (SidePaneKind::Terminal, 0.25), (SidePaneKind::Files, 2.5)])
        );
    }

    #[test]
    fn sanitizes_remembered_widths_on_write_as_well() {
        let mut store = storage(None);
        let state = SidePanesState {
            last_side_flex_by_kind: BTreeMap::from([(SidePaneKind::Terminal, -3.0), (SidePaneKind::Files, 100.0), (SidePaneKind::Preview, f64::NAN)]),
            ..initial()
        };
        persist_side_panes_state(&mut store, &state);
        assert_eq!(store.get_json::<Value>(STORAGE_KEY).unwrap(), serde_json::json!({"terminal": 0.25, "files": 8.0}));
    }

    #[test]
    fn ignores_missing_or_malformed_storage() {
        for raw in [None, Some("{broken"), Some("null"), Some("[]"), Some("3"), Some("\"text\""), Some("true")] {
            assert_eq!(load_side_panes_state(&storage(raw)), initial(), "{raw:?}");
        }
    }

    #[test]
    fn works_when_localstorage_does_not_exist() {
        // A store with no file behind it, which is what a data folder that can't be found gets.
        let mut store = UiStore::default();
        assert_eq!(load_side_panes_state(&store), initial());
        persist_side_panes_state(&mut store, &initial());
        assert!(store.flush().is_ok());
    }

    // preview pages across workspaces

    #[test]
    fn files_each_page_under_the_workspace_it_was_opened_for_and_forgets_it_with_the_page() {
        let tab = format!("{CONVERSATION_ID}#tab_a");
        let mut state = apply(&initial(), register_action(CONVERSATION_ID, Some(2)));
        state = apply(&state, register_action(&tab, Some(3)));
        assert_eq!(roster(&state), [CONVERSATION_ID, tab.as_str()]);
        assert_eq!(preview_workspace_of(&state, CONVERSATION_ID), 2);
        assert_eq!(preview_workspace_of(&state, &tab), 3);
        // A page nobody placed is workspace 1's, and re-registering without a workspace keeps the one it has.
        assert_eq!(preview_workspace_of(&state, "unknown"), 1);
        assert_unchanged(&state, register_action(CONVERSATION_ID, None));

        state = apply(&state, forget_action(CONVERSATION_ID));
        assert_eq!(roster(&state), [tab.as_str()]);
        assert_eq!(preview_workspace_of(&state, CONVERSATION_ID), 1);
        assert_eq!(state.preview_workspaces.keys().collect::<Vec<_>>(), [&tab]);
    }

    #[test]
    fn moves_a_page_to_another_workspace_and_refuses_a_number_that_names_none() {
        let state = apply(&initial(), register_action(CONVERSATION_ID, Some(1)));
        let moved = apply(&state, set_workspace_action(CONVERSATION_ID, 2));
        assert_eq!(preview_workspace_of(&moved, CONVERSATION_ID), 2);
        assert_unchanged(&moved, set_workspace_action(CONVERSATION_ID, 2));
        assert_unchanged(&moved, set_workspace_action(CONVERSATION_ID, 0));
    }

    #[test]
    fn drops_a_removed_conversations_page_placements_with_its_roster() {
        let mut state = apply(&initial(), register_action(CONVERSATION_ID, Some(2)));
        state = apply(&state, SidePanesAction::RegisterPreview { conversation_id: "other".to_owned(), session_id: "other".to_owned(), workspace: Some(3) });
        state = apply(&state, remove_action());
        assert_eq!(state.preview_workspaces, BTreeMap::from([("other".to_owned(), 3)]));
    }

    #[test]
    fn puts_the_pages_in_the_order_their_tabs_were_dragged_into_leaving_the_layout_alone() {
        let second = format!("{CONVERSATION_ID}#tab_a");
        let third = format!("{CONVERSATION_ID}#tab_b");
        let state = stack(&[&preview_pane_id(CONVERSATION_ID), &preview_pane_id(&second), &preview_pane_id(&third)]);
        let moved = apply(&state, reorder_previews_action(&[&third, CONVERSATION_ID, &second]));
        assert_eq!(roster(&moved), [third.as_str(), CONVERSATION_ID, second.as_str()]);
        assert_eq!(moved.layout_by_conversation, state.layout_by_conversation);
        assert_eq!(moved.preview_workspaces, state.preview_workspaces);

        // A strip a render behind can name a page that just closed, or miss one that just opened.
        let stale = apply(&state, reorder_previews_action(&["gone", &third, &third, &second]));
        assert_eq!(roster(&stale), [third.as_str(), second.as_str(), CONVERSATION_ID]);
    }

    #[test]
    fn says_nothing_changed_for_the_order_a_roster_already_has_or_a_roster_that_does_not_exist() {
        let tab = format!("{CONVERSATION_ID}#tab_a");
        let state = stack(&[&preview_pane_id(CONVERSATION_ID), &preview_pane_id(&tab)]);
        assert_unchanged(&state, reorder_previews_action(&[CONVERSATION_ID, &tab]));
        assert_unchanged(&state, reorder_previews_action(&[CONVERSATION_ID]));
        assert_unchanged(&state, reorder_previews_action(&[]));
        assert_unchanged(&state, SidePanesAction::ReorderPreviews { conversation_id: "other".to_owned(), session_ids: strs(&[&tab, CONVERSATION_ID]) });
    }

    #[test]
    fn fills_the_conversations_own_page_first_and_opens_every_further_page_as_a_tab() {
        let tabs = strs(&["a", "b", "c"]);
        assert_eq!(preview_tab_after_closing(&tabs, "a", &strs(&["a"])).as_deref(), Some("b"));
        assert_eq!(preview_tab_after_closing(&tabs, "c", &strs(&["c"])).as_deref(), Some("b"), "back from the end");
        assert_eq!(preview_tab_after_closing(&tabs, "a", &strs(&["a", "b"])).as_deref(), Some("c"), "past another that is going too");
        assert_eq!(preview_tab_after_closing(&tabs, "b", &strs(&["a", "b", "c"])), None);
        assert_eq!(preview_tab_after_closing(&strs(&["a"]), "a", &strs(&["a"])), None);
        assert_eq!(preview_tab_after_closing(&strs(&["a"]), "blank", &strs(&["blank"])).as_deref(), Some("a"), "a start page has no place among the tabs");
        assert_eq!(new_preview_page_session_id(&[], "conv", "tab_x"), "conv");
        assert_eq!(new_preview_page_session_id(&strs(&["conv#tab_a"]), "conv", "tab_x"), "conv");
        assert_eq!(new_preview_page_session_id(&strs(&["conv"]), "conv", "tab_x"), "conv#tab_x");
    }

    // Not upstream's: `dispatchSidePanes` lived in App.tsx, untested.

    #[test]
    fn keeps_the_state_for_the_apps_lifetime_and_persists_only_when_remembered_widths_change() {
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("mewrk-side-panes-test-{}", std::process::id()));
        // Starting sets the process's language from the machine's; other tests in this process read it.
        let english = crate::i18n::is_english();
        let mut app = App::start(crate::StartOptions {
            identifier: "mewrk.test".to_owned(),
            version: "0.0.0".to_owned(),
            data_dir: dir.clone(),
            resource_dir: None,
            wake: Arc::new(|| {}),
            exit: Arc::new(|_| {}),
            show_window: Arc::new(|| {}),
        })
        .map_err(|error| format!("{error:?}"))
        .unwrap();
        crate::i18n::set_english(english);

        // The first read loads the store; later ones don't go back to it.
        app.ui_store().set(STORAGE_KEY, r#"{"review":5}"#);
        assert_eq!(app.side_panes().last_side_flex_by_kind, BTreeMap::from([(SidePaneKind::Review, 5.0)]));
        app.ui_store().set(STORAGE_KEY, "sentinel");
        assert_eq!(app.side_panes().last_side_flex_by_kind, BTreeMap::from([(SidePaneKind::Review, 5.0)]));

        // Opening, focusing and the like leave the store alone, and each dispatch sees the last.
        app.dispatch_side_panes(open_action("review"));
        app.dispatch_side_panes(open_action("terminal"));
        assert_eq!(layout(app.side_panes()).panes, ["review", "terminal"]);
        assert_eq!(layout(app.side_panes()).side_flex, 5.0);
        assert_eq!(app.ui_store().get(STORAGE_KEY), Some("sentinel"));

        // Remembering a new width stores it.
        app.dispatch_side_panes(side_flex_action(6.0));
        assert_eq!(app.ui_store().get(STORAGE_KEY), Some(r#"{"review":6.0}"#));

        // A resize that remembers nothing doesn't: the area is two columns now.
        app.ui_store().set(STORAGE_KEY, "sentinel");
        app.dispatch_side_panes(open_action("tasks"));
        app.dispatch_side_panes(side_flex_action(7.5));
        assert_eq!(layout(app.side_panes()).side_flex, 7.5);
        assert_eq!(app.ui_store().get(STORAGE_KEY), Some("sentinel"));

        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }
}
