//! A context item as the renderer holds it: the persisted `model::ContextItem` plus what exists only while a run is
//! streaming. The host's type has no room for those fields, since they are never saved.

use std::collections::BTreeMap;

use mewrk_client::model::{ContextItem, JsonObject, ModelUsage, SubagentRunRecord, SubagentRunStatus, SubagentUpdate, ToolResult};
use serde::{Deserialize, Serialize};

fn is_false(v: &bool) -> bool {
    !*v
}

/// Upstream's `SubagentLiveStatus`: a persisted status name, or one of the two states only a stream has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LiveStatus {
    Running,
    Idle,
    Interrupted,
    Failed,
    Stopped,
    RoundLimit,
    Completed,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveState {
    #[serde(default)]
    pub contexts: Vec<Ctx>,
    #[serde(default)]
    pub updates: Vec<SubagentUpdate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<LiveStatus>,
    /// Keyed by round because each snapshot supersedes that round's previous one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_by_round: Option<BTreeMap<u64, ModelUsage>>,
}

/// What this crate derives views from. The JSON shape is upstream's `ContextItem`, which is what its test fixtures spell
/// out; `mewrk_client::runs::LiveContext` converts into it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ctx {
    #[serde(flatten)]
    pub item: ContextItem,
    #[serde(default, skip_serializing_if = "is_false")]
    pub streaming: bool,
    #[serde(rename = "streamStatus", default, skip_serializing_if = "Option::is_none")]
    pub stream_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveState>,
}

impl From<&mewrk_client::runs::SubagentLiveState> for LiveState {
    fn from(live: &mewrk_client::runs::SubagentLiveState) -> Self {
        use mewrk_client::runs::SubagentLiveStatus as S;
        Self {
            contexts: live.contexts.iter().map(Ctx::from).collect(),
            updates: live.updates.clone(),
            status: live.status.map(|s| match s {
                S::Running => LiveStatus::Running,
                S::Idle => LiveStatus::Idle,
                S::Interrupted => LiveStatus::Interrupted,
                S::Failed => LiveStatus::Failed,
                S::Stopped => LiveStatus::Stopped,
                S::RoundLimit => LiveStatus::RoundLimit,
            }),
            usage_by_round: live.usage_by_round.as_ref().map(|m| m.iter().map(|(k, v)| (*k as u64, v.clone())).collect()),
        }
    }
}

/// What the run's stream hands over (`contexts_from_model_run`), as the shape views are derived from.
impl From<&mewrk_client::runs::LiveContext> for Ctx {
    fn from(c: &mewrk_client::runs::LiveContext) -> Self {
        use mewrk_client::runs::StreamingToolPhase as P;
        Self {
            item: c.item.clone(),
            streaming: c.is_streaming(),
            stream_status: c.stream_status.map(|p| {
                match p {
                    P::Announced => "announced",
                    P::Ready => "ready",
                    P::Running => "running",
                    P::Completed => "completed",
                }
                .to_owned()
            }),
            live: c.live.as_deref().map(LiveState::from),
        }
    }
}

/// One context of `RunView::rendered_contexts`, the persisted timeline with the run's stream merged in.
impl From<mewrk_client::runs::RenderedContext<'_>> for Ctx {
    fn from(c: mewrk_client::runs::RenderedContext<'_>) -> Self {
        use mewrk_client::runs::StreamingToolPhase as P;
        Self {
            item: c.item().clone(),
            streaming: c.streaming(),
            stream_status: c.stream_status().map(|p| {
                match p {
                    P::Announced => "announced",
                    P::Ready => "ready",
                    P::Running => "running",
                    P::Completed => "completed",
                }
                .to_owned()
            }),
            live: c.live().map(|l| LiveState::from(&**l)),
        }
    }
}

impl From<ContextItem> for Ctx {
    fn from(item: ContextItem) -> Self {
        Self { item, streaming: false, stream_status: None, live: None }
    }
}

/// The fields of a tool card, borrowed, since `ContextItem::Tool` has too many to match on everywhere.
#[derive(Clone, Copy, Debug)]
pub struct ToolRef<'a> {
    pub id: &'a str,
    pub tool_name: &'a str,
    pub input: &'a JsonObject,
    pub result: &'a ToolResult,
    pub subagent: Option<&'a SubagentRunRecord>,
    pub created_at: &'a str,
    pub streaming: bool,
    pub stream_status: Option<&'a str>,
    pub live: Option<&'a LiveState>,
}

pub fn tool_of(item: &ContextItem) -> Option<ToolRef<'_>> {
    match item {
        ContextItem::Tool { id, tool_name, input, result, subagent, created_at, .. } => {
            Some(ToolRef { id, tool_name, input, result, subagent: subagent.as_ref(), created_at, streaming: false, stream_status: None, live: None })
        }
        _ => None,
    }
}

impl Ctx {
    pub fn id(&self) -> &str {
        self.item.id()
    }

    pub fn created_at(&self) -> &str {
        self.item.created_at()
    }

    pub fn is_user(&self) -> bool {
        matches!(self.item, ContextItem::User { .. })
    }

    pub fn is_tool(&self) -> bool {
        matches!(self.item, ContextItem::Tool { .. })
    }

    /// What a text context says; a reasoning card's text counts too, as `context.content` did.
    pub fn content(&self) -> Option<&str> {
        match &self.item {
            ContextItem::User { content, .. } | ContextItem::Assistant { content, .. } | ContextItem::System { content, .. } => Some(content),
            ContextItem::Reasoning { content, .. } => content.as_deref(),
            ContextItem::Tool { .. } => None,
        }
    }

    pub fn tool(&self) -> Option<ToolRef<'_>> {
        tool_of(&self.item).map(|t| ToolRef { streaming: self.streaming, stream_status: self.stream_status.as_deref(), live: self.live.as_ref(), ..t })
    }
}

/// A view status names the same things a `SubagentRunStatus` does, plus "still going".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubagentViewStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
    Stopped,
    RoundLimit,
}

impl From<SubagentRunStatus> for SubagentViewStatus {
    fn from(s: SubagentRunStatus) -> Self {
        match s {
            SubagentRunStatus::Completed => Self::Completed,
            SubagentRunStatus::Interrupted => Self::Interrupted,
            SubagentRunStatus::Failed => Self::Failed,
            SubagentRunStatus::Stopped => Self::Stopped,
            SubagentRunStatus::RoundLimit => Self::RoundLimit,
        }
    }
}

impl From<LiveStatus> for SubagentViewStatus {
    fn from(s: LiveStatus) -> Self {
        match s {
            LiveStatus::Running => Self::Running,
            LiveStatus::Idle | LiveStatus::Completed => Self::Completed,
            LiveStatus::Interrupted => Self::Interrupted,
            LiveStatus::Failed => Self::Failed,
            LiveStatus::Stopped => Self::Stopped,
            LiveStatus::RoundLimit => Self::RoundLimit,
        }
    }
}
