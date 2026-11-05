//! Wire shapes the host exchanges with the renderer but keeps in private modules (`tool_prompt`, `fork_requests`, ...).
//! These mirror `src/types.ts`; each serializes exactly as the host's own does.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model_stream::ModelRunState;
use crate::model::{ApiProvider, ContextItem, ModelProfile, ModelUsage, ProviderFamily, ReasoningEffort, RunModelRequest, ToolDescriptor};

/// `ModelRunRequest`: the host's request plus the two arguments `run_model` takes beside it.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRunRequest {
    pub run: RunModelRequest,
    /// Host-minted first-run intent, consumed only when the run becomes adoptable.
    pub fork_prompt_context_id: Option<String>,
    /// The user's "compact now": the run compacts the context natively at its
    /// first boundary, opens the continuation and ends without a turn of its own.
    pub compact_now: bool,
}

impl std::ops::Deref for ModelRunRequest {
    type Target = RunModelRequest;

    fn deref(&self) -> &RunModelRequest {
        &self.run
    }
}

impl ModelRunRequest {
    /// The fields a renderer fills in. `RunModelRequest` has dozens more, all host-only and `serde(skip)`, so the one
    /// way to name just these is the wire form; contexts and tools are assigned after it, to spare a round trip.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: &ApiProvider,
        model: &ModelProfile,
        reasoning_effort: ReasoningEffort,
        conversation_id: &str,
        workspace_path: &str,
        enabled_tools: Vec<String>,
        contexts: Vec<ContextItem>,
        tools: Vec<ToolDescriptor>,
    ) -> Result<Self, String> {
        let mut run: RunModelRequest = serde_json::from_value(serde_json::json!({
            "provider": provider,
            "model": model,
            "reasoningEffort": reasoning_effort,
            "conversationId": conversation_id,
            "workspacePath": workspace_path,
            "enabledTools": [],
            "contexts": [],
            "tools": [],
        }))
        .map_err(|error| format!("run request: {error}"))?;
        run.enabled_tools = enabled_tools;
        run.contexts = contexts;
        run.tools = tools;
        Ok(Self { run, fork_prompt_context_id: None, compact_now: false })
    }
}

/// Which card a prompt draws: `tool_prompt::PromptKind`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    #[default]
    Tool,
    PlanExit,
    Question,
}

/// One approval card: `PendingToolPrompt`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingToolPrompt {
    pub prompt_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub kind: PromptKind,
    pub label: String,
    pub summary: String,
    pub risk_level: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<String>,
    pub allow_always_offered: bool,
    #[serde(default)]
    pub mandatory: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions: Option<Value>,
}

/// A row of `list_pending_tool_prompts`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationToolPrompt {
    pub conversation_id: String,
    #[serde(flatten)]
    pub prompt: PendingToolPrompt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPromptDecision {
    Deny,
    AllowOnce,
    AllowAlways,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionAction {
    #[default]
    Submit,
    Close,
    Chat,
}

/// What the user did with a question card.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionResponse {
    pub action: QuestionAction,
    #[serde(default)]
    pub answers: Vec<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previews: Option<Vec<Option<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<Vec<Option<String>>>,
}

impl QuestionResponse {
    /// `{ action: "close", answers: [] }`, what an untouched card hands back.
    pub fn close() -> Self {
        Self { action: QuestionAction::Close, ..Self::default() }
    }
}

/// What `resolve_tool_prompt` and `request_tool_approval` answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolApprovalGrant {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(default)]
    pub expires_in_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PendingToolPrompt>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingForkRequest {
    pub fork_id: String,
    pub workspace_id: String,
    pub source_conversation_id: String,
    pub source_title: String,
    pub prompt: String,
    pub requested_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkDecisionRecord {
    pub fork_id: String,
    pub workspace_id: String,
    pub source_conversation_id: String,
    pub title: String,
    pub prompt: String,
    pub requested_at: String,
    pub decided_at: String,
    pub approved: bool,
    pub child_conversation_id: Option<String>,
}

/// A row of `list_pending_fork_starts`, and what the push events queue up.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingForkStart {
    pub workspace_id: String,
    pub conversation_id: String,
    #[serde(default)]
    pub prompt_context_id: String,
    /// `false` only files and follows the child: nothing is armed there.
    #[serde(default = "yes")]
    pub starts_run: bool,
}

fn yes() -> bool {
    true
}

/// `ResumableRun`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumableRun {
    pub conversation_id: String,
    pub request_id: String,
    pub running: bool,
}

/// `WorkflowProgressEntry`; the host's `ProgressRow` as the renderer reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowProgressEntry {
    pub kind: WorkflowEntryKind,
    pub index: usize,
    pub state: WorkflowStepState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub cached: bool,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub skipped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowEntryKind {
    Agent,
    Log,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowStepState {
    Start,
    Progress,
    Done,
    Error,
}

/// `WorkflowStepAction`: what the run panel can ask of one running step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowStepAction {
    Skip,
    Retry,
}

/// What the local helper model wrote about a conversation's tool cards, by card id (`ToolExplanations`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExplanations {
    /// Shell command descriptions and subagent titles.
    #[serde(default)]
    pub explanations: std::collections::BTreeMap<String, String>,
    /// Why each failed call failed.
    #[serde(default)]
    pub errors: std::collections::BTreeMap<String, String>,
}

/// `ModelRunErrorState`: a run's last failure, the way the composer and the retry button read it.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRunErrorState {
    pub message: String,
    pub retryable: bool,
    /// Set when a turn already renders this failure, so the composer does not repeat it.
    pub request_id: Option<String>,
    pub provider_name: String,
    pub model_name: String,
    pub workspace_id: String,
    pub request: std::sync::Arc<ModelRunRequest>,
}

/// `ContextUsage`: how full the conversation's context is, as the gauge shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextUsage {
    pub tokens: u64,
    pub estimated: bool,
    pub unprojectable: bool,
    pub provider_id: Option<String>,
    pub family: Option<ProviderFamily>,
}

/// `TurnCompletionMeta`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnCompletionMeta {
    pub model_id: Option<String>,
    pub usage: Option<ModelUsage>,
    pub duration_ms: Option<u64>,
}

/// Where a card that just arrived wants the view to go (`next_tool_prompt_navigation`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolPromptNavigation<T> {
    /// A plan-exit card is answered in the plan pane, because the answer is "is this plan right" and the plan is not
    /// in the timeline.
    PlanPane,
    /// A subagent's card: the page of the agent that asked.
    Subagent(T),
}

/// How a run ended, as `run_model` and the settlement slot both report it (`ModelRunResponse`).
pub type RunOutcome = Result<crate::model::RunModelResponse, String>;

/// A run as the pipeline hands it to a failure path.
pub type RunSnapshot = ModelRunState;
