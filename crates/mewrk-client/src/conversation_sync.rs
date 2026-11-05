//! The command channel for conversation data (`lib/conversationSync.ts`).
//!
//! The host is the sole writer of conversation bodies; the renderer sends intent. Commands execute in send order, so
//! creation, mutation and deletion cannot overtake each other.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::conversation_bodies::UNLOADED_BODY_EXPECTED_IDS;
use crate::doc::{call_host, DocBackend, SerialQueue};
use crate::model::Conversation;
use crate::timers::TimerId;
use crate::{App, Task};

/// Debounce window that combines consecutive edits to one conversation command.
const COMMIT_DEBOUNCE: Duration = Duration::from_millis(250);

struct PendingCommit {
    workspace_id: String,
    /// Main-timeline context ids visible to the renderer before this debounce window.
    base_context_ids: Vec<String>,
    next: Conversation,
    body_unloaded: bool,
    timer: TimerId,
}

#[derive(Default)]
pub(crate) struct ConversationSyncState {
    pending: HashMap<String, PendingCommit>,
    /// Host-rejected write reasons by conversation, cleared after successful writes.
    failures: HashMap<String, String>,
    queue: Option<SerialQueue>,
}

fn sync(app: &mut App) -> &mut ConversationSyncState {
    app.local::<ConversationSyncState>()
}

/// A failed barrier still lets the command through: the host then refuses it with its own reason, which is the one
/// worth recording.
fn await_workspace(task: &Task, workspace_id: &str) {
    let id = workspace_id.to_owned();
    let holds = task.update(move |app| app.host_holds_workspace(&id));
    if !holds {
        let _ = task.flush_document_store(false);
    }
}

fn apply_authority(task: &Task, workspace_id: &str, conversation: Conversation) {
    let workspace_id = workspace_id.to_owned();
    task.update(move |app| app.apply_authoritative_conversation(&workspace_id, conversation));
}

/// On a rejected write, the renderer's optimistic copy is no longer valid. Reload the host-authoritative body so a run
/// never uses a context the host did not accept.
fn adopt_authority_after_refusal(task: &Task, backend: Option<&DocBackend>, workspace_id: &str, conversation_id: &str, error: &str) {
    let message = if error.trim().is_empty() { "宿主拒收了这次对话写入".to_owned() } else { error.to_owned() };
    let id = conversation_id.to_owned();
    task.update(move |app| {
        sync(app).failures.insert(id, message);
    });
    let authoritative = call_host(task, backend, "load_conversation", json!({ "conversationId": conversation_id })).ok().and_then(|value| serde_json::from_value::<Option<Conversation>>(value).ok()).flatten();
    if let Some(authoritative) = authoritative {
        apply_authority(task, workspace_id, authoritative);
    }
}

fn parse_conversation(command: &str, value: Value) -> Result<Conversation, String> {
    serde_json::from_value(value).map_err(|e| format!("{command}: {e}"))
}

impl App {
    fn enqueue_conversation_command(&mut self, job: impl FnOnce(&Task, Option<&DocBackend>) + Send + 'static) {
        let task = self.task();
        let backend = self.doc_backend();
        let queue = sync(self).queue.get_or_insert_with(|| SerialQueue::new("mewrk-conversation-sync"));
        queue.push(Box::new(move || job(&task, backend.as_ref())));
    }

    /// `applyAuthoritativeConversation`: replaces the local read model with the host-written body by id. A conversation
    /// whose body is unloaded takes only the metadata: loading a body back is the body cache's call, made when the
    /// conversation is opened, not a side effect of a write reply.
    pub fn apply_authoritative_conversation(&mut self, workspace_id: &str, mut conversation: Conversation) {
        let unloaded = self.unloaded.contains(&conversation.id);
        if unloaded {
            crate::conversation_bodies::hollow_conversation(&mut conversation);
        }
        let id = conversation.id.clone();
        self.update_document_if(|doc| {
            let Some(slot) = doc.workspaces.iter_mut().find(|workspace| workspace.id == workspace_id).and_then(|workspace| workspace.conversations.iter_mut().find(|candidate| candidate.id == id)) else { return false };
            *slot = conversation;
            true
        });
        self.mark_body_dirty(&id);
    }

    /// `conversationSync.created`: a conversation, including a fork destination, was created. `ordered_ids` is the
    /// workspace's new sidebar order.
    pub fn sync_conversation_created(&mut self, workspace_id: &str, conversation: Conversation, ordered_ids: Vec<String>) {
        let workspace_id = workspace_id.to_owned();
        self.enqueue_conversation_command(move |task, backend| {
            await_workspace(task, &workspace_id);
            let id = conversation.id.clone();
            let created = call_host(task, backend, "create_conversation", json!({ "workspaceId": workspace_id, "conversation": conversation })).and_then(|value| parse_conversation("create_conversation", value));
            match created {
                Err(error) => {
                    adopt_authority_after_refusal(task, backend, &workspace_id, &id, &error);
                    eprintln!("新建对话未能写入宿主: {error}");
                }
                Ok(stored) => {
                    let forget = id.clone();
                    task.update(move |app| {
                        sync(app).failures.remove(&forget);
                    });
                    apply_authority(task, &workspace_id, stored);
                    if let Err(error) = call_host(task, backend, "reorder_conversations", json!({ "workspaceId": workspace_id, "conversationIds": ordered_ids })) {
                        eprintln!("新建对话未能写入宿主: {error}");
                    }
                }
            }
        });
    }

