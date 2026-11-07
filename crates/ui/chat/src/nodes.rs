//! `buildContextRenderNodes` and the turn projection of `ContextStream`: how a flat list of contexts becomes what is
//! drawn. No elements here: this runs when a timeline changes, not per frame.

use std::collections::{HashMap, HashSet};

use mewrk_client::model::ContextItem;
use mewrk_client::model_capabilities::is_encrypted_reasoning;
use mewrk_client::runs::{ConversationTurn, ConversationTurnStatus};

use crate::item::{Item, ReasoningItem};
use crate::orchestration::{is_host_authored_user_context, is_legacy_paused_question};
use crate::tool_view::{is_hoisted_tool_call, tool_surface_for_name, ToolSurface};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Tool,
    /// A tool call whose body is a live run rather than a result.
    Workflow,
    Reasoning,
    Hook,
}

/// One row of a block, by its index in the rendered contexts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockEntry {
    pub kind: EntryKind,
    pub index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderNode {
    Context { index: usize },
    Block { key: String, entries: Vec<BlockEntry> },
    Question { key: String, entry: usize, answer: Option<usize> },
}

impl RenderNode {
    /// `renderNodeKey`: a context's own id, or the node's key.
    pub fn key<'a>(&'a self, contexts: &impl Contexts<'a>) -> std::borrow::Cow<'a, str> {
        match self {
            Self::Context { index } => std::borrow::Cow::Borrowed(contexts.at(*index).id()),
            Self::Block { key, .. } | Self::Question { key, .. } => std::borrow::Cow::Borrowed(key),
        }
    }

    /// The context indexes this node draws, in the order they sit.
    pub fn indexes(&self) -> Vec<usize> {
        match self {
            Self::Context { index } => vec![*index],
            Self::Block { entries, .. } => entries.iter().map(|entry| entry.index).collect(),
            Self::Question { entry, answer, .. } => std::iter::once(*entry).chain(*answer).collect(),
        }
    }

    /// The lowest of [`Self::indexes`], without collecting them: an incremental rebuild searches the nodes by it.
    pub fn first_index(&self) -> usize {
        match self {
            Self::Context { index } => *index,
            Self::Block { entries, .. } => entries.iter().map(|entry| entry.index).min().unwrap_or(0),
            Self::Question { entry, answer, .. } => answer.map_or(*entry, |answer| answer.min(*entry)),
        }
    }

    pub fn after_index(&self) -> usize {
        self.indexes().into_iter().max().unwrap_or(0) + 1
    }
}

/// What the projection reads contexts through, so a live timeline needs no copy of the persisted one.
pub trait Contexts<'a> {
    fn count(&self) -> usize;
    fn at(&self, index: usize) -> Item<'a>;
}

impl<'a> Contexts<'a> for &[Item<'a>] {
    fn count(&self) -> usize {
        self.len()
    }
    fn at(&self, index: usize) -> Item<'a> {
        self[index]
    }
}

/// `isProtocolOnlyContext`: empty model output is a wire-protocol anchor, not a visible timeline boundary. Visibility is
/// derived from the item itself, since editing can invalidate its provenance. An empty reasoning context carrying a
/// duration or a token count is the exception: that is the entire visible trace of a round billed for encrypted thinking.
fn is_protocol_only(item: &Item<'_>) -> bool {
    match item.item {
        ContextItem::Assistant { content, .. } => content.is_empty(),
        ContextItem::System { tools_added, .. } => !tools_added.is_empty(),
        ContextItem::Reasoning { content, duration_ms, tokens, .. } => !item.streaming && content.as_deref().unwrap_or("").is_empty() && duration_ms.is_none() && tokens.is_none(),
        _ => false,
    }
}

fn is_live_encrypted_reasoning(item: &ReasoningItem<'_>) -> bool {
    item.streaming && item.content.is_empty() && is_encrypted_reasoning(item.form, Some(item.content))
}

