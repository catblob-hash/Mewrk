//! Port of the run commands in `lib/runtime.ts`, as blocking calls for a task thread.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use super::state::{EventSink, RunBackend, RunsState};
use super::types::*;
use crate::model::{ConversationPlan, Conversation, QueuedMessage, RunModelResponse, SubagentRunRecord, ToolExecutionRequest, ToolResult};
use crate::{App, Channel, Task};

/// A stream a command opened: closes the channel when the run is over.
pub(super) struct OpenStream(Option<Channel>);

impl OpenStream {
    pub fn close(&self) {
        if let Some(channel) = &self.0 {
            channel.close();
        }
    }
}

/// `await`-style access to the host and the UI thread for one piece of pipeline work.
#[derive(Clone)]
pub struct Cx {
    pub(super) task: Task,
    backend: Option<Arc<dyn RunBackend>>,
}

impl Cx {
    pub(super) fn of(app: &mut App) -> Self {
        let backend = app.local::<RunsState>().backend.clone();
        Self { task: app.task(), backend }
    }

    /// Runs on the UI thread and waits for the answer: every stretch of upstream's code between two `await`s.
    pub fn update<R: Send + 'static>(&self, f: impl FnOnce(&mut App) -> R + Send + 'static) -> R {
        self.task.update(f)
    }

    pub fn post(&self, f: impl FnOnce(&mut App) + Send + 'static) {
        self.task.post(f);
    }

    /// `await invoke(command, args)`.
    pub fn call<T: DeserializeOwned>(&self, command: &str, args: Value) -> Result<T, String> {
        let value = match &self.backend {
            Some(backend) => backend.invoke(command, args, None)?,
            None => self.task.invoke::<Value>(command, args)?,
        };
        serde_json::from_value(value).map_err(|error| format!("{command}: {error}"))
    }

    /// The same for a command with an `onEvent` channel, whose messages run on the UI thread in order.
    fn call_streaming<T: DeserializeOwned>(&self, command: &str, mut args: Value, on_event: impl Fn(&mut App, Value) + Send + Sync + 'static) -> (Result<T, String>, OpenStream) {
        let (value, stream) = match &self.backend {
            Some(backend) => {
                let task = self.task.clone();
                let on_event = Arc::new(on_event);
                let sink: EventSink = Arc::new(move |event| {
                    let on_event = on_event.clone();
                    task.post(move |app| on_event(app, event));
                });
                (backend.invoke(command, args, Some(sink)), OpenStream(None))
            }
            None => {
                let channel = self.task.open_channel(on_event);
                args["onEvent"] = channel.arg();
                (self.task.invoke::<Value>(command, args), OpenStream(Some(channel)))
            }
        };
        (value.and_then(|value| serde_json::from_value(value).map_err(|error| format!("{command}: {error}"))), stream)
    }

    /// `runModel`.
    pub(super) fn run_model(&self, request: &ModelRunRequest, request_id: &str, on_event: impl Fn(&mut App, Value) + Send + Sync + 'static) -> RunOutcome {
        if !request.provider.enabled {
            return Err(format!("API 提供商 {} 未启用，不能执行聊天请求", request.provider.name));
        }
        let request_json = serde_json::to_value(&request.run).map_err(|error| format!("run_model: {error}"))?;
        let (result, stream) = self.call_streaming::<RunModelResponse>(
            "run_model",
            json!({
                "request": request_json,
                "forkPromptContextId": request.fork_prompt_context_id,
                "compactNow": request.compact_now.then_some(true),
                "requestId": request_id,
            }),
            on_event,
        );
        stream.close();
        result
    }

    /// `attachModelRun`: attaches a conversation's host event stream, replaying buffered events into `on_event`.
    pub(super) fn attach_model_run(&self, conversation_id: &str, on_event: impl Fn(&mut App, Value) + Send + Sync + 'static) -> (Result<AttachRunResult, String>, OpenStream) {
        let (value, stream) = self.call_streaming::<Value>("attach_model_run", json!({ "conversationId": conversation_id }), on_event);
        (value.and_then(AttachRunResult::parse), stream)
    }

    /// `takeRunSettlement`: claims a `run_concluded` settlement.
    pub(super) fn take_run_settlement(&self, conversation_id: &str) -> Result<Option<RunSettlementPayload>, String> {
        self.call("take_run_settlement", json!({ "conversationId": conversation_id }))
    }

    pub(super) fn cancel_model_run(&self, request_id: &str) -> Result<bool, String> {
        self.call("cancel_model_run", json!({ "requestId": request_id }))
    }

    /// Cancels the current run by conversation when a reload has lost the renderer's request id.
    pub(super) fn cancel_conversation_run(&self, conversation_id: &str) -> Result<bool, String> {
        self.call("cancel_conversation_run", json!({ "conversationId": conversation_id }))
    }

    pub(super) fn list_resumable_runs(&self) -> Result<Vec<ResumableRun>, String> {
        self.call("list_resumable_runs", json!({}))
    }

    pub(super) fn list_wake_pending_conversations(&self) -> Result<Vec<String>, String> {
        self.call("list_wake_pending_conversations", json!({}))
    }

    pub(super) fn list_pending_tool_prompts(&self) -> Result<Vec<ConversationToolPrompt>, String> {
        self.call("list_pending_tool_prompts", json!({}))
    }

    /// `loadConversationPlan`: the plan is host state, not part of the document snapshot.
    pub(super) fn load_conversation_plan(&self, conversation_id: &str) -> Result<Option<ConversationPlan>, String> {
        self.call("load_conversation_plan", json!({ "conversationId": conversation_id }))
    }

    pub(super) fn list_pending_fork_requests(&self) -> Result<Vec<PendingForkRequest>, String> {
        self.call("list_pending_fork_requests", json!({}))
    }

    pub(super) fn list_pending_fork_starts(&self) -> Result<Vec<PendingForkStart>, String> {
        self.call("list_pending_fork_starts", json!({}))
    }

    pub(super) fn list_fork_decisions(&self, conversation_id: &str) -> Result<Vec<ForkDecisionRecord>, String> {
        self.call("list_fork_decisions", json!({ "conversationId": conversation_id }))
    }

    /// `resolveForkRequest`: approval creates the child and returns it; the `forkResolved` push event — not this
    /// result — is what starts the child's run and carries the decision record the task bar shows.
    pub(super) fn resolve_fork_request(&self, fork_id: &str, approved: bool) -> Result<Option<Conversation>, String> {
        self.call("resolve_fork_request", json!({ "forkId": fork_id, "approved": approved }))
    }

    pub(super) fn steer_model_run(&self, request_id: &str, message: &QueuedMessage) -> Result<(), String> {
        self.call::<Value>(
            "steer_model_run",
            json!({
                "requestId": request_id,
                "messageId": message.id,
                "content": message.content,
                "images": message.images,
                "files": message.files,
                "createdAt": message.created_at,
            }),
        )
        .map(|_| ())
    }

    pub(super) fn request_tool_approval(&self, request: &ToolExecutionRequest) -> Result<ToolApprovalGrant, String> {
        self.call("request_tool_approval", json!({ "request": request }))
    }

    /// `resolveToolPrompt`: answers one approval card. For a call the renderer started itself, the returned grant
    /// carries the nonce — the backend mints it here, from the arguments it classified when the card was raised, so
    /// the call that runs is the one the user saw. For a card raised inside a model run, the blocked worker is what
    /// resumes and the grant is empty.
    pub(super) fn resolve_tool_prompt(&self, prompt_id: &str, decision: ToolPromptDecision, feedback: Option<&str>, question: Option<&QuestionResponse>) -> Result<ToolApprovalGrant, String> {
        self.call("resolve_tool_prompt", json!({ "promptId": prompt_id, "decision": decision, "feedback": feedback, "question": question }))
    }

    pub(super) fn execute_tool(&self, request: &ToolExecutionRequest, approval_nonce: Option<&str>) -> Result<ToolResult, String> {
        self.call("execute_tool", json!({ "request": request, "approvalNonce": approval_nonce }))
    }

    /// `controlWorkflowStep`: asks a live workflow run to skip or retry one running step. The run is addressed by its
    /// conversation, not by the round that started it: a run outlives that round, and its controls work for as long
    /// as it runs. The host refuses a run that has already ended.
    pub(super) fn control_workflow_step(&self, conversation_id: &str, run_id: &str, step_index: u64, action: WorkflowStepAction) -> Result<(), String> {
        self.call::<Value>("workflow_step_control", json!({ "conversationId": conversation_id, "runId": run_id, "stepIndex": step_index, "action": action })).map(|_| ())
    }

    /// `workflowStepRecord`: `None` is a legal answer, not an error: run directories are deleted with their
    /// conversation, so a dangling coordinate renders as "body no longer available" rather than throwing.
    pub(super) fn workflow_step_record(&self, conversation_id: &str, run_id: &str, step_index: u64) -> Result<Option<SubagentRunRecord>, String> {
        self.call("workflow_step_record", json!({ "conversationId": conversation_id, "runId": run_id, "stepIndex": step_index }))
    }

    pub(super) fn get_tool_explanations(&self, conversation_id: &str) -> Result<ToolExplanations, String> {
        self.call("get_tool_explanations", json!({ "conversationId": conversation_id }))
    }

    /// `prepareImageAttachment`: stores image bytes outside the main document and returns its lightweight reference.
    pub(super) fn prepare_image_attachment(&self, name: &str, bytes: &[u8]) -> Result<crate::model::ImageAttachment, String> {
        self.call("image_attachment_upload", json!({ "name": name, "data": super::attachments::bytes_to_base64(bytes) }))
    }

    /// `prepareFileAttachment`: stores a non-image file with the host and returns its reference.
    pub(super) fn prepare_file_attachment(&self, document: &super::attachments::ReadDocument) -> Result<crate::model::FileAttachment, String> {
        self.call(
            "file_attachment_upload",
            json!({
                "name": document.name,
                "data": super::attachments::bytes_to_base64(&document.bytes),
                "format": document.format,
                "text": document.extracted.as_ref().map(|extracted| extracted.text.clone()),
                "pages": document.extracted.as_ref().map(|extracted| extracted.pages),
            }),
        )
    }
}