    /// `conversationSync.deleted`: a conversation was deleted. A pending edit to it is dropped.
    pub fn sync_conversation_deleted(&mut self, workspace_id: &str, conversation_id: &str) {
        let entry = sync(self).pending.remove(conversation_id);
        if let Some(entry) = entry {
            self.clear_timer(entry.timer);
        }
        sync(self).failures.remove(conversation_id);
        let args = json!({ "workspaceId": workspace_id, "conversationId": conversation_id });
        self.enqueue_conversation_command(move |task, backend| {
            if let Err(error) = call_host(task, backend, "delete_conversation", args) {
                eprintln!("删除对话未能写入宿主: {error}");
            }
        });
    }

    /// `conversationSync.changed`: a conversation changed. `base_context_ids` are the renderer-visible timeline before
    /// the change. Edits in one debounce window make one command.
    pub fn sync_conversation_changed(&mut self, workspace_id: &str, base_context_ids: Vec<String>, next: Conversation) {
        let id = next.id.clone();
        let existing = sync(self).pending.remove(&id);
        // The first baseline in a debounce window is the host-visible one;
        // renderer-created intermediate states cannot be used as its baseline.
        let base_context_ids = match existing {
            Some(entry) => {
                self.clear_timer(entry.timer);
                entry.base_context_ids
            }
            None => base_context_ids,
        };
        let body_unloaded = self.unloaded.contains(&id);
        let commit_id = id.clone();
        let timer = self.set_timer(COMMIT_DEBOUNCE, move |app| app.commit_conversation(&commit_id));
        sync(self).pending.insert(id, PendingCommit { workspace_id: workspace_id.to_owned(), base_context_ids, next, body_unloaded, timer });
    }

    /// `conversationSync.reordered`: the sidebar order changed, or a conversation moved workspaces.
    pub fn sync_conversations_reordered(&mut self, workspace_id: &str, ordered_ids: Vec<String>) {
        let workspace_id = workspace_id.to_owned();
        self.enqueue_conversation_command(move |task, backend| {
            // Moving a conversation into a project added in the same event is a reorder of that project.
            await_workspace(task, &workspace_id);
            if let Err(error) = call_host(task, backend, "reorder_conversations", json!({ "workspaceId": workspace_id, "conversationIds": ordered_ids })) {
                eprintln!("对话顺序未能写入宿主: {error}");
            }
        });
    }

    fn commit_conversation(&mut self, conversation_id: &str) {
        let Some(entry) = sync(self).pending.remove(conversation_id) else { return };
        self.clear_timer(entry.timer);
        let conversation_id = conversation_id.to_owned();
        self.enqueue_conversation_command(move |task, backend| {
            await_workspace(task, &entry.workspace_id);
            // A body that is not loaded is an empty stand-in, never the conversation's timeline: whatever the
            // baseline, it must not match the host's, so the host keeps its body and takes the metadata.
            let expected: Vec<String> = if entry.body_unloaded { UNLOADED_BODY_EXPECTED_IDS.iter().map(|id| (*id).to_owned()).collect() } else { entry.base_context_ids };
            let request = json!({ "workspaceId": entry.workspace_id, "conversation": entry.next, "expectedContextIds": expected });
            match call_host(task, backend, "update_conversation", request).and_then(|value| parse_conversation("update_conversation", value)) {
                Err(error) => {
                    adopt_authority_after_refusal(task, backend, &entry.workspace_id, &conversation_id, &error);
                    eprintln!("对话改动未能写入宿主: {error}");
                }
                Ok(authoritative) => {
                    let workspace_id = entry.workspace_id;
                    let id = conversation_id;
                    task.update(move |app| {
                        sync(app).failures.remove(&id);
                        // The reply answers the body that was sent, not the one on screen. If the user edited again
                        // while it was in flight, `pending` already holds that newer body and will commit it in its
                        // own window; installing the reply here would put the control they just moved back where it
                        // was until that commit answers: a switch that flips itself off a moment after it was turned on.
                        if !sync(app).pending.contains_key(&id) {
                            app.apply_authoritative_conversation(&workspace_id, authoritative);
                        }
                    });
                }
            }
        });
    }

    #[cfg(test)]
    pub(crate) fn flush_conversation_sync_one(&mut self, conversation_id: &str) {
        self.commit_conversation(conversation_id);
    }

    /// `conversationSync.flush`: waits for every in-flight command, sending what's still debouncing. Call before
    /// starting a run so the user message is persisted.
    pub fn flush_conversation_sync(&mut self, done: impl FnOnce(&mut App) + Send + 'static) {
        let pending: Vec<String> = sync(self).pending.keys().cloned().collect();
        for id in pending {
            self.commit_conversation(&id);
        }
        self.enqueue_conversation_command(move |task, _| task.post(done));
    }

    /// `conversationSync.refresh`: flushes, then retrieves the host-authoritative body. `Ok(None)` when the host doesn't have it.
    pub fn refresh_conversation(&mut self, conversation_id: &str, done: impl FnOnce(&mut App, Result<Option<Conversation>, String>) + Send + 'static) {
        let id = conversation_id.to_owned();
        self.flush_conversation_sync(move |app| {
            app.invoke::<Option<Conversation>>("load_conversation", json!({ "conversationId": id }), done);
        });
    }

    /// `conversationSync.lastFailure`: the reason the host most recently rejected a write for this conversation; clears after success.
    pub fn conversation_write_failure(&mut self, conversation_id: &str) -> Option<String> {
        sync(self).failures.get(conversation_id).cloned()
    }

    /// Whether an edit to this conversation is still waiting for its debounce window.
    pub fn conversation_commit_is_pending(&mut self, conversation_id: &str) -> bool {
        sync(self).pending.contains_key(conversation_id)
    }
}

impl Task {
    /// [`App::flush_conversation_sync`], for a task thread.
    pub fn flush_conversation_sync(&self) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.post(move |app| {
            app.flush_conversation_sync(move |_| {
                let _ = tx.send(());
            });
        });
        let _ = rx.recv();
    }
}