/// `isSettledWorkflowCall`.
fn is_settled_workflow_call(item: &crate::item::ToolItem<'_>) -> bool {
    use mewrk_client::runs::StreamingToolPhase::Completed;
    item.stream_status == Some(Completed) || (!item.streaming && item.stream_status.is_none())
}

/// Where a legacy question's search for its answer stops: at the next `ask_user` call, or at the next message the user wrote.
/// Everything else is passed over.
pub fn ends_answer_search(item: &Item<'_>) -> bool {
    match item.item {
        ContextItem::Tool { tool_name, .. } => tool_name == "ask_user",
        ContextItem::User { .. } => !is_host_authored_user_context(item.item),
        ContextItem::System { .. } | ContextItem::Reasoning { .. } | ContextItem::Assistant { .. } => false,
    }
}

/// Groups the timeline into blocks, questions and plain message cards. A block runs for as long as nothing the user wrote
/// or the model said interrupts it. `ask_user` is the one tool that leaves the block: its card carries the user's own answer.
pub fn build_context_render_nodes<'a>(contexts: impl Contexts<'a>, break_after_indexes: &HashSet<usize>, has_workflow_view: impl Fn(&str) -> bool) -> Vec<RenderNode> {
    build_context_render_nodes_from(contexts, break_after_indexes, has_workflow_view, 0, None)
}

/// [`build_context_render_nodes`] from context `start` on: the nodes the whole pass goes on to draw once it has reached `start`
/// with no block open, which is what an incremental rebuild appends to the nodes it keeps. `held_answer` is an answer at or
/// after `start` that a question before it folds into its card, so it is not drawn here either.
pub fn build_context_render_nodes_from<'a>(contexts: impl Contexts<'a>, break_after_indexes: &HashSet<usize>, has_workflow_view: impl Fn(&str) -> bool, start: usize, held_answer: Option<usize>) -> Vec<RenderNode> {
    let count = contexts.count();
    let mut answers: HashMap<usize, usize> = HashMap::new();
    let mut projected: HashSet<usize> = held_answer.into_iter().collect();
    for index in start..count {
        let item = contexts.at(index);
        // Only a question from before questions blocked has its answer in the next user message.
        if !is_legacy_paused_question(&item) {
            continue;
        }
        for answer_index in index + 1..count {
            let candidate = contexts.at(answer_index);
            if !ends_answer_search(&candidate) {
                continue;
            }
            // Another question ends the search unanswered; the user's next message answers it.
            if matches!(candidate.item, ContextItem::User { .. }) && !break_after_indexes.contains(&answer_index) && !projected.contains(&answer_index) {
                answers.insert(index, answer_index);
                projected.insert(answer_index);
            }
            break;
        }
    }

    let mut nodes = Vec::new();
    let mut block: Vec<BlockEntry> = Vec::new();
    let flush = |block: &mut Vec<BlockEntry>, nodes: &mut Vec<RenderNode>| {
        if let Some(first) = block.first() {
            let key = format!("block:{}", contexts.at(first.index).id());
            nodes.push(RenderNode::Block { key, entries: std::mem::take(block) });
        }
    };
    for index in start..count {
        let item = contexts.at(index);
        if break_after_indexes.contains(&index) {
            flush(&mut block, &mut nodes);
        }
        if is_protocol_only(&item) || projected.contains(&index) {
            continue;
        }
        if let Some(tool) = item.tool() {
            // A call in flight is narrated by the end-of-stream indicator as one line; a row for it would be a name and a
            // spinner, replaced the moment the receipt lands.
            if is_hoisted_tool_call(&tool) {
                continue;
            }
            match tool_surface_for_name(tool.tool_name) {
                ToolSurface::Question => {
                    flush(&mut block, &mut nodes);
                    nodes.push(RenderNode::Question { key: format!("question:{}", tool.id), entry: index, answer: answers.get(&index).copied() });
                }
                ToolSurface::Workflow => {
                    // An unfinished run with no view would claim a zero-step workflow.
                    if has_workflow_view(tool.id) || is_settled_workflow_call(&tool) {
                        block.push(BlockEntry { kind: EntryKind::Workflow, index });
                    }
                }
                ToolSurface::Group => block.push(BlockEntry { kind: EntryKind::Tool, index }),
            }
            continue;
        }
        if let Some(reasoning) = item.reasoning() {
            if !is_live_encrypted_reasoning(&reasoning) {
                block.push(BlockEntry { kind: EntryKind::Reasoning, index });
            }
            continue;
        }
        if item.system().is_some_and(|system| system.hook_execution.is_some()) {
            block.push(BlockEntry { kind: EntryKind::Hook, index });
            continue;
        }
        flush(&mut block, &mut nodes);
        nodes.push(RenderNode::Context { index });
    }
    flush(&mut block, &mut nodes);
    nodes
}

