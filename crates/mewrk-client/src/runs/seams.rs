//! Everything the run code needs from areas other modules own, in one place: what upstream's `SendPipelineHost` reached
//! through, and the one-shot requests to views that hold no state of their own.

use serde_json::{json, Value};

use super::bridge::Cx;
use super::composer::Deferred;
use super::state::RunsState;
use crate::doc::QuarantinedContextReplacement;
use crate::machine_shells::{available_shell_backends, host_platform, tools_for_shell_backends};
use crate::model::{AppDocument, ContextItem, Conversation, ToolDescriptor, Workspace};
use crate::tool_defaults::localize_tool_descriptor;
use crate::App;

pub use crate::doc::{find_conversation, model_choice};
pub use crate::drafts::is_draft_conversation_id;

/// One-shot requests to whoever owns the view: the pipeline can't reach them, and they hold no state of their own.
/// Each is drained by what handles it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunSignals {
    /// `openProviderSettings`: `openGlobalSettings("providers")`.
    pub open_provider_settings: bool,
    /// `scrollTimelineToBottom`.
    pub scroll_timeline_to_bottom: bool,
    /// `registerPreviewSession`: conversations that now own a preview page.
    pub register_preview_sessions: Vec<String>,
    /// `setBranchChipError`: an isolated worktree could not be made, so nothing was sent.
    pub branch_chip_error: Option<String>,
}

pub fn signals(app: &mut App) -> &mut RunSignals {
    &mut app.local::<RunsState>().signals
}

/// `activeWorkspaceId()` and `activeConversationId()`.
pub fn active_ids(app: &mut App) -> (Option<String>, Option<String>) {
    (app.active_workspace_id(), app.active_conversation_id())
}

/// `activeConversationTools()`: the localized catalog, narrowed to the shells the conversation's machines have.
pub fn active_conversation_tools(app: &mut App) -> Vec<ToolDescriptor> {
    let language = app.doc.global_settings.resolved_app_language;
    let catalog: Vec<ToolDescriptor> = app.doc.tools.iter().map(|tool| localize_tool_descriptor(tool, language)).collect();
    let workspaces = app.active_conversation_workspaces();
    let backends = available_shell_backends(&workspaces, app.machine_shells(), host_platform());
    tools_for_shell_backends(&catalog, |tool| tool.name.as_str(), &backends)
}

/// `activeEnabledTools()`: names enabled for the active conversation and present in the catalog.
pub fn active_enabled_tools(app: &mut App) -> Vec<String> {
    let tools = active_conversation_tools(app);
    let Some(conversation) = app.active_conversation_view() else { return Vec::new() };
    conversation.settings.enabled_tools.iter().filter(|name| tools.iter().any(|tool| &tool.name == *name)).cloned().collect()
}

/// The workspace holding a conversation.
pub fn workspace_of<'a>(document: &'a AppDocument, conversation_id: &str) -> Option<&'a Workspace> {
    document.workspaces.iter().find(|workspace| workspace.conversations.iter().any(|conversation| conversation.id == conversation_id))
}

/// The workspace's path, what a request's `workspacePath` carries.
pub fn workspace_path(app: &App, workspace_id: &str) -> String {
    app.doc.workspaces.iter().find(|workspace| workspace.id == workspace_id).map(|workspace| workspace.path.clone()).unwrap_or_default()
}

/// `persistDocumentImmediately` for one conversation: `next` takes its place at once, so later changes build on it. The
/// answer says whether the write, and the conversation command it carried, went through.
pub fn publish_conversation(app: &mut App, workspace_id: &str, next: Conversation, durable: bool) -> Deferred<Result<(), String>> {
    let saved = Deferred::new();
    let settled = saved.clone();
    let changed = [(workspace_id.to_owned(), next.id.clone())];
    let workspace = workspace_id.to_owned();
    app.persist_document_immediately(
        durable,
        &changed,
        move |document| {
            let conversation = document.workspaces.iter_mut().find(|candidate| candidate.id == workspace).and_then(|candidate| candidate.conversations.iter_mut().find(|conversation| conversation.id == next.id));
            if let Some(conversation) = conversation {
                *conversation = next;
            }
        },
        move |_, result| settled.resolve(result),
    );
    saved
}

/// `conversationSync.refresh` then `applyAuthoritativeConversation`: after settlement, replace the read model from the
/// host conversation store so UI and disk match exactly.
pub fn refresh_conversation(app: &mut App, workspace_id: &str, conversation_id: &str) {
    let cx = Cx::of(app);
    let workspace_id = workspace_id.to_owned();
    let conversation_id = conversation_id.to_owned();
    app.spawn(move |task| {
        task.flush_conversation_sync();
        if let Ok(Some(authoritative)) = cx.call::<Option<Conversation>>("load_conversation", json!({ "conversationId": conversation_id })) {
            task.post(move |app| app.apply_authoritative_conversation(&workspace_id, authoritative));
        }
    });
}

/// `ensureConversationBody` for a caller that does not wait: the reconcile tries again once the body is here.
pub fn ensure_conversation_body_detached(app: &mut App, conversation_id: &str) {
    if app.conversation_body_is_loading(conversation_id) {
        return;
    }
    app.ensure_conversation_loaded(conversation_id, |app, _| app.note_runs_changed());
}

/// `applySettledToolContext`: replaces a settled signed tool card's persisted copy by id and persists it debounced, so
/// finalized subagent records survive a mid-run process death.
pub fn apply_settled_tool_context(app: &mut App, conversation_id: &str, context: ContextItem) {
    let location = QuarantinedContextReplacement { conversation_id: conversation_id.to_owned(), context_id: context.id().to_owned(), replacement: context };
    app.update_document_if(|document| crate::doc::apply_quarantined_context_replacements(document, std::slice::from_ref(&location)));
}

/// A command with no answer worth reading.
pub fn call_unit(cx: &Cx, command: &str, args: Value) -> Result<(), String> {
    cx.call::<Value>(command, args).map(|_| ())
}
