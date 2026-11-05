//! Where the engine's events wait for the next frame.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use crate::model::{Item, ItemId, ToolMeta, ToolStatus, Usage};

/// Wakes the UI at most once per frame, however many events arrive.
pub struct Notifier {
    pending: AtomicBool,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Notifier {
    pub fn new(wake: Box<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { pending: AtomicBool::new(false), wake })
    }

    pub fn none() -> Arc<Self> {
        Self::new(Box::new(|| {}))
    }

    pub fn poke(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }

    /// Call when a frame starts. Whatever arrives after this wakes the UI again.
    pub fn frame_started(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RunStatus {
    Connecting,
    Streaming,
    RunningTools,
    Retrying { attempt: u32, delay_ms: u64, detail: String },
    Compacting,
    WaitingForUser,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEnd {
    Completed,
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalRequest {
    pub request: u64,
    pub item: ItemId,
    pub tool: String,
    pub args: Value,
    /// What the tool would do, in one line.
    pub summary: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    /// Allow this tool for the rest of the conversation.
    AllowAlways,
    Deny,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuestionRequest {
    pub request: u64,
    pub item: ItemId,
    pub questions: Value,
}

#[derive(Clone, Debug)]
pub enum RunEvent {
    Status(RunStatus),
    Push(Item),
    AppendText { id: ItemId, delta: String },
    AppendArgs { id: ItemId, delta: String },
    Tool { id: ItemId, status: ToolStatus, output: Option<String>, took_ms: u64, meta: Option<ToolMeta> },
    /// Tail items produced since a failed attempt began are gone: drop every item with an id at or above this.
    Rollback { from: ItemId },
    Usage(Usage),
    Approval(ApprovalRequest),
    Question(QuestionRequest),
    /// A subagent started. Its transcript has an inbox of its own.
    Child(Arc<crate::engine::RunHandle>),
    Finished(RunEnd),
}

#[derive(Default)]
pub struct Inbox {
    q: Mutex<VecDeque<RunEvent>>,
}

impl Inbox {
    pub fn push(&self, ev: RunEvent) {
        let mut q = self.q.lock();
        // Merging is the whole point: a late frame finds one chunk of text, not thousands of events.
        match (&ev, q.back_mut()) {
            (RunEvent::AppendText { id, delta }, Some(RunEvent::AppendText { id: bid, delta: bd })) if id == bid => bd.push_str(delta),
            (RunEvent::AppendArgs { id, delta }, Some(RunEvent::AppendArgs { id: bid, delta: bd })) if id == bid => bd.push_str(delta),
            _ => q.push_back(ev),
        }
    }

    pub fn drain(&self, out: &mut Vec<RunEvent>) {
        out.extend(self.q.lock().drain(..));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_text_merges_and_other_events_keep_their_order() {
        let inbox = Inbox::default();
        inbox.push(RunEvent::AppendText { id: 1, delta: "a".into() });
        inbox.push(RunEvent::AppendText { id: 1, delta: "b".into() });
        inbox.push(RunEvent::AppendText { id: 2, delta: "c".into() });
        inbox.push(RunEvent::Status(RunStatus::Streaming));
        inbox.push(RunEvent::AppendText { id: 2, delta: "d".into() });
        let mut out = Vec::new();
        inbox.drain(&mut out);
        assert_eq!(out.len(), 4);
        assert!(matches!(&out[0], RunEvent::AppendText { delta, .. } if delta == "ab"));
        assert!(matches!(&out[3], RunEvent::AppendText { delta, .. } if delta == "d"));
    }
}