/// `renderedContextIndexes`: what the insertion line can sit against, ascending.
pub fn rendered_context_indexes(nodes: &[RenderNode]) -> Vec<usize> {
    let mut indexes: Vec<usize> = nodes.iter().flat_map(RenderNode::indexes).collect();
    indexes.sort_unstable();
    indexes
}

/// The drawn row an insert index lands on: the first drawn context at or after it, else the end.
pub fn display_insertion_index(rendered: &[usize], insertion: usize, count: usize) -> usize {
    match rendered.binary_search(&insertion) {
        Ok(at) => rendered[at],
        Err(at) => rendered.get(at).copied().unwrap_or(count),
    }
}

/// What a round still draws for itself, by node: the sign that it is running, and the notice saying why it stopped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnProjection {
    /// Turn positions in `turns`, per node position.
    pub notices_after_node: HashMap<usize, Vec<usize>>,
    pub leading_turns: Vec<usize>,
    /// Whether a running turn reaches the screen; if not, the end of the stream hosts the indicator.
    pub any_visible_running: bool,
}

/// The turn projection of `ContextStream`. `index_of` maps a context id to its index, `node_of` a context index to the
/// node drawing it.
pub fn project_turns<'a>(turns: &[ConversationTurn], index_of: impl Fn(&str) -> Option<usize>, node_of: impl Fn(usize) -> Option<usize>, _contexts: impl Contexts<'a>) -> TurnProjection {
    let mut out = TurnProjection::default();
    // Where a turn sits in the stream. A turn outlives the deletion of its own anchor: fall back to just before the first
    // message it still owns, so a round that lost its user message still shows its notice among its own replies.
    let stream_position = |turn: &ConversationTurn| -> Option<isize> {
        if let Some(index) = anchor_index(&turn.anchor_context_id, &index_of) {
            return Some(index);
        }
        turn.context_ids.iter().filter_map(|id| index_of(id)).min().map(|index| index as isize - 1)
    };
    let mut sorted: Vec<(usize, isize)> = turns.iter().enumerate().filter_map(|(at, turn)| stream_position(turn).map(|position| (at, position))).collect();
    sorted.sort_by_key(|(_, position)| *position);
    for (at, position) in sorted {
        let turn = &turns[at];
        let running = turn.status == ConversationTurnStatus::Running;
        if !running && turn.error.is_none() {
            continue;
        }
        let last_owned = turn.context_ids.iter().filter_map(|id| index_of(id)).filter_map(&node_of).max();
        if let Some(node) = last_owned {
            out.notices_after_node.entry(node).or_default().push(at);
        } else {
            // The context at that position may not be drawn at all, so walk back to the nearest one that is.
            let mut anchor = None;
            let mut index = position;
            while index >= 0 {
                if let Some(node) = node_of(index as usize) {
                    anchor = Some(node);
                    break;
                }
                index -= 1;
            }
            match anchor {
                Some(node) => out.notices_after_node.entry(node).or_default().push(at),
                // It opens the conversation: lead the stream rather than drop the only surface a live run has.
                None => out.leading_turns.push(at),
            }
        }
        if running {
            out.any_visible_running = true;
        }
    }
    out
}

