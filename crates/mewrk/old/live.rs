//! A conversation in memory: its timeline, the run feeding it, and the panes that belong to it.

use std::sync::Arc;

use mewrk_core::tasks::Session;
use mewrk_core::*;
use mewrk_panes::{FilesPane, ReviewPane};
use mewrk_term::Terminal;

use crate::composer::Composer;
use crate::timeline::Timeline;

pub struct TermTab {
    pub term: Terminal,
    pub id: u64,
}

/// A subagent's transcript, kept live while the parent runs.
pub struct Child {
    pub handle: Arc<RunHandle>,
    pub items: Vec<Item>,
    pub view: Timeline,
    pub status: Option<RunStatus>,
    pub end: Option<RunEnd>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    NotLoaded,
    Loading,
    Loaded,
}

pub struct LiveConv {
    pub items: Vec<Item>,
    pub load: LoadState,
    pub view: Timeline,
    pub composer: Composer,
    pub handle: Option<Arc<RunHandle>>,
    pub status: Option<RunStatus>,
    pub session: Arc<Session>,
    pub approvals: Vec<ApprovalRequest>,
    pub questions: Vec<QuestionRequest>,
    pub children: Vec<Child>,
    pub shown_child: Option<usize>,
    pub unseen_done: bool,
    pub error: Option<String>,
    pub queue: Vec<String>,
    pub terminals: Vec<TermTab>,
    pub active_term: usize,
    pub next_term_id: u64,
    pub review: Option<ReviewPane>,
    pub files: Option<FilesPane>,
    pub preview: Option<crate::preview::Preview>,
    pub context_tokens: u64,
    /// Files changed since the review pane last looked.
    pub files_touched: bool,
}

impl LiveConv {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            load: LoadState::NotLoaded,
            view: Timeline::new(),
            composer: Composer::new(),
            handle: None,
            status: None,
            session: Session::new(),
            approvals: Vec::new(),
            questions: Vec::new(),
            children: Vec::new(),
            shown_child: None,
            unseen_done: false,
            error: None,
            queue: Vec::new(),
            terminals: Vec::new(),
            active_term: 0,
            next_term_id: 0,
            review: None,
            files: None,
            preview: None,
            context_tokens: 0,
            files_touched: false,
        }
    }

    pub fn is_running(&self) -> bool {
        self.handle.is_some()
    }

    pub fn is_blocked(&self) -> bool {
        !self.approvals.is_empty() || !self.questions.is_empty()
    }
}

/// Applies a run's events to a timeline. The same for a conversation and for a subagent's transcript.
pub fn apply_event(items: &mut Vec<Item>, view: &mut Timeline, ev: &RunEvent) {
    match ev {
        RunEvent::Push(item) => {
            items.push(item.clone());
            view.structure_changed();
        }
        RunEvent::AppendText { id, delta } => {
            if let Some(Item::Text { text, tokens, .. } | Item::Thinking { text, tokens, .. }) = items.iter_mut().rev().find(|i| i.id() == *id) {
                text.push_str(delta);
                *tokens = estimate_tokens(text);
            }
        }
        RunEvent::AppendArgs { id, delta } => {
            if let Some(Item::Tool(t)) = items.iter_mut().rev().find(|i| i.id() == *id) {
                t.args.push_str(delta);
                t.tokens = estimate_tokens(&t.args);
            }
        }
        RunEvent::Tool { id, status, output, took_ms, meta } => {
            if let Some(Item::Tool(t)) = items.iter_mut().rev().find(|i| i.id() == *id) {
                t.status = *status;
                if let Some(o) = output {
                    t.output.clone_from(o);
                }
                if *took_ms > 0 {
                    t.took_ms = *took_ms;
                }
                if let Some(m) = meta {
                    t.meta = m.clone();
                }
                view.tool_changed(*id);
                view.structure_changed();
            }
        }
        RunEvent::Rollback { from } => {
            items.retain(|i| i.id() < *from);
            view.rolled_back(*from);
            view.structure_changed();
        }
        _ => {}
    }
}