/// `RunSettlementPayload`.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSettlementPayload {
    #[serde(default)]
    pub response: Option<RunModelResponse>,
    #[serde(default)]
    pub error: Option<String>,
}

impl RunSettlementPayload {
    /// `settlementToResponse`.
    pub(super) fn into_outcome(settlement: Option<Self>, missing_message: &str) -> RunOutcome {
        match settlement {
            Some(Self { response: Some(response), .. }) => Ok(response),
            Some(Self { error: Some(error), .. }) => Err(error),
            _ => Err(missing_message.to_owned()),
        }
    }
}

/// `AttachRunResult`.
#[derive(Clone, Debug, PartialEq)]
pub enum AttachRunResult {
    None,
    Running { request_id: String, request: Box<ModelRunRequest> },
    Finished { request_id: String, request: Box<ModelRunRequest>, settlement: Box<RunSettlementPayload> },
}

impl AttachRunResult {
    fn parse(value: Value) -> Result<Self, String> {
        let request_of = |value: &Value| -> Result<Box<ModelRunRequest>, String> {
            let run = serde_json::from_value(value.get("request").cloned().unwrap_or(Value::Null)).map_err(|error| format!("attach_model_run: {error}"))?;
            Ok(Box::new(ModelRunRequest { run, fork_prompt_context_id: None, compact_now: false }))
        };
        let request_id = || value.get("requestId").and_then(Value::as_str).unwrap_or("").to_owned();
        match value.get("status").and_then(Value::as_str) {
            Some("running") => Ok(Self::Running { request_id: request_id(), request: request_of(&value)? }),
            Some("finished") => Ok(Self::Finished {
                request_id: request_id(),
                request: request_of(&value)?,
                settlement: Box::new(serde_json::from_value(value.get("settlement").cloned().unwrap_or(Value::Null)).map_err(|error| format!("attach_model_run: {error}"))?),
            }),
            _ => Ok(Self::None),
        }
    }
}