/// `turnAnchorIndex`: an anchor may sit before everything, named "" or absent.
fn anchor_index(anchor: &str, index_of: &impl Fn(&str) -> Option<usize>) -> Option<isize> {
    if anchor.is_empty() {
        return Some(-1);
    }
    index_of(anchor).map(|index| index as isize)
}

/// `contextVisualLength`: whether the tail grew, for follow-output.
pub fn context_visual_length(item: Option<Item<'_>>) -> usize {
    let Some(item) = item else { return 0 };
    match item.item {
        ContextItem::Tool { result, .. } => result.output.len() + result.diff.as_ref().map_or(0, String::len) + result.images.len() * 200,
        ContextItem::User { content, images, files, .. } => content.len() + (images.len() + files.len()) * 200,
        ContextItem::Assistant { content, .. } | ContextItem::System { content, .. } => content.len(),
        ContextItem::Reasoning { content, .. } => content.as_ref().map_or(0, String::len),
    }
}

/// `firstProseLine`: the one line a collapsed system-prompt row shows.
pub fn first_prose_line(content: &str) -> Option<&str> {
    content.split('\n').map(str::trim).find(|line| !line.is_empty())
}

/// How much of a reasoning line reaches the screen.
const REASONING_LINE_LIMIT: usize = 220;

/// `reasoningLine`: while the round is still thinking, the newest line, so the row reads as the thought moving forward;
/// once it settles, the first. Markers are stripped because a row is plain text.
pub fn reasoning_line(content: &str, streaming: bool) -> Option<String> {
    let mut lines = content.split('\n').map(str::trim).filter(|line| !line.is_empty());
    let line = if streaming { lines.next_back() } else { lines.next() }?;
    let line = strip_block_marker(line);
    let stripped: String = line.chars().filter(|c| !matches!(c, '*' | '_' | '`')).collect();
    let stripped = stripped.trim();
    if stripped.is_empty() {
        return None;
    }
    let count = stripped.encode_utf16().count();
    if count > REASONING_LINE_LIMIT {
        let mut taken = String::new();
        let mut units = 0;
        for c in stripped.chars() {
            units += c.len_utf16();
            if units > REASONING_LINE_LIMIT - 1 {
                break;
            }
            taken.push(c);
        }
        return Some(format!("{}…", taken.trim_end()));
    }
    Some(stripped.to_owned())
}

/// `/^(#{1,6}|[-*+]|>|\d+\.)\s+/`.
fn strip_block_marker(line: &str) -> &str {
    let bytes = line.as_bytes();
    let marker = match bytes.first() {
        Some(b'#') => {
            let hashes = bytes.iter().take_while(|b| **b == b'#').count();
            if hashes <= 6 { hashes } else { 0 }
        }
        Some(b'-' | b'*' | b'+' | b'>') => 1,
        Some(b'0'..=b'9') => {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if bytes.get(digits) == Some(&b'.') { digits + 1 } else { 0 }
        }
        _ => 0,
    };
    if marker == 0 {
        return line;
    }
    let rest = &line[marker..];
    let trimmed = rest.trim_start();
    if trimmed.len() < rest.len() { trimmed } else { line }
}

/// `sourceHostname`.
pub fn source_hostname(url: Option<&str>) -> Option<String> {
    let parsed = url::Url::parse(url?).ok()?;
    parsed.host_str().filter(|host| !host.is_empty()).map(str::to_owned)
}

/// `safeSourceHref`: upstream search URLs are untrusted, so only http and https become links.
pub fn safe_source_href(url: Option<&str>) -> Option<String> {
    let raw = url?;
    let parsed = url::Url::parse(raw).ok()?;
    matches!(parsed.scheme(), "http" | "https").then(|| raw.to_owned())
}
