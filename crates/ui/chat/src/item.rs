//! A context as the timeline reads it: the stored card, and what is live about it while a run streams.
//!
//! Upstream kept `streaming`, `streamStatus`, `live` and `startedAt` as optional fields on the context itself; here the
//! persisted `ContextItem` stays clean and these travel beside it.

use std::sync::Arc;

use mewrk_client::model::{ContextItem, HookContextMetadata, JsonObject, NativeCompaction, ReasoningForm, ReasoningReplay, SubagentRunRecord, ToolResult};
use mewrk_client::runs::{LiveContext, RenderedContext, StreamingToolPhase, SubagentLiveState};

#[derive(Clone, Copy, Debug)]
pub struct Item<'a> {
    pub item: &'a ContextItem,
    /// `streaming === true`.
    pub streaming: bool,
    pub stream_status: Option<StreamingToolPhase>,
    pub live: Option<&'a Arc<SubagentLiveState>>,
    pub started_at: Option<&'a str>,
}

impl<'a> Item<'a> {
    pub fn settled(item: &'a ContextItem) -> Self {
        Self { item, streaming: false, stream_status: None, live: None, started_at: None }
    }

    pub fn live(context: &'a LiveContext) -> Self {
        Self { item: &context.item, streaming: context.is_streaming(), stream_status: context.stream_status, live: context.live.as_ref(), started_at: context.started_at.as_deref() }
    }

    pub fn id(&self) -> &'a str {
        self.item.id()
    }

    /// A tool card's fields; `None` for any other kind.
    pub fn tool(&self) -> Option<ToolItem<'a>> {
        match self.item {
            ContextItem::Tool { id, tool_name, input, result, subagent, notice, created_at, attestation, provider_call_id, .. } => Some(ToolItem {
                id,
                tool_name,
                input,
                result,
                subagent: subagent.as_ref(),
                notice: notice.as_deref(),
                created_at,
                attestation,
                provider_call_id: provider_call_id.as_deref(),
                streaming: self.streaming,
                stream_status: self.stream_status,
                live: self.live,
            }),
            _ => None,
        }
    }

    pub fn reasoning(&self) -> Option<ReasoningItem<'a>> {
        match self.item {
            ContextItem::Reasoning { id, content, form, interrupted, duration_ms, tokens, replay, .. } => Some(ReasoningItem {
                id,
                content: content.as_deref().unwrap_or(""),
                form: *form,
                interrupted: *interrupted,
                duration_ms: *duration_ms,
                tokens: *tokens,
                replay: replay.as_ref(),
                streaming: self.streaming,
                started_at: self.started_at,
            }),
            _ => None,
        }
    }

    pub fn system(&self) -> Option<SystemItem<'a>> {
        match self.item {
            ContextItem::System { id, content, local_only, hook_execution, tools_added, native_compaction, .. } => {
                Some(SystemItem { id, content, local_only: *local_only, hook_execution: hook_execution.as_ref(), tools_added, native_compaction: native_compaction.as_deref() })
            }
            _ => None,
        }
    }
}

impl<'a> From<RenderedContext<'a>> for Item<'a> {
    fn from(context: RenderedContext<'a>) -> Self {
        match context {
            RenderedContext::Persisted(item) => Self::settled(item),
            RenderedContext::Live(context) => Self::live(context),
        }
    }
}

/// The fields of a tool card, borrowed: `ContextItem::Tool` has too many to match on everywhere.
#[derive(Clone, Copy, Debug)]
pub struct ToolItem<'a> {
    pub id: &'a str,
    pub tool_name: &'a str,
    pub input: &'a JsonObject,
    pub result: &'a ToolResult,
    pub subagent: Option<&'a SubagentRunRecord>,
    pub notice: Option<&'a str>,
    pub created_at: &'a str,
    pub attestation: &'a str,
    pub provider_call_id: Option<&'a str>,
    pub streaming: bool,
    pub stream_status: Option<StreamingToolPhase>,
    pub live: Option<&'a Arc<SubagentLiveState>>,
}

impl<'a> ToolItem<'a> {
    /// What a live explanation of this call may be filed under.
    pub fn provider_call(&self) -> Option<String> {
        self.provider_call_id.map(str::to_owned)
    }

    /// `isLiveCall`: a streamed call that has not reached its terminal status. Never true of a reloaded card.
    pub fn is_live_call(&self) -> bool {
        self.streaming && self.stream_status != Some(StreamingToolPhase::Completed)
    }

    /// The tasks pane's view of the same card, for its `subagents` helpers. `live` is a converted copy (`LiveState::from`).
    pub fn task_ref(&self, live: Option<&'a mewrk_pane_tasks::context::LiveState>) -> mewrk_pane_tasks::context::ToolRef<'a> {
        mewrk_pane_tasks::context::ToolRef {
            id: self.id,
            tool_name: self.tool_name,
            input: self.input,
            result: self.result,
            subagent: self.subagent,
            created_at: self.created_at,
            streaming: self.streaming,
            stream_status: self.stream_status.map(stream_status_name),
            live,
        }
    }
}

pub fn stream_status_name(status: StreamingToolPhase) -> &'static str {
    match status {
        StreamingToolPhase::Announced => "announced",
        StreamingToolPhase::Ready => "ready",
        StreamingToolPhase::Running => "running",
        StreamingToolPhase::Completed => "completed",
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ReasoningItem<'a> {
    pub id: &'a str,
    pub content: &'a str,
    pub form: Option<ReasoningForm>,
    pub interrupted: bool,
    pub duration_ms: Option<u64>,
    pub tokens: Option<u64>,
    pub replay: Option<&'a ReasoningReplay>,
    pub streaming: bool,
    pub started_at: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
pub struct SystemItem<'a> {
    pub id: &'a str,
    pub content: &'a str,
    pub local_only: bool,
    pub hook_execution: Option<&'a HookContextMetadata>,
    pub tools_added: &'a [String],
    pub native_compaction: Option<&'a NativeCompaction>,
}
