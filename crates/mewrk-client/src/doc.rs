//! The document: its store and save pipeline (`lib/documentStore.ts`, the save half of `lib/runtime.ts`), what changes it
//! (`lib/documentUpdates.ts`), and the conversation commands of `App.tsx`.
//!
//! `App::doc` is the synchronous authoritative value upstream's `documentStore.current()` returned. Change it through
//! [`App::update_document`] and friends: they publish at once and schedule the debounced background save (420 ms) on
//! one serialized write queue. Conversation bodies never go through that save: the host is their only writer, and the
//! renderer sends intent through `conversation_sync.rs`.
//!
//! What upstream ran in `useEffect` after a document change runs here between frames: see [`App::schedule_doc_effects`].

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use crate::runs::util::{create_id, now_iso};

use crate::conversation_bodies::keep_unloaded_body;
use crate::conversation_tree::{detach_absent_parents, reparent_children};
use crate::model::{ApiProvider, AppDocument, AttachedWorkspace, ContextItem, Conversation, ConversationForkOrigin, ConversationPreset, ConversationPresetSettings, ConversationSettings, ConversationToolLock, ConversationWorktree, ModelProfile, ResolvedLanguage, RunTarget, Workspace};
use crate::timers::TimerId;
use crate::workspaces::{reorder_items, NewConversationSource};
use crate::{t, App, Task};

/// `DocumentSaveStatus`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DocumentSaveStatus {
    #[default]
    Idle,
    Saving,
    Saved,
    Error,
}

/// `debounceMs` of the store.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(420);

/// How long a timeline notice about worktrees a deletion kept stays up: it names directories to find.
pub const KEPT_WORKTREE_NOTICE_MS: u64 = 9000;

/// Stands in for the host in tests and tools, as upstream's tests mock `lib/runtime.ts`.
pub type DocBackend = Arc<dyn Fn(&str, Value) -> Result<Value, String> + Send + Sync>;

/// `await invoke(command, args)` for this module's commands.
pub(crate) fn call_host(task: &Task, backend: Option<&DocBackend>, command: &str, args: Value) -> Result<Value, String> {
    match backend {
        Some(backend) => backend(command, args),
        None => task.invoke::<Value>(command, args),
    }
}

/// A thread that runs jobs one after another: upstream's promise tails.
pub(crate) struct SerialQueue {
    tx: mpsc::Sender<Box<dyn FnOnce() + Send>>,
}

impl SerialQueue {
    pub fn new(name: &str) -> Self {
        let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job();
                }
            })
            .expect("thread");
        Self { tx }
    }

    pub fn push(&self, job: Box<dyn FnOnce() + Send>) {
        let _ = self.tx.send(job);
    }
}

/// Something that takes time to close, run on a task thread: a terminal session, a browser page.
pub type SurfaceCloser = Box<dyn FnOnce(&Task) -> Result<(), String> + Send>;

/// `Promise.allSettled`: runs them all at once and answers each.
pub(crate) fn run_all(closers: Vec<SurfaceCloser>, task: &Task) -> Vec<Result<(), String>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = closers.into_iter().map(|closer| {
            let task = task.clone();
            scope.spawn(move || closer(&task))
        }).collect();
        handles.into_iter().map(|handle| handle.join().unwrap_or_else(|_| Err("panicked".to_owned()))).collect()
    })
}

/// What this module needs from areas it doesn't own. Each area registers what it has with [`App::doc_hooks`]; what
/// nobody has registered reads as "nothing there", so deleting a conversation with no terminal module just closes nothing.
#[derive(Clone, Default)]
pub struct DocHooks {
    /// `gitController.mutationIsActive`. TODO: the Git pane registers this.
    pub git_mutation_active: Vec<Rc<dyn Fn(&mut App, &str) -> bool>>,
    /// The rest of `conversationHasLiveActivity`: unfinished shell tasks, busy terminals. TODO: the task and terminal panes.
    pub live_activity: Vec<Rc<dyn Fn(&mut App, &str) -> bool>>,
    /// `requestConversationBrowserClose` and `requestTerminalSessionClose` of everything a conversation has open,
    /// keyed by the conversation's host id. TODO: the browser and terminal panes.
    pub surface_closers: Vec<Rc<dyn Fn(&mut App, &str) -> Vec<SurfaceCloser>>>,
    /// Terminal sessions only: moving a conversation closes these and keeps its browser page. TODO: the terminal pane.
    pub terminal_closers: Vec<Rc<dyn Fn(&mut App, &str) -> Vec<SurfaceCloser>>>,
    /// `releaseDraftSurfaces`' dev servers: stops what a draft (by the id it will materialize as) started in its project.
    /// TODO: the preview pane.
    pub draft_server_closers: Vec<Rc<dyn Fn(&mut App, &str, &str) -> Vec<SurfaceCloser>>>,
    /// A conversation is gone, or its id was reused: forget what's keyed by it (`terminal tabs remove_conversation`, Git
    /// snapshots, ...). What this crate holds is forgotten by [`App::forget_conversation_state`] itself.
    pub conversation_forgotten: Vec<Rc<dyn Fn(&mut App, &str)>>,
    /// `adoptDraftConversationId`: (draft id, conversation id, worktree members). What's keyed by the placeholder moves
    /// onto the real conversation; a Git snapshot of a member with a worktree must be dropped rather than carried.
    pub draft_adopted: Vec<Rc<dyn Fn(&mut App, &str, &str, &[u32])>>,
    /// `forgetDraft`: (draft id, materializes-as id). Git snapshots and terminal tabs of the draft go.
    pub draft_forgotten: Vec<Rc<dyn Fn(&mut App, &str, &str)>>,
    /// `setEditor(null)` and `setQuestionEditor(null)`.
    pub editors_closed: Vec<Rc<dyn Fn(&mut App)>>,
    /// `showTimelineNotice(null, message, undefined, ms)`.
    pub timeline_notice: Vec<Rc<dyn Fn(&mut App, &str, u64)>>,
    /// `refreshGitSnapshot` of a conversation's member, after a draft's worktree was created: (conversation id, workspace id, member).
    pub git_refresh: Vec<Rc<dyn Fn(&mut App, &str, &str, u32)>>,
}

/// `KeptWorktree`: a worktree a deletion left on disk: one still holding work (`error` is `None`), or one that could not
/// be released, with the reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeptWorktree {
    pub path: String,
    pub error: Option<String>,
}

/// What to tell the user about the worktrees a deletion left on disk, or `None` for none.
pub fn kept_worktrees_notice(kept: &[KeptWorktree]) -> Option<String> {
    let holding: Vec<&str> = kept.iter().filter(|worktree| worktree.error.is_none()).map(|worktree| worktree.path.as_str()).collect();
    let mut sentences = Vec::new();
    if let [only] = holding.as_slice() {
        sentences.push(crate::tf("工作树 {path} 里还有未提交的改动或新提交，目录与分支已保留", "The worktree {path} still has uncommitted changes or new commits, so its directory and branch were kept", &[("path", only)]));
    } else if holding.len() > 1 {
        let paths = holding.join(t("、", ", "));
        sentences.push(crate::tf("{count} 个工作树里还有未提交的改动或新提交，目录与分支已保留：{paths}", "{count} worktrees still have uncommitted changes or new commits, so their directories and branches were kept: {paths}", &[("count", &holding.len()), ("paths", &paths)]));
    }
    for worktree in kept {
        if let Some(error) = &worktree.error {
            sentences.push(crate::tf("无法释放工作树 {path}，它仍在磁盘上：{error}", "Could not release the worktree {path}, so it is still on disk: {error}", &[("path", &worktree.path), ("error", error)]));
        }
    }
    (!sentences.is_empty()).then(|| sentences.join(t("。", ". ")))
}

/// Releases the worktrees deleting these conversations leaves unused, before the records the host finds them through
/// leave the document. A failure never stops the deletion: the worktrees stay on disk and the notice says so.
pub(crate) fn release_deleted_worktrees(task: &Task, conversation_ids: &[String]) -> Vec<KeptWorktree> {
    if conversation_ids.is_empty() {
        return Vec::new();
    }
    match task.invoke::<Vec<KeptWorktree>>("release_worktrees_of_deleted_conversations", json!({ "conversationIds": conversation_ids })) {
        Ok(kept) => kept,
        Err(error) => {
            let error = if error.trim().is_empty() { t("释放失败", "release failed").to_owned() } else { error };
            vec![KeptWorktree { path: t("任务的工作树", "the task's worktree").to_owned(), error: Some(error) }]
        }
    }
}

type SaveDone = Box<dyn FnOnce(&mut App, Result<(), String>) + Send>;

#[derive(Default)]
pub(crate) struct DocStore {
    status: DocumentSaveStatus,
    revision: u64,
    debounce: Option<TimerId>,
    disposed: bool,
    /// `saveFailureReason`: the host's specific rejection, so a failed document is diagnosable.
    failure_reason: Option<String>,
    /// What of `persisted()` is ever read: which workspaces the host's document holds.
    persisted_workspaces: Option<HashSet<String>>,
    queue: Option<SerialQueue>,
    pub(crate) backend: Option<DocBackend>,
    hooks: DocHooks,
    effects_posted: bool,
    fork_title_targets: HashMap<String, String>,
    /// The language last resolved, and for which setting: `auto` resolves through a subprocess, so once.
    resolved_language: Option<(crate::model::AppLanguage, bool)>,
    conversation_templates: Vec<ConversationTemplateSummary>,
    template_error: Option<String>,
    capability_error: Option<String>,
    template_switch_prompt: Option<String>,
    deleting_conversation_ids: HashSet<String>,
    started: bool,
}

fn workspace_ids(document: &AppDocument) -> HashSet<String> {
    document.workspaces.iter().map(|workspace| workspace.id.clone()).collect()
}

/// Installs what the document's logic listens to. Call once after [`App::start`]; `root::init` is the place. What it
/// does in order is upstream's startup effect: marks the load for saving, since loading may have migrated the schema,
/// listens for push events, lists machine shells and templates, opens the first conversation or a draft, and rescans
/// capabilities, which mirror disk rather than document state.
pub fn init(app: &mut App) {
    if std::mem::replace(&mut app.store().started, true) {
        return;
    }
    app.on_push_event(|app, event| match event["type"].as_str() {
        // Surface background document-write failures immediately; recovery returns only error status to saved.
        Some("documentWriteFailure") => app.report_backend_save_result(false),
        Some("documentWriteRecovered") => app.report_backend_save_result(true),
        Some("toolContextsQuarantined") => {
            // The host saved exact local-only markers in place of these cards. The renderer still holds the
            // originals, so install the committed markers; deleting them would erase the user-visible evidence.
            let Ok(locations) = serde_json::from_value::<Vec<QuarantinedContextReplacement>>(event["contexts"].clone()) else { return };
            app.update_document_if(|doc| apply_quarantined_context_replacements(doc, &locations));
        }
        _ => {}
    });
    app.init_machine_shells();
    app.init_workspaces();
    app.refresh_conversation_templates();
    app.schedule_debounced_save();
    let first = app.doc.workspaces.first().and_then(|workspace| workspace.conversations.first().map(|conversation| (workspace.id.clone(), conversation.id.clone())));
    match first {
        Some((workspace_id, conversation_id)) => app.set_active(Some(&workspace_id), Some(&conversation_id)),
        None => {
            // A fresh installation has no last project, so it opens the temporary project's draft.
            app.set_active(None, None);
            app.open_draft_conversation(None);
        }
    }
    app.rescan_capabilities(|_, _| {});
}

/// The store.
impl App {
    pub(crate) fn store(&mut self) -> &mut DocStore {
        if self.local::<DocStore>().persisted_workspaces.is_none() {
            let held = workspace_ids(&self.doc);
            self.local::<DocStore>().persisted_workspaces = Some(held);
        }
        self.local::<DocStore>()
    }

    /// What other areas register here; see [`DocHooks`].
    pub fn doc_hooks(&mut self) -> &mut DocHooks {
        &mut self.store().hooks
    }

    /// Stands in for the host in the document's commands. Only for tests and tools.
    pub fn set_doc_backend(&mut self, backend: DocBackend) {
        self.store().backend = Some(backend);
    }

    pub(crate) fn doc_backend(&mut self) -> Option<DocBackend> {
        self.store().backend.clone()
    }

    /// `documentStore.getSaveStatus`.
    pub fn save_status(&mut self) -> DocumentSaveStatus {
        self.store().status
    }

    /// `saveFailureReason`: the host's reason for the last refused background save, gone once a save succeeds.
    pub fn save_failure_reason(&mut self) -> Option<String> {
        self.store().failure_reason.clone()
    }

    fn set_save_status(&mut self, next: DocumentSaveStatus) {
        let store = self.store();
        if store.status == next {
            return;
        }
        store.status = next;
        // Preserved only while it explains a failure.
        if next == DocumentSaveStatus::Saved {
            store.failure_reason = None;
        }
        crate::prelude::repaint();
    }

    /// `documentStore.update`: applies and synchronously publishes a change, then schedules a debounced background save.
    pub fn update_document(&mut self, f: impl FnOnce(&mut AppDocument)) {
        f(&mut self.doc);
        self.document_changed();
    }

    /// The same when the change may turn out to be none: `f` answers whether it changed anything (upstream's returning
    /// the same object), and if not, no save is scheduled.
    pub fn update_document_if(&mut self, f: impl FnOnce(&mut AppDocument) -> bool) -> bool {
        let changed = f(&mut self.doc);
        if changed {
            self.document_changed();
        }
        changed
    }

    /// What follows a change made directly to `App::doc`: the debounced save and the effects.
    pub fn document_changed(&mut self) {
        self.schedule_debounced_save();
        self.schedule_doc_effects();
        crate::prelude::repaint();
    }

    fn cancel_save_debounce(&mut self) {
        if let Some(timer) = self.store().debounce.take() {
            self.clear_timer(timer);
        }
    }

    pub(crate) fn schedule_debounced_save(&mut self) {
        if self.store().disposed {
            return;
        }
        self.cancel_save_debounce();
        let store = self.store();
        store.revision += 1;
        let revision = store.revision;
        self.set_save_status(DocumentSaveStatus::Saving);
        let timer = self.set_timer(SAVE_DEBOUNCE, move |app| {
            app.store().debounce = None;
            app.enqueue_save(false, Box::new(move |app, result| {
                if app.store().revision != revision {
                    return;
                }
                match result {
                    Ok(()) => app.set_save_status(DocumentSaveStatus::Saved),
                    Err(error) => {
                        app.set_save_status(DocumentSaveStatus::Error);
                        eprintln!("[mewrk] document save rejected: debounced: {error}");
                        app.store().failure_reason = (!error.trim().is_empty()).then_some(error);
                        crate::prelude::repaint();
                    }
                }
            }));
        });
        self.store().debounce = Some(timer);
    }

    /// `documentForPersistence` and `saveDocumentNow`'s payload. The host persists conversation bodies; return an empty
    /// read-model list to avoid overwriting them with the renderer snapshot. What upstream's context projection strips
    /// is never in the model's types to begin with.
    fn document_for_host(&mut self) -> Result<Value, String> {
        let lists: Vec<Vec<Conversation>> = self.doc.workspaces.iter_mut().map(|workspace| std::mem::take(&mut workspace.conversations)).collect();
        let document = serde_json::to_value(&self.doc).map_err(|e| format!("save_document: {e}"));
        for (workspace, conversations) in self.doc.workspaces.iter_mut().zip(lists) {
            workspace.conversations = conversations;
        }
        document
    }

    /// Serialized disk-write queue: it swallows earlier failures while preserving write order.
    fn enqueue_save(&mut self, durable: bool, done: SaveDone) {
        let snapshot = self.document_for_host();
        let ids = workspace_ids(&self.doc);
        let task = self.task();
        let backend = self.doc_backend();
        let store = self.store();
        let queue = store.queue.get_or_insert_with(|| SerialQueue::new("mewrk-document-save"));
        queue.push(Box::new(move || {
            let result = snapshot.and_then(|document| call_host(&task, backend.as_ref(), "save_document", json!({ "document": document, "durable": durable })).map(|_| ()));
            task.post(move |app| {
                if result.is_ok() {
                    // Writes are serialized, so the last one to succeed is the newest accepted.
                    app.store().persisted_workspaces = Some(ids);
                }
                done(app, result);
            });
        }));
    }

    /// `documentStore.publish`: applies a change, then immediately persists that snapshot. Keeps the optimistic state on
    /// failure and tells `done`; one ordinary background retry is scheduled, which can't loop because another failure
    /// only reports through the status.
    pub fn publish_document(&mut self, durable: bool, f: impl FnOnce(&mut AppDocument), done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
        self.cancel_save_debounce();
        let store = self.store();
        store.revision += 1;
        let revision = store.revision;
        self.set_save_status(DocumentSaveStatus::Saving);
        // Publish before I/O so later changes build on this snapshot.
        f(&mut self.doc);
        self.schedule_doc_effects();
        crate::prelude::repaint();
        self.enqueue_save(durable, Box::new(move |app, result| {
            app.settle_immediate_save(revision, result, done);
        }));
    }

    fn settle_immediate_save(&mut self, revision: u64, result: Result<(), String>, done: impl FnOnce(&mut App, Result<(), String>)) {
        let current = self.store().revision == revision;
        match &result {
            Ok(()) => {
                if current {
                    self.set_save_status(DocumentSaveStatus::Saved);
                }
            }
            Err(_) => {
                if current {
                    self.set_save_status(DocumentSaveStatus::Error);
                }
                // A cancelled debounce must not be lost when the application exits.
                self.schedule_debounced_save();
            }
        }
        done(self, result);
    }

    /// `documentStore.flush`: immediately persists the current snapshot, cancelling the pending debounce.
    pub fn flush_document_store(&mut self, durable: bool, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
        self.cancel_save_debounce();
        let store = self.store();
        store.revision += 1;
        let revision = store.revision;
        self.set_save_status(DocumentSaveStatus::Saving);
        self.enqueue_save(durable, Box::new(move |app, result| {
            app.settle_immediate_save(revision, result, done);
        }));
    }

    /// `flushLatestDocument`: flushes the authoritative snapshot, then awaits conversation commands, so user messages
    /// reach disk before a run.
    pub fn flush_latest_document(&mut self, durable: bool, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
        self.flush_document_store(durable, move |app, result| {
            app.flush_conversation_sync(move |app| done(app, result));
        });
    }

    /// `flushDocumentSaves`: durability barrier: resolves after every commit so far is on disk. Quitting and tests use it.
    pub fn flush_document_saves(&mut self, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
        let task = self.task();
        let backend = self.doc_backend();
        let store = self.store();
        let queue = store.queue.get_or_insert_with(|| SerialQueue::new("mewrk-document-save"));
        queue.push(Box::new(move || {
            let result = call_host(&task, backend.as_ref(), "flush_document_saves", json!({})).map(|_| ());
            task.post(move |app| done(app, result));
        }));
    }

    /// `persistDocumentImmediately`: publishes `f` synchronously before awaiting persistence. `changed` names the
    /// conversations `f` changes, as `conversationSync.syncDocument` found by identity. Includes conversation commands,
    /// so `done` can safely read the host's committed document.
    pub fn persist_document_immediately(&mut self, durable: bool, changed: &[(String, String)], f: impl FnOnce(&mut AppDocument), done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
        let bases: Vec<Option<Vec<String>>> = changed.iter().map(|(workspace_id, conversation_id)| self.find_conversation(workspace_id, conversation_id).map(|conversation| context_ids(conversation))).collect();
        let changed = changed.to_vec();
        self.publish_document(durable, f, move |app, result| app.flush_conversation_sync(move |app| done(app, result)));
        for ((workspace_id, conversation_id), base) in changed.iter().zip(bases) {
            self.mark_body_dirty(conversation_id);
            let Some(next) = self.find_conversation(workspace_id, conversation_id).cloned() else { continue };
            match base {
                Some(base) => self.sync_conversation_changed(workspace_id, base, next),
                None => {
                    let ordered = self.workspace(workspace_id).map(|workspace| workspace.conversations.iter().map(|conversation| conversation.id.clone()).collect()).unwrap_or_default();
                    self.sync_conversation_created(workspace_id, next, ordered);
                }
            }
        }
    }

    /// `resetDocument`, with `documentStore.load` of its answer: deletes everything and starts over. Waits for writes
    /// already queued first. Upstream only offers this from its can't-load screen.
    pub fn reset_document(&mut self, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
        let task = self.task();
        let backend = self.doc_backend();
        let store = self.store();
        let queue = store.queue.get_or_insert_with(|| SerialQueue::new("mewrk-document-save"));
        queue.push(Box::new(move || {
            let result = call_host(&task, backend.as_ref(), "reset_document", json!({})).and_then(|value| serde_json::from_value::<AppDocument>(value).map_err(|e| format!("reset_document: {e}")));
            task.post(move |app| match result {
                Ok(document) => {
                    app.doc = document;
                    app.unloaded.clear();
                    let held = workspace_ids(&app.doc);
                    app.store().persisted_workspaces = Some(held);
                    // Loading may migrate the schema, so persist its result without waiting for the first user edit.
                    app.document_changed();
                    done(app, Ok(()));
                }
                Err(error) => done(app, Err(error)),
            });
        }));
    }

    /// `reportBackendSaveResult`: failures immediately set error; successes only restore error to saved without interrupting saving.
    pub fn report_backend_save_result(&mut self, success: bool) {
        if !success {
            self.set_save_status(DocumentSaveStatus::Error);
        } else if self.store().status == DocumentSaveStatus::Error {
            self.set_save_status(DocumentSaveStatus::Saved);
        }
    }

    /// `documentStore.dispose`: cancels the pending debounce and stops scheduling background saves. Writes already asked for are unaffected.
    pub fn dispose_document_store(&mut self) {
        self.store().disposed = true;
        self.cancel_save_debounce();
    }

    /// `documentStore.resume`: re-enables scheduling, and schedules a save for changes made while it was off.
    pub fn resume_document_store(&mut self) {
        if std::mem::replace(&mut self.store().disposed, false) {
            self.schedule_debounced_save();
        }
    }

    /// Whether the document the host holds has this workspace yet.
    pub(crate) fn host_holds_workspace(&mut self, workspace_id: &str) -> bool {
        self.store().persisted_workspaces.as_ref().is_some_and(|held| held.contains(workspace_id))
    }

    /// `awaitWorkspaceAtHost`: resolves once the host's document holds `workspace_id`, sending the pending document save
    /// first when it does not. A project added in the same event reaches the host through the debounced save, later than
    /// a command that names it would; the host refuses what it cannot resolve.
    pub fn await_workspace_at_host(&mut self, workspace_id: &str, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
        if self.host_holds_workspace(workspace_id) {
            return done(self, Ok(()));
        }
        self.flush_document_store(false, done);
    }

    /// Asks for [`run_doc_effects`](Self::run_doc_effects) on the next tick: what upstream's `useEffect`s did after a
    /// render. Several changes in one tick run them once.
    pub fn schedule_doc_effects(&mut self) {
        if !std::mem::replace(&mut self.store().effects_posted, true) {
            self.post(|app| {
                app.store().effects_posted = false;
                app.run_doc_effects();
            });
        }
    }

    fn run_doc_effects(&mut self) {
        self.mirror_resolved_language();
        self.follow_fork_titles();
        self.keep_draft_settings_in_document();
        self.record_default_agent_shells();
        self.selection_effects();
    }

    /// The host cannot resolve `auto` without an OS language library, so mirror the resolved application language into
    /// the document for built-in tool descriptions.
    fn mirror_resolved_language(&mut self) {
        let setting = self.doc.global_settings.app_language;
        let known = self.store().resolved_language;
        let chinese = match known {
            Some((language, chinese)) if language == setting => chinese,
            // `App::start` resolved it for the setting it loaded.
            None => !crate::i18n::is_english(),
            Some(_) => {
                let chinese = self.language_is_chinese();
                crate::i18n::set_english(!chinese);
                chinese
            }
        };
        self.store().resolved_language = Some((setting, chinese));
        let resolved = if chinese { ResolvedLanguage::ZhCn } else { ResolvedLanguage::EnUs };
        if self.doc.global_settings.resolved_app_language != resolved {
            self.update_document(|doc| doc.global_settings.resolved_app_language = resolved);
        }
    }

    /// Forks follow their origin's title however it changed: a rename here, the first message's excerpt, the local
    /// helper model's title, a language switch. A title is written once per change of target: a commit the host refuses
    /// comes back as the old title, and writing the same target again would loop.
    fn follow_fork_titles(&mut self) {
        for update in crate::conversation_forks::stale_fork_titles(&self.doc) {
            let targets = &mut self.store().fork_title_targets;
            if targets.get(&update.conversation_id) == Some(&update.title) {
                continue;
            }
            targets.insert(update.conversation_id.clone(), update.title.clone());
            self.update_conversation(&update.workspace_id, &update.conversation_id, |conversation| conversation.title = update.title);
        }
    }
}

impl Task {
    fn wait_for_store(&self, ask: impl FnOnce(&mut App, Box<dyn FnOnce(&mut App, Result<(), String>) + Send>) + Send + 'static) -> Result<(), String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(move |app| {
            ask(app, Box::new(move |_, result| {
                let _ = tx.send(result);
            }));
        });
        rx.recv().unwrap_or_else(|_| Err("the app is closing".to_owned()))
    }

    /// [`App::flush_document_store`], for a task thread.
    pub fn flush_document_store(&self, durable: bool) -> Result<(), String> {
        self.wait_for_store(move |app, done| app.flush_document_store(durable, done))
    }

    /// [`App::flush_latest_document`], for a task thread.
    pub fn flush_latest_document(&self, durable: bool) -> Result<(), String> {
        self.wait_for_store(move |app, done| app.flush_latest_document(durable, done))
    }

    /// [`App::flush_document_saves`], for a task thread.
    pub fn flush_document_saves(&self) -> Result<(), String> {
        self.wait_for_store(|app, done| app.flush_document_saves(done))
    }

    /// [`App::persist_document_immediately`], for a task thread.
    pub fn persist_document_immediately(&self, durable: bool, changed: Vec<(String, String)>, f: impl FnOnce(&mut AppDocument) + Send + 'static) -> Result<(), String> {
        self.wait_for_store(move |app, done| app.persist_document_immediately(durable, &changed, f, done))
    }
}

fn context_ids(conversation: &Conversation) -> Vec<String> {
    conversation.contexts.iter().map(|context| context.id().to_owned()).collect()
}

/// `findConversation`.
pub fn find_conversation<'a>(document: &'a AppDocument, workspace_id: &str, conversation_id: &str) -> Option<&'a Conversation> {
    document.workspaces.iter().find(|workspace| workspace.id == workspace_id)?.conversations.iter().find(|conversation| conversation.id == conversation_id)
}

/// `modelChoiceForConversation` and `modelChoiceOf`: the document-wide provider and model choice; conversations do not
/// own model selections. One spelling of the rule, so surfaces can never disagree.
pub fn model_choice(document: &AppDocument) -> (Option<&ApiProvider>, Option<&ModelProfile>) {
    let active = document.global_settings.active_provider_id.as_deref();
    let provider = document.assets.api_providers.iter().find(|provider| Some(provider.id.as_str()) == active);
    let model = provider.and_then(|provider| provider.models.iter().find(|model| Some(model.id.as_str()) == provider.active_model_id.as_deref()));
    (provider, model)
}

/// `applyGlobalSettingsChange`: applies a change to the global settings. Presets are templates: editing or deleting one
/// never propagates to conversations. The default preset is a real setting and cannot dangle.
pub fn apply_global_settings_change(document: &mut AppDocument, change: impl FnOnce(&mut AppDocument)) {
    change(document);
    let presets = &mut document.presets;
    if !presets.conversation_presets.iter().any(|preset| preset.id == presets.default_conversation_preset_id) {
        presets.default_conversation_preset_id = presets.conversation_presets.first().map(|preset| preset.id.clone()).unwrap_or_default();
    }
}

/// One card the host could not attest, with the exact local-only marker it committed in its place.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuarantinedContextReplacement {
    pub conversation_id: String,
    pub context_id: String,
    pub replacement: ContextItem,
}

/// `applyQuarantinedContextReplacements`: replaces the addressed contexts with the markers committed by the host,
/// including transcripts nested inside subagent records. Installing rather than deleting keeps the visible audit
/// evidence and prevents the next save from offering the card again. `false` when nothing matched, so no save is scheduled.
pub fn apply_quarantined_context_replacements(document: &mut AppDocument, locations: &[QuarantinedContextReplacement]) -> bool {
    fn replace(contexts: &mut [ContextItem], replacements: &HashMap<&str, &ContextItem>) -> bool {
        let mut changed = false;
        for context in contexts {
            if let Some(replacement) = replacements.get(context.id()) {
                *context = (*replacement).clone();
                changed = true;
            } else if let ContextItem::Tool { subagent: Some(subagent), .. } = context {
                changed |= replace(&mut subagent.contexts, replacements);
            }
        }
        changed
    }
    if locations.is_empty() {
        return false;
    }
    let mut by_conversation: HashMap<&str, HashMap<&str, &ContextItem>> = HashMap::new();
    for location in locations {
        by_conversation.entry(&location.conversation_id).or_default().insert(&location.context_id, &location.replacement);
    }
    let mut changed = false;
    for conversation in document.workspaces.iter_mut().flat_map(|workspace| workspace.conversations.iter_mut()) {
        let Some(replacements) = by_conversation.get(conversation.id.as_str()) else { continue };
        changed |= replace(&mut conversation.contexts, replacements);
        for branch in &mut conversation.branches {
            changed |= replace(&mut branch.contexts, replacements);
        }
    }
    changed
}

/// What creating a conversation may differ by: the optional parameters of `createConversation`.
#[derive(Clone, Debug, Default)]
pub struct NewConversation {
    /// Where; the active project, then the first, when absent or unknown.
    pub workspace_id: Option<String>,
    /// `None` is `global`.
    pub source: Option<NewConversationSource>,
    /// Settings to copy instead of resolving them.
    pub settings_override: Option<ConversationSettings>,
    pub run_target_override: Option<RunTarget>,
    pub parent_conversation_id: Option<String>,
    /// A materialized draft brings the content the user wrote before sending. Seeding it here rather than writing it
    /// afterwards lets the create command carry it to the host in one piece.
    pub initial_contexts: Vec<ContextItem>,
    /// Preset the overridden settings came from; only a materialized draft supplies it.
    pub preset_id_override: String,
    /// Template whose queue the draft was showing; only a materialized draft supplies it.
    pub template_id_override: String,
    /// Workspaces the draft was granted; only a materialized draft supplies them.
    pub attached_workspaces_override: Vec<AttachedWorkspace>,
    /// The id a materialized draft was minted with, which its terminals are already open under.
    pub conversation_id_override: Option<String>,
    /// A timeline fork's name and origin; see `conversation_forks.rs`.
    pub fork: Option<(String, ConversationForkOrigin)>,
    /// The source's lock, for a branch or fork that carries history over: the cache that history rides on is the
    /// source's, and it runs out at the same moment in both.
    pub inherited_tool_lock: Option<ConversationToolLock>,
    /// The source's isolated worktrees, for a fork that carries its work on.
    pub shared_worktrees: Vec<ConversationWorktree>,
}

/// `ConversationTemplateSummary`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationTemplateSummary {
    pub id: String,
    pub name: String,
    pub message_count: u64,
    pub created_at: String,
    pub updated_at: String,
}

/// Lookups.
impl App {
    /// `findConversation(document, workspaceId, conversationId).conversation`.
    pub fn find_conversation(&self, workspace_id: &str, conversation_id: &str) -> Option<&Conversation> {
        find_conversation(&self.doc, workspace_id, conversation_id)
    }

    /// A conversation by id alone, wherever it is.
    pub fn conversation(&self, conversation_id: &str) -> Option<&Conversation> {
        self.doc.workspaces.iter().flat_map(|workspace| workspace.conversations.iter()).find(|conversation| conversation.id == conversation_id)
    }

    /// The workspace holding a conversation.
    pub fn workspace_of_conversation(&self, conversation_id: &str) -> Option<&Workspace> {
        self.doc.workspaces.iter().find(|workspace| workspace.conversations.iter().any(|conversation| conversation.id == conversation_id))
    }
}

/// Blocking guards.
impl App {
    /// `conversationOperationIsActive`: a run is starting or going, or a Git write holds the conversation.
    pub fn conversation_operation_is_active(&mut self, conversation_id: &str) -> bool {
        let controller = &self.runs().controller;
        if controller.has_run_token(conversation_id) || controller.has_performing_run(conversation_id) || controller.has_preparing_run(conversation_id) {
            return true;
        }
        let hooks = self.store().hooks.git_mutation_active.clone();
        hooks.iter().any(|active| active(self, conversation_id))
    }

    /// `conversationIsInLockedWorkspace`: its project is being deleted.
    pub fn conversation_is_in_locked_workspace(&mut self, conversation_id: &str) -> bool {
        let Some(workspace_id) = self.workspace_of_conversation(conversation_id).map(|workspace| workspace.id.clone()) else { return false };
        self.workspace_is_deleting(&workspace_id)
    }

    /// `contextMutationIsBlocked`.
    pub fn context_mutation_is_blocked(&mut self, conversation_id: &str) -> bool {
        self.conversation_operation_is_active(conversation_id) || self.conversation_is_in_locked_workspace(conversation_id)
    }

    /// `conversationIsBusy`.
    pub fn conversation_is_busy(&mut self, conversation_id: &str) -> bool {
        self.runs().controller.run(conversation_id).is_some() || self.context_mutation_is_blocked(conversation_id)
    }

    /// `deletingConversationIds.has`.
    pub fn conversation_is_deleting(&mut self, conversation_id: &str) -> bool {
        self.store().deleting_conversation_ids.contains(conversation_id)
    }

    /// `browserSessionDeletionIsActive`: the conversation, or its project, is being deleted.
    pub fn conversation_deletion_is_active(&mut self, conversation_id: &str) -> bool {
        self.conversation_is_deleting(conversation_id) || self.conversation_is_in_locked_workspace(conversation_id)
    }

    /// `conversationMoveIsBlocked`.
    pub fn conversation_move_is_blocked(&mut self, conversation_id: &str, source_workspace_id: &str, target_workspace_id: &str) -> bool {
        self.conversation_is_deleting(conversation_id) || self.workspace_is_deleting(source_workspace_id) || self.workspace_is_deleting(target_workspace_id)
    }

    /// `setEditor(null)`.
    pub(crate) fn close_editors(&mut self) {
        let hooks = self.store().hooks.editors_closed.clone();
        for close in hooks {
            close(self);
        }
    }

    pub(crate) fn report_kept_worktrees(&mut self, kept: Vec<KeptWorktree>) {
        let Some(notice) = kept_worktrees_notice(&kept) else { return };
        let hooks = self.store().hooks.timeline_notice.clone();
        for show in hooks {
            show(self, &notice, KEPT_WORKTREE_NOTICE_MS);
        }
    }

    pub(crate) fn conversation_surface_closers(&mut self, conversation_ids: &[String]) -> Vec<SurfaceCloser> {
        let hooks = self.store().hooks.surface_closers.clone();
        let mut closers = Vec::new();
        for id in conversation_ids {
            for closers_of in &hooks {
                closers.extend(closers_of(self, id));
            }
        }
        closers
    }

    /// Forgets everything kept by a conversation's id: pane layout, composer text, timeline history, run errors.
    pub(crate) fn forget_conversation_state(&mut self, conversation_id: &str) {
        self.dispatch_side_panes(crate::side_panes::SidePanesAction::RemoveConversation { conversation_id: conversation_id.to_owned() });
        self.timeline_history().forget(conversation_id);
        let runs = self.runs();
        runs.composer.drafts.remove(conversation_id);
        runs.composer.pasted_texts.remove(conversation_id);
        runs.model_run_errors.remove(conversation_id);
        self.forget_selection_of(conversation_id);
        let hooks = self.store().hooks.conversation_forgotten.clone();
        for forget in hooks {
            forget(self, conversation_id);
        }
    }

    /// `setContextUsage` with `estimateActiveContextUsage`, once a body changed under a conversation not running.
    fn refresh_context_usage(&mut self, conversation_id: &str) {
        let Some(conversation) = self.conversation(conversation_id) else { return };
        let (provider, model) = model_choice(&self.doc);
        let target = provider.filter(|provider| provider.enabled).zip(model);
        let usage = crate::runs::ContextUsage {
            tokens: crate::runs::estimate_wire_tokens(crate::runs::wire_view(&conversation.contexts, target)),
            estimated: true,
            unprojectable: false,
            provider_id: target.map(|(provider, _)| provider.id.clone()),
            family: target.map(|(provider, _)| provider.family),
        };
        self.runs().context_usage.insert(conversation_id.to_owned(), usage);
    }
}

/// Conversation commands.
impl App {
    fn update_conversation_inner(&mut self, workspace_id: &str, conversation_id: &str, persist: bool, f: impl FnOnce(&mut Conversation) -> bool) -> bool {
        let unloaded = self.unloaded.contains(conversation_id);
        let Some(workspace) = self.doc.workspaces.iter_mut().find(|workspace| workspace.id == workspace_id) else { return false };
        let Some(conversation) = workspace.conversations.iter_mut().find(|conversation| conversation.id == conversation_id) else { return false };
        let base = persist.then(|| context_ids(conversation));
        let settings_before = conversation.settings.clone();
        if !f(conversation) {
            return false;
        }
        // An unloaded body is an empty stand-in: an updater that read it keeps its metadata
        // changes only, never a body derived from nothing.
        keep_unloaded_body(unloaded, conversation);
        // The workspace's last settings follow only a settings change; streaming context updates preserve them.
        if conversation.settings != settings_before {
            workspace.last_conversation_settings = Some(conversation.settings.clone());
        }
        let next = persist.then(|| conversation.clone());
        self.mark_body_dirty(conversation_id);
        self.document_changed();
        if let (Some(base), Some(next)) = (base, next) {
            self.sync_conversation_changed(workspace_id, base, next);
        }
        true
    }

    /// `updateConversation`: changes a conversation and writes it back to the host (debounced).
    pub fn update_conversation(&mut self, workspace_id: &str, conversation_id: &str, f: impl FnOnce(&mut Conversation)) {
        self.update_conversation_inner(workspace_id, conversation_id, true, |conversation| {
            f(conversation);
            true
        });
    }

    /// The same when `f` may change nothing: it answers whether it did. Upstream's updater returning its argument.
    pub fn update_conversation_if(&mut self, workspace_id: &str, conversation_id: &str, f: impl FnOnce(&mut Conversation) -> bool) -> bool {
        self.update_conversation_inner(workspace_id, conversation_id, true, f)
    }

    /// `updateConversation(..., { persist: false })`: host-produced contexts are read-model updates only.
    pub fn update_conversation_local(&mut self, workspace_id: &str, conversation_id: &str, f: impl FnOnce(&mut Conversation) -> bool) -> bool {
        self.update_conversation_inner(workspace_id, conversation_id, false, f)
    }

    /// `renameConversation`. A fork the user names stops following its origin's title, and a name the user chose is
    /// final: the local helper model must not replace it.
    pub fn rename_conversation(&mut self, workspace_id: &str, conversation_id: &str, title: &str) {
        let title = title.to_owned();
        self.update_conversation(workspace_id, conversation_id, |conversation| {
            conversation.title = title;
            conversation.fork_of = None;
            conversation.updated_at = now_iso();
        });
        self.settle_conversation_title(conversation_id);
    }

    /// `createConversation`: makes a conversation at the top of its project and opens it. `None` when there's no such
    /// project, or the project is being deleted.
    pub fn create_conversation(&mut self, options: NewConversation) -> Option<String> {
        // The store, not a rendered snapshot: a workspace registered in this same event is in `doc` already.
        let requested = options.workspace_id.clone().or_else(|| self.active_workspace_id());
        let target = requested.as_deref().and_then(|id| self.workspace(id)).or_else(|| self.doc.workspaces.first())?;
        let target_id = target.id.clone();
        if self.workspace_is_deleting(&target_id) {
            return None;
        }
        let known_tool_names: HashSet<String> = self.doc.tools.iter().map(|tool| tool.name.clone()).collect();
        let now = now_iso();
        let materializing = options.conversation_id_override.is_some();
        let conversation_id = options.conversation_id_override.clone().unwrap_or_else(|| create_id("conv"));
        // Materializing a draft preserves its exact settings; other creation paths resolve them now.
        let (mut settings, preset_id) = match &options.settings_override {
            Some(settings) => (crate::presets::clone_conversation_settings(settings, Some(&known_tool_names)), options.preset_id_override.clone()),
            None => self.resolve_new_conversation_settings(&target_id, options.source.unwrap_or(NewConversationSource::Global))?,
        };
        // A new task that has not chosen how to auto-compact chooses by its model: native where the model compacts
        // natively. A branch or fork keeps its source's choice, or the lack of one.
        if settings.compaction_method.is_none() && options.parent_conversation_id.is_none() && options.fork.is_none() {
            let (provider, model) = model_choice(&self.doc);
            settings.compaction_method = Some(crate::auto_compact::default_compaction_method(provider.zip(model)));
        }
        if let Some(lock) = options.inherited_tool_lock {
            settings.tool_lock = Some(lock);
        }
        let mut conversation: Conversation = serde_json::from_value(json!({ "id": conversation_id, "title": "", "createdAt": now, "updatedAt": now, "settings": { "enabledTools": [] }, "contexts": [] })).ok()?;
        conversation.settings = settings.clone();
        conversation.contexts = options.initial_contexts;
        conversation.worktrees = options.shared_worktrees;
        conversation.run_target = options.run_target_override;
        conversation.attached_workspaces = options.attached_workspaces_override;
        conversation.parent_conversation_id = options.parent_conversation_id.clone();
        conversation.preset_id = preset_id;
        // Only a materialized draft carries one: every other creation path has never had a template applied to it.
        conversation.template_id = options.template_id_override;
        match options.fork {
            Some((title, fork_of)) => {
                conversation.title = title;
                conversation.fork_of = Some(fork_of);
            }
            None => conversation.title = t("新任务", "New task").to_owned(),
        }
        let is_branch = options.parent_conversation_id.is_some();
        let created = conversation.clone();
        self.update_document(|doc| {
            let Some(workspace) = doc.workspaces.iter_mut().find(|workspace| workspace.id == target_id) else { return };
            // A new task becomes the workspace's remembered settings snapshot. A branch or fork only copies an existing
            // conversation's, which may be older than what the user last chose for a new task.
            if !is_branch {
                workspace.last_conversation_settings = Some(settings);
            }
            workspace.conversations.insert(0, conversation);
        });
        let ordered: Vec<String> = self.workspace(&target_id).map(|workspace| workspace.conversations.iter().map(|item| item.id.clone()).collect()).unwrap_or_default();
        self.sync_conversation_created(&target_id, created, ordered);
        self.set_active(Some(&target_id), Some(&conversation_id));
        // A brand-new conversation starts on its own timeline; the per-conversation view state means this only has to
        // clear a stale entry left by a conversation id that was reused. A draft's terminal tabs are already filed under
        // the id it materializes as, and are its own.
        self.dispatch_side_panes(crate::side_panes::SidePanesAction::RemoveConversation { conversation_id: conversation_id.clone() });
        if !materializing {
            let hooks = self.store().hooks.conversation_forgotten.clone();
            for forget in hooks {
                forget(self, &conversation_id);
            }
        }
        Some(conversation_id)
    }

    /// `reorderSidebarConversation`. Only the order within the project changes, so a running conversation moves as
    /// freely as an idle one.
    pub fn reorder_sidebar_conversation(&mut self, workspace_id: &str, conversation_id: &str, target_conversation_id: &str, after: bool) {
        if conversation_id == target_conversation_id {
            return;
        }
        let moved = self.update_document_if(|doc| {
            let Some(workspace) = doc.workspaces.iter_mut().find(|item| item.id == workspace_id) else { return false };
            reorder_items(&mut workspace.conversations, conversation_id, target_conversation_id, after, |conversation| &conversation.id)
        });
        if moved {
            let ordered: Vec<String> = self.workspace(workspace_id).map(|workspace| workspace.conversations.iter().map(|conversation| conversation.id.clone()).collect()).unwrap_or_default();
            self.sync_conversations_reordered(workspace_id, ordered);
        }
    }

    /// `moveActiveConversation`: moves the conversation on screen to another project, which only one with nothing in it
    /// can do. Its terminals are closed first. `done` answers whether it moved.
    pub fn move_active_conversation(&mut self, target_workspace_id: &str, done: impl FnOnce(&mut App, bool) + Send + 'static) {
        let (Some(source), Some(moving)) = (self.active_workspace_id(), self.active_conversation_id()) else { return done(self, false) };
        let movable = self.find_conversation(&source, &moving).is_some_and(|conversation| self.conversation_has_no_contexts(conversation));
        if !movable || target_workspace_id == source || self.conversation_move_is_blocked(&moving, &source, target_workspace_id) {
            return done(self, false);
        }
        let hooks = self.store().hooks.terminal_closers.clone();
        let closers: Vec<SurfaceCloser> = hooks.iter().flat_map(|closers_of| closers_of(self, &moving)).collect();
        let target = target_workspace_id.to_owned();
        self.spawn(move |task| {
            let closed = run_all(closers, &task).iter().all(Result::is_ok);
            task.post(move |app| {
                let moved = closed && app.finish_move_conversation(&source, &moving, &target);
                done(app, moved);
            });
        });
    }

    fn finish_move_conversation(&mut self, source: &str, moving: &str, target: &str) -> bool {
        if self.conversation_move_is_blocked(moving, source, target) || self.active_conversation_id().as_deref() != Some(moving) {
            return false;
        }
        let movable = self.find_conversation(source, moving).is_some_and(|conversation| self.conversation_has_no_contexts(conversation));
        if !movable || self.workspace(target).is_none() {
            return false;
        }
        self.update_document(|doc| {
            let Some(from) = doc.workspaces.iter_mut().find(|workspace| workspace.id == source) else { return };
            let Some(index) = from.conversations.iter().position(|conversation| conversation.id == moving) else { return };
            let moved = from.conversations.remove(index);
            detach_absent_parents(&mut from.conversations);
            let Some(to) = doc.workspaces.iter_mut().find(|workspace| workspace.id == target) else { return };
            to.conversations.insert(0, moved);
            detach_absent_parents(&mut to.conversations);
        });
        // Reordering the destination workspace also updates the host-side conversation ownership.
        let ordered: Vec<String> = self.workspace(target).map(|workspace| workspace.conversations.iter().map(|conversation| conversation.id.clone()).collect()).unwrap_or_default();
        self.sync_conversations_reordered(target, ordered);
        self.set_active(Some(target), Some(moving));
        self.close_editors();
        true
    }

    /// `deleteConversation`. Refused while it's busy or already being deleted. Closes its surfaces and releases its
    /// worktrees while the host's saved document still records them, then removes it; the checks repeat after each wait.
    pub fn delete_conversation(&mut self, workspace_id: &str, conversation_id: &str) {
        if self.conversation_is_busy(conversation_id) || self.conversation_is_deleting(conversation_id) {
            return;
        }
        self.store().deleting_conversation_ids.insert(conversation_id.to_owned());
        crate::prelude::repaint();
        let closers = self.conversation_surface_closers(std::slice::from_ref(&conversation_id.to_owned()));
        let workspace_id = workspace_id.to_owned();
        let conversation_id = conversation_id.to_owned();
        self.spawn(move |task| {
            let finish = |id: String| {
                task.post(move |app| {
                    app.store().deleting_conversation_ids.remove(&id);
                    crate::prelude::repaint();
                });
            };
            let closed = run_all(closers, &task).iter().all(Result::is_ok);
            let check = conversation_id.clone();
            if !closed || task.update(move |app| app.conversation_is_busy(&check)) {
                return finish(conversation_id);
            }
            // Its worktrees go first, while the host's saved document still records them.
            let kept = release_deleted_worktrees(&task, std::slice::from_ref(&conversation_id));
            let (workspace, id) = (workspace_id, conversation_id.clone());
            task.update(move |app| app.finish_delete_conversation(&workspace, &id, kept));
            finish(conversation_id);
        });
    }

    fn finish_delete_conversation(&mut self, workspace_id: &str, conversation_id: &str, kept: Vec<KeptWorktree>) {
        let Some(queued) = self.find_conversation(workspace_id, conversation_id).map(|conversation| conversation.queued_messages.iter().map(|message| message.id.clone()).collect::<Vec<_>>()) else { return };
        if self.conversation_is_busy(conversation_id) {
            return;
        }
        self.invalidate_composer_images(std::slice::from_ref(&conversation_id.to_owned()));
        self.composer().forget_queued_messages(&queued);
        // Children move up to the deleted conversation's parent, mirroring what the host store does on delete; the host
        // never re-sends them, so the renderer's copy has to make the same move itself.
        self.update_document(|doc| {
            let Some(workspace) = doc.workspaces.iter_mut().find(|item| item.id == workspace_id) else { return };
            reparent_children(&mut workspace.conversations, conversation_id);
            workspace.conversations.retain(|item| item.id != conversation_id);
        });
        self.unloaded.remove(conversation_id);
        self.sync_conversation_deleted(workspace_id, conversation_id);
        self.forget_conversation_state(conversation_id);
        if self.active_workspace_id().as_deref() == Some(workspace_id) && self.active_conversation_id().as_deref() == Some(conversation_id) {
            // After deleting the active conversation, open a draft in the same workspace rather than an unrelated list neighbor.
            self.open_draft_conversation(Some(workspace_id));
            self.close_editors();
        }
        self.report_kept_worktrees(kept);
    }

    /// `handleGlobalSettingsChange`: evaluated against the current settings; a default preset that disappeared moves to what's left.
    pub fn update_global_settings(&mut self, change: impl FnOnce(&mut AppDocument)) {
        self.update_document(|doc| apply_global_settings_change(doc, change));
    }

    /// `rescanCapabilities`: re-reads the capability catalog from disk and publishes it. The catalog mirrors disk rather
    /// than document state, so every surface that can have invalidated it calls this. A failure keeps the previous
    /// snapshot, because a temporarily unreadable directory is not a reason to drop rows the user is looking at.
    pub fn rescan_capabilities(&mut self, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
        self.invoke::<crate::model::CapabilityCatalog>("discover_capabilities", json!({}), move |app, result| match result {
            Ok(catalog) => {
                app.update_document(|doc| doc.capabilities = catalog);
                done(app, Ok(()));
            }
            Err(error) => done(app, Err(error)),
        });
    }
}

/// Branches and forks.
impl App {
    /// `branchFromUserContext`: starts an independent conversation that owns a full copy of the history preceding the
    /// branch point. The branched user message goes to the new composer as a draft, so the branch only begins once it's
    /// sent, and the original is never truncated. The host performs the copy: every copied tool result needs an execution
    /// receipt re-issued for its new owner. The branch nests under its source and keeps its settings and attached
    /// workspaces, so a workspace number in the copied history still names the same directory.
    pub fn branch_from_user_context(&mut self, item: &ContextItem) {
        let (Some(workspace_id), Some(conversation_id)) = (self.active_workspace_id(), self.active_conversation_id()) else { return };
        if !matches!(item, ContextItem::User { .. }) {
            return;
        }
        let Some(source) = self.find_conversation(&workspace_id, &conversation_id) else { return };
        let Some(fork_index) = source.contexts.iter().position(|context| context.id() == item.id()) else { return };
        let through = fork_index.checked_sub(1).map(|index| source.contexts[index].id().to_owned());
        let options = NewConversation {
            workspace_id: Some(workspace_id.clone()),
            settings_override: Some(source.settings.clone()),
            parent_conversation_id: Some(conversation_id.clone()),
            preset_id_override: source.preset_id.clone(),
            attached_workspaces_override: source.attached_workspaces.clone(),
            // A branch with nothing before it carries no history, and so no cache.
            inherited_tool_lock: if fork_index > 0 { source.settings.tool_lock.clone() } else { None },
            ..NewConversation::default()
        };
        // Created before any wait, so nothing in between sees a stale list.
        let Some(created) = self.create_conversation(options) else { return };
        self.put_message_in_composer(&created, item);
        // Everything before the branch point carries over. The branched message stays in the composer instead.
        let Some(through) = through else { return };
        self.copy_history_into(workspace_id, conversation_id, created, through, false);
    }

    /// `forkConversationAt`: forks the active conversation at a gap in its timeline: everything above the insertion line
    /// is copied into a new conversation, which opens. Unlike a branch nothing is handed to the composer and it keeps the
    /// source's settings, attached workspaces and worktrees, carrying on that work. Named after its origin, and the name
    /// is settled at once so the local helper model never retitles it.
    pub fn fork_conversation_at(&mut self, index: usize) {
        let (Some(workspace_id), Some(conversation_id)) = (self.active_workspace_id(), self.active_conversation_id()) else { return };
        if index == 0 {
            return;
        }
        let Some(source) = self.find_conversation(&workspace_id, &conversation_id) else { return };
        let Some(through) = source.contexts.get(index.min(source.contexts.len()).wrapping_sub(1)).map(|context| context.id().to_owned()) else { return };
        let origin = crate::conversation_forks::fork_origin(&self.doc, source);
        let number = crate::conversation_forks::next_fork_number(&self.doc, &origin.conversation_id);
        let options = NewConversation {
            workspace_id: Some(workspace_id.clone()),
            settings_override: Some(source.settings.clone()),
            // It nests under the source's root in the sidebar.
            parent_conversation_id: Some(source.parent_conversation_id.clone().unwrap_or_else(|| source.id.clone())),
            preset_id_override: source.preset_id.clone(),
            attached_workspaces_override: source.attached_workspaces.clone(),
            fork: Some((crate::conversation_forks::fork_title(&origin.title, number), ConversationForkOrigin { conversation_id: origin.conversation_id, number })),
            inherited_tool_lock: source.settings.tool_lock.clone(),
            shared_worktrees: source.worktrees.clone(),
            ..NewConversation::default()
        };
        let Some(created) = self.create_conversation(options) else { return };
        self.copy_history_into(workspace_id, conversation_id, created, through, true);
    }

    fn copy_history_into(&mut self, workspace_id: String, source_id: String, target_id: String, through_id: String, settle_title: bool) {
        let backend = self.doc_backend();
        self.spawn(move |task| {
            // The host copies from its own committed document, so the new conversation and any pending edit must be on
            // disk before it reads. A branch that couldn't copy still exists; the user can branch again.
            let copied = task.flush_latest_document(false).and_then(|()| {
                if settle_title {
                    let target = target_id.clone();
                    task.post(move |app| app.settle_conversation_title(&target));
                }
                call_host(&task, backend.as_ref(), "fork_conversation_contexts", json!({ "workspaceId": workspace_id, "sourceConversationId": source_id, "targetConversationId": target_id, "throughContextId": through_id }))
            }).and_then(|value| serde_json::from_value::<Vec<ContextItem>>(value).map_err(|e| format!("fork_conversation_contexts: {e}")));
            let contexts = match copied {
                Ok(contexts) => contexts,
                Err(error) => return eprintln!("Could not copy the forked history: {error}"),
            };
            // Receipts issued by the fork are consumed by this save, so it must not wait for the debounce.
            let changed = vec![(workspace_id.clone(), target_id.clone())];
            let (workspace, target) = (workspace_id.clone(), target_id.clone());
            let persisted = task.persist_document_immediately(false, changed, move |doc| {
                let Some(conversation) = doc.workspaces.iter_mut().find(|item| item.id == workspace).and_then(|item| item.conversations.iter_mut().find(|item| item.id == target)) else { return };
                conversation.contexts = contexts;
                conversation.updated_at = now_iso();
            });
            if let Err(error) = persisted {
                return eprintln!("Could not copy the forked history: {error}");
            }
            task.post(move |app| app.refresh_context_usage(&target_id));
        });
    }

    /// `selectConversationBranch`: swaps the timeline to another branch at a fork message and persists it at once.
    /// Refused while something is active in the conversation. The edits on record were made to the branch that is leaving
    /// the screen, so its timeline history is forgotten: undoing one would reach into the branch arriving.
    pub fn select_conversation_branch(&mut self, fork_context_id: &str, branch_id: &str) {
        let (Some(workspace_id), Some(conversation_id)) = (self.active_workspace_id(), self.active_conversation_id()) else { return };
        if self.context_mutation_is_blocked(&conversation_id) {
            return;
        }
        let now = now_iso();
        let Some(mut switched) = self.find_conversation(&workspace_id, &conversation_id).cloned() else { return };
        if !crate::conversation_branches::switch_conversation_branch(&mut switched, fork_context_id, branch_id, &now) {
            return;
        }
        let key = self.host_conversation_id(&conversation_id);
        self.timeline_history().forget(&key);
        self.runs().controller.add_preparing_run(&conversation_id);
        let changed = vec![(workspace_id.clone(), conversation_id.clone())];
        let (workspace, conversation) = (workspace_id, conversation_id.clone());
        self.persist_document_immediately(false, &changed, move |doc| {
            if let Some(slot) = doc.workspaces.iter_mut().find(|item| item.id == workspace).and_then(|item| item.conversations.iter_mut().find(|item| item.id == conversation)) {
                *slot = switched;
            }
        }, move |app, result| {
            // On failure the branch changed locally; the store retries persistence and the title bar surfaces it.
            if result.is_ok() {
                app.close_editors();
                app.refresh_context_usage(&conversation_id);
                app.runs().model_run_errors.remove(&conversation_id);
            }
            app.runs().controller.delete_preparing_run(&conversation_id);
            crate::prelude::repaint();
        });
    }
}

/// Conversation templates. The host owns template bodies, so this holds only the list the picker draws and refreshes it
/// after every mutation. Nothing here sends a body except an editor's `write_template_body`.
impl App {
    /// `conversationTemplates`: the list the picker draws.
    pub fn conversation_templates(&mut self) -> Vec<ConversationTemplateSummary> {
        self.store().conversation_templates.clone()
    }

    /// `templateError`.
    pub fn template_error(&mut self) -> Option<String> {
        self.store().template_error.clone()
    }

    /// `setTemplateError(null)`.
    pub fn clear_template_error(&mut self) {
        self.store().template_error = None;
    }

    /// `capabilityError`: kept apart so a failed skill delete is reported on the skills page.
    pub fn capability_error(&mut self) -> Option<String> {
        self.store().capability_error.clone()
    }

    /// `setCapabilityError`.
    pub fn set_capability_error(&mut self, error: Option<String>) {
        self.store().capability_error = error;
        crate::prelude::repaint();
    }

    /// `templateSwitchPrompt`: the preset whose queue is waiting for the user to confirm replacing the timeline.
    pub fn template_switch_prompt(&mut self) -> Option<String> {
        self.store().template_switch_prompt.clone()
    }

    /// `setTemplateSwitchPrompt(null)`.
    pub fn dismiss_template_switch_prompt(&mut self) {
        self.store().template_switch_prompt = None;
        crate::prelude::repaint();
    }

    fn set_template_error(&mut self, error: Option<String>) {
        self.store().template_error = error;
        crate::prelude::repaint();
    }

    /// `refreshConversationTemplates`.
    pub fn refresh_conversation_templates(&mut self) {
        self.invoke::<Vec<ConversationTemplateSummary>>("list_conversation_templates", json!({}), |app, result| match result {
            Ok(list) => {
                app.store().conversation_templates = list;
                crate::prelude::repaint();
            }
            Err(error) => app.set_template_error(Some(failure_message(&error, t("无法读取对话模板", "Could not read conversation templates")))),
        });
    }

    /// `templateSwitchNeedsConfirmation`: whether laying a preset's queue over this timeline has to ask first. Only a
    /// timeline that is the user's own work is at risk. An empty one has nothing to lose, and one whose length still
    /// matches the applied template's is being swapped for another preset's queue. The comparison is deliberately loose:
    /// that is the cheap direction to be wrong in, as the dialog only asks.
    pub fn template_switch_needs_confirmation(&mut self) -> bool {
        let Some(conversation) = self.active_conversation_view() else { return false };
        if self.conversation_has_no_contexts(&conversation) {
            return false;
        }
        let applied = self.store().conversation_templates.iter().find(|template| template.id == conversation.template_id).cloned();
        applied.is_none_or(|applied| applied.message_count != conversation.contexts.len() as u64)
    }

    /// `applyPresetToActiveConversation`: copies a preset's values and records which it came from. A preset opening with
    /// a queue also replaces the timeline, the one destructive half, so that half asks when there's work to lose.
    pub fn apply_preset_to_active_conversation(&mut self, preset_id: &str) {
        if self.active_conversation_id().is_none() {
            return;
        }
        let Some(preset) = crate::presets::conversation_preset_by_id(&self.doc.presets, preset_id).cloned() else { return };
        if !preset.template_id.is_empty() && self.template_switch_needs_confirmation() {
            self.store().template_switch_prompt = Some(preset.id);
            crate::prelude::repaint();
            return;
        }
        self.apply_preset_body(preset);
    }

    /// `applyPresetBody`: applies a preset's settings and, when it has one, the queue it opens with. Settings alone are a
    /// draft's own business, so a preset without a queue leaves a draft a draft. A queue is different: the host attests
    /// its copied tool cards against the target conversation id, so a draft is redeemed first, before either write.
    pub fn apply_preset_body(&mut self, preset: ConversationPreset) {
        self.set_template_error(None);
        let known_tool_names: HashSet<String> = self.doc.tools.iter().map(|tool| tool.name.clone()).collect();
        let settings = preset.settings.clone();
        let preset_id = preset.id.clone();
        let with_preset = move |conversation: &mut Conversation| {
            conversation.settings = crate::presets::apply_conversation_preset_settings(&conversation.settings, &settings, Some(&known_tool_names));
            conversation.preset_id = preset_id;
        };
        let active = self.active_conversation_id();
        if active.as_deref().is_some_and(crate::drafts::is_draft_conversation_id) {
            if preset.template_id.is_empty() {
                self.update_active_conversation(with_preset);
                return;
            }
            self.redeem_draft(true, move |app, redeemed| match redeemed {
                Ok(Some(redeemed)) => app.apply_preset_to_real_conversation(preset, redeemed.workspace_id, redeemed.conversation_id, with_preset),
                Ok(None) => {}
                Err(error) => app.set_template_error(Some(failure_message(&error, t("无法套用对话模板", "Could not apply the conversation template")))),
            });
            return;
        }
        let (Some(workspace_id), Some(conversation_id)) = (self.active_workspace_id(), active) else { return };
        self.apply_preset_to_real_conversation(preset, workspace_id, conversation_id, with_preset);
    }

    fn apply_preset_to_real_conversation(&mut self, preset: ConversationPreset, workspace_id: String, conversation_id: String, with_preset: impl FnOnce(&mut Conversation)) {
        self.update_conversation(&workspace_id, &conversation_id, with_preset);
        if preset.template_id.is_empty() {
            return;
        }
        let template_id = preset.template_id.clone();
        let request = json!({ "workspaceId": workspace_id, "templateId": template_id, "targetConversationId": conversation_id });
        self.invoke::<Vec<ContextItem>>("apply_conversation_template", request, move |app, result| match result {
            Ok(contexts) => {
                app.update_conversation(&workspace_id, &conversation_id, |conversation| {
                    conversation.contexts = contexts;
                    conversation.template_id = template_id;
                });
                // The timeline is a different one now; nothing on record was done to it.
                app.timeline_history().forget(&conversation_id);
            }
            Err(error) => app.set_template_error(Some(failure_message(&error, t("无法套用对话模板", "Could not apply the conversation template")))),
        });
    }

    /// `readTemplateBody`: reads a template body for an editor. An id nothing has been written under is an empty body:
    /// that is every preset and every role before its first save.
    pub fn read_template_body(&mut self, template_id: &str, done: impl FnOnce(&mut App, Result<Vec<ContextItem>, String>) + Send + 'static) {
        if template_id.is_empty() {
            return done(self, Ok(Vec::new()));
        }
        self.invoke::<Vec<ContextItem>>("preview_conversation_template", json!({ "templateId": template_id }), done);
    }

    /// `writeTemplateBody`: writes a template body, minting the id when its owner has none yet, and answers the id it
    /// landed under. The host normalizes every tool card it takes. Failures reach the caller: the editor that asked is
    /// the only surface that can say what went wrong.
    pub fn write_template_body(&mut self, template_id: &str, contexts: Vec<ContextItem>, done: impl FnOnce(&mut App, Result<String, String>) + Send + 'static) {
        let id = if template_id.is_empty() { create_id("template") } else { template_id.to_owned() };
        let request = json!({ "templateId": id, "contexts": contexts });
        self.invoke::<ConversationTemplateSummary>("update_conversation_template", request, move |app, result| match result {
            Ok(_) => {
                app.refresh_conversation_templates();
                done(app, Ok(id));
            }
            Err(error) => done(app, Err(error)),
        });
    }

    /// `renameConversationPreset`. Conversations cite it by id, so the trace follows.
    pub fn rename_conversation_preset(&mut self, preset_id: &str, name: &str) {
        if name.trim().is_empty() || crate::presets::is_builtin_conversation_preset(preset_id) {
            return;
        }
        self.update_global_settings(|doc| {
            if let Some(preset) = doc.presets.conversation_presets.iter_mut().find(|preset| preset.id == preset_id) {
                preset.name = name.trim().to_owned();
            }
        });
    }

    /// `deleteConversationPreset`. Leaves every citing conversation's `preset_id` dangling, which reads as an unnamed
    /// draft. The default moves to whatever preset is left, and the body it opened with goes too: nothing else cites it.
    pub fn delete_conversation_preset(&mut self, preset_id: &str) {
        // The host would put it back on save anyway; not asking is the honest answer.
        if crate::presets::is_builtin_conversation_preset(preset_id) {
            return;
        }
        let doomed = self.doc.presets.conversation_presets.iter().find(|preset| preset.id == preset_id).map(|preset| preset.template_id.clone());
        self.update_global_settings(|doc| doc.presets.conversation_presets.retain(|preset| preset.id != preset_id));
        // Best effort, and deliberately after the document change.
        if let Some(template_id) = doomed.filter(|template_id| !template_id.is_empty()) {
            self.invoke::<Value>("delete_conversation_template", json!({ "templateId": template_id }), |app, result| {
                if result.is_ok() {
                    app.refresh_conversation_templates();
                }
            });
        }
    }

    /// `saveConversationPreset`: writes an edited body onto a saved preset. Conversations already stamped from it keep
    /// what they were given; a preset never reaches back into one.
    pub fn save_conversation_preset(&mut self, preset_id: &str, settings: ConversationPresetSettings) {
        if crate::presets::is_builtin_conversation_preset(preset_id) {
            return;
        }
        self.update_global_settings(|doc| {
            if let Some(preset) = doc.presets.conversation_presets.iter_mut().find(|preset| preset.id == preset_id) {
                preset.settings = settings;
            }
        });
    }

    /// `bindPresetTemplate`: records which template a preset opens with, the moment its body is written, so closing the
    /// preset dialog can't strand a body nothing cites.
    pub fn bind_preset_template(&mut self, preset_id: &str, template_id: &str) {
        if crate::presets::is_builtin_conversation_preset(preset_id) {
            return;
        }
        self.update_global_settings(|doc| {
            if let Some(preset) = doc.presets.conversation_presets.iter_mut().find(|preset| preset.id == preset_id) {
                preset.template_id = template_id.to_owned();
            }
        });
    }

    /// `saveConversationPresetCopy`: saves what the user made of a preset they cannot change, the built-in one, as a new
    /// preset of their own, named after it. The copy opens with the template as edited, or with the source's own. The
    /// settings are worth keeping even when the body can't be written; the copy then opens with nothing, and says why.
    pub fn save_conversation_preset_copy(&mut self, preset_id: &str, settings: ConversationPresetSettings, template_body: Option<Vec<ContextItem>>) {
        self.set_template_error(None);
        let source = self.doc.presets.conversation_presets.iter().find(|preset| preset.id == preset_id).cloned();
        let source_name = source.as_ref().map(|preset| preset.name.clone()).unwrap_or_default();
        let source_template = source.map(|preset| preset.template_id).unwrap_or_default();
        self.spawn(move |task| {
            let written = (|| -> Result<String, String> {
                let body = match template_body {
                    Some(body) => body,
                    None if !source_template.is_empty() => task.invoke::<Vec<ContextItem>>("preview_conversation_template", json!({ "templateId": source_template }))?,
                    None => Vec::new(),
                };
                if body.is_empty() {
                    return Ok(String::new());
                }
                let id = create_id("template");
                task.invoke::<ConversationTemplateSummary>("update_conversation_template", json!({ "templateId": id, "contexts": body }))?;
                Ok(id)
            })();
            task.post(move |app| {
                let template_id = match written {
                    Ok(id) => {
                        if !id.is_empty() {
                            app.refresh_conversation_templates();
                        }
                        id
                    }
                    Err(error) => {
                        app.set_template_error(Some(failure_message(&error, t("无法复制对话模板", "Could not copy the conversation template"))));
                        String::new()
                    }
                };
                let preset = ConversationPreset { id: create_id("preset"), name: crate::tf("{name} 副本", "{name} copy", &[("name", &source_name)]).trim().to_owned(), description: String::new(), template_id, settings };
                app.update_global_settings(|doc| doc.presets.conversation_presets.push(preset));
            });
        });
    }

    /// `createConversationPreset`: saves the active conversation's settings as a new preset, and with them, when asked,
    /// its timeline as the preset's template. The host takes the timeline from its own store, so tool cards keep the
    /// results this application really produced. A draft has no row there yet, and nothing but prose in it, so its
    /// timeline is written as a body. The template is stored before the preset is.
    pub fn create_conversation_preset(&mut self, name: &str, capture_template: bool, done: impl FnOnce(&mut App, Result<ConversationPreset, String>) + Send + 'static) {
        let Some(conversation) = self.active_conversation_view() else { return done(self, Err(t("没有活动对话", "No active conversation").to_owned())) };
        let workspace_id = self.active_workspace_id();
        let name = name.to_owned();
        let settings = crate::presets::capture_conversation_preset_settings(&conversation.settings);
        let is_draft = crate::drafts::is_draft_conversation_id(&conversation.id);
        let wants_template = capture_template && !conversation.contexts.is_empty();
        self.spawn(move |task| {
            let template = (|| -> Result<String, String> {
                if !wants_template {
                    return Ok(String::new());
                }
                let template_id = create_id("template");
                if is_draft {
                    task.invoke::<ConversationTemplateSummary>("update_conversation_template", json!({ "templateId": template_id, "contexts": conversation.contexts }))?;
                } else {
                    let workspace_id = workspace_id.ok_or_else(|| t("没有活动对话", "No active conversation").to_owned())?;
                    task.flush_latest_document(false)?;
                    task.invoke::<ConversationTemplateSummary>("capture_conversation_template", json!({ "workspaceId": workspace_id, "conversationId": conversation.id, "templateId": template_id }))?;
                }
                Ok(template_id)
            })();
            task.post(move |app| match template {
                Err(error) => done(app, Err(error)),
                Ok(template_id) => {
                    if !template_id.is_empty() {
                        app.refresh_conversation_templates();
                    }
                    let preset = ConversationPreset { id: create_id("preset"), name, description: String::new(), template_id, settings };
                    let added = preset.clone();
                    app.update_global_settings(|doc| doc.presets.conversation_presets.push(added));
                    done(app, Ok(preset));
                }
            });
        });
    }
}

/// `failureMessage` of `App.tsx`: the reason, or `fallback` when it says nothing.
pub fn failure_message(reason: &str, fallback: &str) -> String {
    if reason.trim().is_empty() { fallback.to_owned() } else { reason.to_owned() }
}

/// Building blocks for tests here and in the modules this one coordinates.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::time::Instant;

    pub fn conversation(id: &str) -> Conversation {
        serde_json::from_value(json!({ "id": id, "title": id, "createdAt": "2026-07-20T00:00:00Z", "updatedAt": "2026-07-20T00:00:00Z", "settings": { "enabledTools": [] }, "contexts": [] })).unwrap()
    }

    pub fn text(id: &str, kind: &str) -> ContextItem {
        serde_json::from_value(json!({ "id": id, "kind": kind, "content": id, "createdAt": "2026-07-20T00:00:00Z" })).unwrap()
    }

    /// A document with one workspace per list, named `ws0`, `ws1`, ...
    pub fn document_of(workspaces: Vec<Vec<Conversation>>) -> AppDocument {
        let list: Vec<Value> = (0..workspaces.len()).map(|index| json!({ "id": format!("ws{index}"), "name": format!("ws{index}"), "path": format!("/ws{index}"), "createdAt": "2026-07-20T00:00:00Z", "conversations": [] })).collect();
        let mut document: AppDocument = serde_json::from_value(json!({ "schemaVersion": 1, "globalSettings": {}, "workspaces": list, "tools": [], "capabilities": { "skills": [], "mcps": [], "hooks": [] } })).unwrap();
        for (workspace, conversations) in document.workspaces.iter_mut().zip(workspaces) {
            workspace.conversations = conversations;
        }
        document
    }

    /// A real headless `App` on its own temp folder, with the host's commands replaced by `backend`.
    pub fn app_with(backend: DocBackend, tag: &str) -> App {
        let dir = std::env::temp_dir().join(format!("mewrk-client-doc-{tag}-{}-{}", std::process::id(), create_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App::start(crate::StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: dir, resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
        app.set_doc_backend(backend);
        app
    }

    /// Runs the UI thread until `done` holds, failing after a while.
    pub fn pump_until(app: &mut App, what: &str, mut done: impl FnMut(&mut App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.tick();
            if done(app) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::sync::Mutex;

    /// The commands the app sent, and what it answers: everything succeeds, echoing conversations, unless told otherwise.
    #[derive(Clone, Default)]
    struct Remote {
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        fail_saves: Arc<Mutex<Vec<String>>>,
        update_reply: Arc<Mutex<Option<Value>>>,
        gate: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
    }

    impl Remote {
        fn backend(&self) -> DocBackend {
            let this = self.clone();
            Arc::new(move |command, args| {
                this.calls.lock().unwrap().push((command.to_owned(), args.clone()));
                match command {
                    "save_document" => {
                        if let Some(gate) = this.gate.lock().unwrap().as_ref() {
                            let _ = gate.recv();
                        }
                        let mut failing = this.fail_saves.lock().unwrap();
                        if failing.is_empty() { Ok(Value::Null) } else { Err(failing.remove(0)) }
                    }
                    "update_conversation" => Ok(this.update_reply.lock().unwrap().take().unwrap_or_else(|| args["conversation"].clone())),
                    "create_conversation" => Ok(args["conversation"].clone()),
                    _ => Ok(Value::Null),
                }
            })
        }

        fn count(&self, command: &str) -> usize {
            self.calls.lock().unwrap().iter().filter(|(name, _)| name == command).count()
        }

        fn last(&self, command: &str) -> Value {
            self.calls.lock().unwrap().iter().rev().find(|(name, _)| name == command).map(|(_, args)| args.clone()).unwrap()
        }
    }

    fn flushed(app: &mut App) {
        let done = Arc::new(Mutex::new(false));
        let mark = done.clone();
        app.flush_latest_document(false, move |_, _| *mark.lock().unwrap() = true);
        pump_until(app, "flush", |_| *done.lock().unwrap());
    }

    fn add_conversation(app: &mut App, id: &str, contexts: Vec<ContextItem>) -> String {
        let workspace_id = app.doc.workspaces[0].id.clone();
        let mut c = conversation(id);
        c.contexts = contexts;
        app.doc.workspaces[0].conversations.insert(0, c);
        workspace_id
    }

    #[test]
    fn publishes_updates_synchronously_and_debounces_the_save() {
        let remote = Remote::default();
        let mut app = app_with(remote.backend(), "publish");
        app.update_document(|doc| doc.schema_version = 77);
        assert_eq!(app.doc.schema_version, 77);
        assert_eq!(app.save_status(), DocumentSaveStatus::Saving);
        assert_eq!(remote.count("save_document"), 0);
        app.update_document(|doc| doc.schema_version = 78);
        pump_until(&mut app, "the debounced save", |app| app.save_status() == DocumentSaveStatus::Saved);
        assert_eq!(remote.count("save_document"), 1);
        assert_eq!(remote.last("save_document")["document"]["schemaVersion"], 78);
    }

    #[test]
    fn a_save_never_carries_conversations() {
        let remote = Remote::default();
        let mut app = app_with(remote.backend(), "no-conversations");
        let workspace_id = add_conversation(&mut app, "c", vec![text("u", "user")]);
        flushed(&mut app);
        let saved = remote.last("save_document");
        let workspace = saved["document"]["workspaces"].as_array().unwrap().iter().find(|item| item["id"] == workspace_id.as_str()).unwrap().clone();
        assert_eq!(workspace["conversations"], json!([]));
        // And the conversation is back where it was.
        assert_eq!(app.doc.workspaces[0].conversations[0].contexts.len(), 1);
    }

    #[test]
    fn flush_cancels_the_pending_debounce_and_saves_the_current_snapshot() {
        let remote = Remote::default();
        let mut app = app_with(remote.backend(), "flush");
        app.update_document(|doc| doc.schema_version = 5);
        flushed(&mut app);
        assert_eq!(remote.count("save_document"), 1);
        assert_eq!(app.save_status(), DocumentSaveStatus::Saved);
        // The debounce it cancelled doesn't fire afterwards.
        let until = std::time::Instant::now() + Duration::from_millis(500);
        pump_until(&mut app, "the debounce window", |_| std::time::Instant::now() > until);
        assert_eq!(remote.count("save_document"), 1);
    }

    #[test]
    fn flush_reschedules_the_debounce_it_cancelled_when_the_save_fails() {
        let remote = Remote::default();
        remote.fail_saves.lock().unwrap().push("disk full".into());
        let mut app = app_with(remote.backend(), "flush-retry");
        app.update_document(|doc| doc.schema_version = 5);
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        app.flush_document_store(false, move |_, answer| *slot.lock().unwrap() = Some(answer));
        pump_until(&mut app, "the flush", |_| result.lock().unwrap().is_some());
        assert_eq!(result.lock().unwrap().clone().unwrap(), Err("disk full".to_owned()));
        // The retry it scheduled succeeds and clears the error.
        pump_until(&mut app, "the retry", |app| app.save_status() == DocumentSaveStatus::Saved);
        assert_eq!(remote.count("save_document"), 2);
    }

    #[test]
    fn reports_debounced_save_failures_with_their_reason() {
        let remote = Remote::default();
        remote.fail_saves.lock().unwrap().push("工作区未获授权".into());
        let mut app = app_with(remote.backend(), "failure-reason");
        app.update_document(|doc| doc.schema_version = 6);
        pump_until(&mut app, "the failure", |app| app.save_status() == DocumentSaveStatus::Error);
        assert_eq!(app.save_failure_reason().as_deref(), Some("工作区未获授权"));
        // A later save that works clears it.
        app.update_document(|doc| doc.schema_version = 7);
        pump_until(&mut app, "the next save", |app| app.save_status() == DocumentSaveStatus::Saved);
        assert_eq!(app.save_failure_reason(), None);
    }

    #[test]
    fn publish_persists_immediately_keeps_optimistic_state_on_failure_and_tells_the_caller() {
        let remote = Remote::default();
        remote.fail_saves.lock().unwrap().push("nope".into());
        let mut app = app_with(remote.backend(), "publish-failure");
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        app.publish_document(true, |doc| doc.schema_version = 9, move |_, answer| *slot.lock().unwrap() = Some(answer));
        assert_eq!(app.doc.schema_version, 9);
        pump_until(&mut app, "the publish", |_| result.lock().unwrap().is_some());
        assert_eq!(result.lock().unwrap().clone().unwrap(), Err("nope".to_owned()));
        assert_eq!(app.doc.schema_version, 9);
        assert_eq!(remote.calls.lock().unwrap()[0].1["durable"], true);
    }

    #[test]
    fn a_newer_revisions_outcome_owns_the_status_and_stale_outcomes_are_ignored() {
        let remote = Remote::default();
        let (release, gate) = mpsc::channel();
        *remote.gate.lock().unwrap() = Some(gate);
        remote.fail_saves.lock().unwrap().push("stale failure".into());
        let mut app = app_with(remote.backend(), "revisions");
        let first = Arc::new(Mutex::new(None));
        let slot = first.clone();
        app.flush_document_store(false, move |_, answer| *slot.lock().unwrap() = Some(answer));
        let second = Arc::new(Mutex::new(None));
        let slot = second.clone();
        app.publish_document(false, |doc| doc.schema_version = 3, move |_, answer| *slot.lock().unwrap() = Some(answer));
        release.send(()).unwrap();
        pump_until(&mut app, "the first save", |_| first.lock().unwrap().is_some());
        // The older write failed, but it no longer owns the status.
        assert_eq!(first.lock().unwrap().clone().unwrap(), Err("stale failure".to_owned()));
        assert_eq!(app.save_status(), DocumentSaveStatus::Saving);
        release.send(()).unwrap();
        pump_until(&mut app, "the second save", |_| second.lock().unwrap().is_some());
        assert_eq!(second.lock().unwrap().clone().unwrap(), Ok(()));
        *remote.gate.lock().unwrap() = None;
    }

    #[test]
    fn bridges_backend_push_results_failure_turns_red_success_only_clears_error() {
        let mut app = app_with(Remote::default().backend(), "bridge");
        app.report_backend_save_result(true);
        assert_eq!(app.save_status(), DocumentSaveStatus::Idle);
        app.report_backend_save_result(false);
        assert_eq!(app.save_status(), DocumentSaveStatus::Error);
        app.report_backend_save_result(true);
        assert_eq!(app.save_status(), DocumentSaveStatus::Saved);
    }

    #[test]
    fn dispose_cancels_the_pending_debounce_and_resume_rearms_saves() {
        let remote = Remote::default();
        let mut app = app_with(remote.backend(), "dispose");
        app.update_document(|doc| doc.schema_version = 4);
        app.dispose_document_store();
        app.update_document(|doc| doc.schema_version = 5);
        let until = std::time::Instant::now() + Duration::from_millis(500);
        pump_until(&mut app, "the debounce window", |_| std::time::Instant::now() > until);
        assert_eq!(remote.count("save_document"), 0);
        app.resume_document_store();
        pump_until(&mut app, "the save", |app| app.save_status() == DocumentSaveStatus::Saved);
        assert_eq!(remote.count("save_document"), 1);
    }

    // conversationSync.test.ts

    #[test]
    fn sends_one_command_for_a_burst_of_edits_keeping_the_first_base() {
        let remote = Remote::default();
        let mut app = app_with(remote.backend(), "burst");
        let mut last = conversation("conv_a");
        last.title = "last".into();
        last.contexts = vec![text("ctx_1", "user"), text("ctx_2", "assistant")];
        let mut middle = conversation("conv_a");
        middle.title = "middle".into();
        middle.contexts = vec![text("ctx_1", "user")];
        app.sync_conversation_changed("ws", vec!["ctx_1".into()], middle);
        app.sync_conversation_changed("ws", vec!["ctx_1".into(), "other".into()], last);
        flushed(&mut app);
        assert_eq!(remote.count("update_conversation"), 1);
        let sent = remote.last("update_conversation");
        assert_eq!(sent["workspaceId"], "ws");
        assert_eq!(sent["conversation"]["title"], "last");
        assert_eq!(sent["expectedContextIds"], json!(["ctx_1"]));
    }

    #[test]
    fn never_offers_an_unloaded_body_as_the_timeline_whatever_the_baseline() {
        let remote = Remote::default();
        let mut app = app_with(remote.backend(), "unloaded-commit");
        app.unloaded.insert("conv_a".into());
        let mut next = conversation("conv_a");
        next.title = "renamed".into();
        // A body that went from loaded to unloaded inside one debounce window: the baseline still names the host's real
        // timeline, which is exactly when the empty stand-in would replace it.
        app.sync_conversation_changed("ws", vec!["ctx_1".into(), "ctx_2".into()], next);
        flushed(&mut app);
        let sent = remote.last("update_conversation");
        assert_eq!(sent["conversation"]["title"], "renamed");
        assert_eq!(sent["expectedContextIds"], json!(["\u{0}unloaded-body"]));
    }

    #[test]
    fn applies_the_hosts_authoritative_body_over_the_optimistic_one() {
        let remote = Remote::default();
        let mut authority = conversation("conv_a");
        authority.title = "authoritative".into();
        authority.contexts = vec![text("ctx_1", "user"), text("ctx_host_tail", "assistant")];
        *remote.update_reply.lock().unwrap() = Some(serde_json::to_value(&authority).unwrap());
        let mut app = app_with(remote.backend(), "authority");
        let workspace_id = add_conversation(&mut app, "conv_a", vec![text("ctx_1", "user")]);
        app.update_conversation(&workspace_id, "conv_a", |conversation| conversation.title = "local".into());
        assert_eq!(app.find_conversation(&workspace_id, "conv_a").unwrap().title, "local");
        flushed(&mut app);
        pump_until(&mut app, "the authority", |app| app.find_conversation(&workspace_id, "conv_a").unwrap().title == "authoritative");
        assert_eq!(app.find_conversation(&workspace_id, "conv_a").unwrap().contexts.len(), 2);
    }

    #[test]
    fn an_edit_made_while_a_commit_is_in_flight_is_not_overwritten_by_its_reply() {
        let remote = Remote::default();
        let mut stale = conversation("conv_a");
        stale.title = "older reply".into();
        *remote.update_reply.lock().unwrap() = Some(serde_json::to_value(&stale).unwrap());
        let mut app = app_with(remote.backend(), "in-flight");
        let workspace_id = add_conversation(&mut app, "conv_a", vec![]);
        app.update_conversation(&workspace_id, "conv_a", |conversation| conversation.title = "first".into());
        app.commit_for_test("conv_a");
        // Before the reply lands, a newer edit is waiting its own window.
        app.update_conversation(&workspace_id, "conv_a", |conversation| conversation.title = "newer".into());
        let first_sent = remote.clone();
        pump_until(&mut app, "the first reply", |_| first_sent.count("update_conversation") >= 1);
        let settle = std::time::Instant::now() + Duration::from_millis(50);
        pump_until(&mut app, "settling", |_| std::time::Instant::now() > settle);
        assert_eq!(app.find_conversation(&workspace_id, "conv_a").unwrap().title, "newer");
    }

    #[test]
    fn drops_a_pending_edit_when_the_conversation_is_deleted() {
        let remote = Remote::default();
        let mut app = app_with(remote.backend(), "drop-pending");
        app.sync_conversation_changed("ws", vec![], conversation("conv_a"));
        app.sync_conversation_deleted("ws", "conv_a");
        flushed(&mut app);
        assert_eq!(remote.count("update_conversation"), 0);
        assert_eq!(remote.last("delete_conversation"), json!({ "workspaceId": "ws", "conversationId": "conv_a" }));
    }

    #[test]
    fn holds_workspace_commands_until_the_host_knows_the_workspace() {
        let remote = Remote::default();
        let mut app = app_with(remote.backend(), "barrier");
        app.update_document(|doc| {
            let mut extra = crate::workspaces::create_temporary_workspace(Vec::new());
            extra.id = "ws_new".into();
            extra.kind = crate::model::WorkspaceKind::Directory;
            doc.workspaces.insert(0, extra);
        });
        app.sync_conversation_created("ws_new", conversation("conv_slot"), vec!["conv_slot".into()]);
        flushed(&mut app);
        let order: Vec<String> = remote.calls.lock().unwrap().iter().map(|(name, _)| name.clone()).collect();
        let save = order.iter().position(|name| name == "save_document").unwrap();
        let create = order.iter().position(|name| name == "create_conversation").unwrap();
        assert!(save < create, "{order:?}");
        assert_eq!(remote.last("reorder_conversations"), json!({ "workspaceId": "ws_new", "conversationIds": ["conv_slot"] }));
    }

    #[test]
    fn still_sends_the_command_when_the_barrier_fails_so_the_host_reports_why() {
        let remote = Remote::default();
        remote.fail_saves.lock().unwrap().push("save refused".into());
        let mut app = app_with(remote.backend(), "barrier-failure");
        app.sync_conversations_reordered("ws_new", vec!["conv_moved".into()]);
        flushed(&mut app);
        assert_eq!(remote.last("reorder_conversations"), json!({ "workspaceId": "ws_new", "conversationIds": ["conv_moved"] }));
    }

    #[test]
    fn a_refused_write_records_why_and_adopts_the_hosts_body() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = calls.clone();
        let mut host_body = conversation("conv_a");
        host_body.title = "from the host".into();
        let reply = serde_json::to_value(&host_body).unwrap();
        let mut app = app_with(Arc::new(move |command, _| {
            seen.lock().unwrap().push(command.to_owned());
            match command {
                "update_conversation" => Err("refused".to_owned()),
                "load_conversation" => Ok(reply.clone()),
                _ => Ok(Value::Null),
            }
        }), "refusal");
        let workspace_id = add_conversation(&mut app, "conv_a", vec![]);
        app.update_conversation(&workspace_id, "conv_a", |conversation| conversation.title = "local".into());
        flushed(&mut app);
        pump_until(&mut app, "the host's body", |app| app.find_conversation(&workspace_id, "conv_a").unwrap().title == "from the host");
        assert_eq!(app.conversation_write_failure("conv_a").as_deref(), Some("refused"));
    }

    // conversationBodies.test.ts, on a real App's body cache.

    #[test]
    fn unloads_the_least_recently_opened_body_past_the_cap_never_a_pinned_one() {
        let mut app = app_with(Remote::default().backend(), "cap");
        *app.memory_pool() = crate::memory_pool::MemoryPool::new(2500);
        let big = |id: &str| -> ContextItem { serde_json::from_value(json!({ "id": format!("{id}-u"), "kind": "user", "content": "z".repeat(1000), "createdAt": "2026-09-30T00:00:00.000Z" })).unwrap() };
        for id in ["new", "mid", "old"] {
            add_conversation(&mut app, id, vec![big(id)]);
        }
        // `old` is inserted last, so it's first in the list and measured first: the oldest use.
        let pinned: HashSet<String> = ["new".to_owned()].into();
        app.sync_conversation_bodies(&pinned);
        assert!(app.is_body_unloaded("old"));
        assert!(!app.is_body_unloaded("mid"));
        assert!(!app.is_body_unloaded("new"));
        assert!(app.conversation("old").unwrap().contexts.is_empty());
    }

    #[test]
    fn opening_a_conversation_fetches_its_body_once_and_keeps_it() {
        let loads = Arc::new(Mutex::new(0));
        let counted = loads.clone();
        let mut stored = conversation("c");
        stored.contexts = vec![text("u", "user")];
        let reply = serde_json::to_value(&stored).unwrap();
        let mut app = app_with(Arc::new(move |command, _| {
            if command == "load_conversation" {
                *counted.lock().unwrap() += 1;
                return Ok(reply.clone());
            }
            Ok(Value::Null)
        }), "fetch-once");
        add_conversation(&mut app, "c", vec![]);
        app.unloaded.insert("c".into());
        let answers = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..2 {
            let answers = answers.clone();
            app.ensure_conversation_loaded("c", move |_, answer| answers.lock().unwrap().push(answer));
        }
        assert!(app.conversation_body_is_loading("c"));
        pump_until(&mut app, "both answers", |_| answers.lock().unwrap().len() == 2);
        assert_eq!(*loads.lock().unwrap(), 1);
        assert_eq!(*answers.lock().unwrap(), vec![Ok(true), Ok(true)]);
        assert_eq!(app.conversation("c").unwrap().contexts.len(), 1);
        assert!(!app.is_body_unloaded("c"));
        assert!(app.memory_pool().has(crate::memory_pool::MemoryPoolKind::ConversationBody, "c"));
        let again = Arc::new(Mutex::new(None));
        let slot = again.clone();
        app.ensure_conversation_loaded("c", move |_, answer| *slot.lock().unwrap() = Some(answer));
        assert_eq!(again.lock().unwrap().clone(), Some(Ok(true)));
        assert_eq!(*loads.lock().unwrap(), 1);
    }

    #[test]
    fn a_conversation_deleted_while_its_body_was_on_the_way_stays_deleted() {
        let (release, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let reply = serde_json::to_value(conversation("c")).unwrap();
        let mut app = app_with(Arc::new(move |command, _| {
            if command == "load_conversation" {
                let _ = gate.lock().unwrap().recv();
                return Ok(reply.clone());
            }
            Ok(Value::Null)
        }), "deleted-while-loading");
        add_conversation(&mut app, "c", vec![]);
        app.unloaded.insert("c".into());
        let answer = Arc::new(Mutex::new(None));
        let slot = answer.clone();
        app.ensure_conversation_loaded("c", move |_, result| *slot.lock().unwrap() = Some(result));
        app.doc.workspaces[0].conversations.retain(|conversation| conversation.id != "c");
        release.send(()).unwrap();
        pump_until(&mut app, "the answer", |_| answer.lock().unwrap().is_some());
        assert_eq!(answer.lock().unwrap().clone(), Some(Ok(false)));
        assert!(app.conversation("c").is_none());
    }

    #[test]
    fn a_failed_body_load_keeps_the_reason_until_retried() {
        let fail = Arc::new(Mutex::new(true));
        let failing = fail.clone();
        let reply = serde_json::to_value(conversation("c")).unwrap();
        let mut app = app_with(Arc::new(move |command, _| {
            if command == "load_conversation" {
                return if *failing.lock().unwrap() { Err("disk gone".to_owned()) } else { Ok(reply.clone()) };
            }
            Ok(Value::Null)
        }), "load-failure");
        add_conversation(&mut app, "c", vec![]);
        app.unloaded.insert("c".into());
        app.load_active_body("c");
        pump_until(&mut app, "the failure", |app| app.body_load_failure("c").is_some());
        assert_eq!(app.body_load_failure("c").as_deref(), Some("disk gone"));
        assert!(app.is_body_unloaded("c"));
        *fail.lock().unwrap() = false;
        app.load_active_body("c");
        assert_eq!(app.body_load_failure("c"), None);
        pump_until(&mut app, "the retry", |app| !app.is_body_unloaded("c"));
    }

    #[test]
    fn pooled_fetch_shares_one_load_between_readers_keeps_what_arrives_and_forgets_what_fails() {
        use crate::memory_pool::{MemoryPool, MemoryPoolKind::{ImageData, ImageThumbnail}};
        let mut app = app_with(Remote::default().backend(), "pooled");
        *app.memory_pool() = MemoryPool::new(1000);
        let loads = Arc::new(Mutex::new(0));
        let answers = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..2 {
            let (loads, answers) = (loads.clone(), answers.clone());
            app.pooled_fetch(ImageThumbnail, "k", move |_| {
                *loads.lock().unwrap() += 1;
                Ok("data:image/png;base64,AAAA".to_owned())
            }, move |_, answer| answers.lock().unwrap().push(answer));
        }
        pump_until(&mut app, "both readers", |_| answers.lock().unwrap().len() == 2);
        assert_eq!(*loads.lock().unwrap(), 1);
        assert_eq!(app.memory_pool().stats().total_bytes, "data:image/png;base64,AAAA".len() as u64);
        let again = answers.clone();
        app.pooled_fetch(ImageThumbnail, "k", |_| panic!("cached"), move |_, answer| again.lock().unwrap().push(answer));
        assert_eq!(answers.lock().unwrap().len(), 3);

        let failed = Arc::new(Mutex::new(None));
        let slot = failed.clone();
        app.pooled_fetch(ImageData, "x", |_| Err("gone".to_owned()), move |_, answer| *slot.lock().unwrap() = Some(answer));
        pump_until(&mut app, "the failure", |_| failed.lock().unwrap().is_some());
        assert_eq!(failed.lock().unwrap().clone().unwrap(), Err("gone".to_owned()));
        assert!(!app.memory_pool().has(ImageData, "x"));
    }

    #[test]
    fn selecting_a_branch_persists_the_swap_and_refuses_while_the_conversation_is_active() {
        let remote = Remote::default();
        let mut app = app_with(remote.backend(), "select-branch");
        let workspace_id = add_conversation(&mut app, "c", vec![text("u1", "user"), text("new", "assistant")]);
        let branch = |id: &str, active: bool, contexts: Vec<ContextItem>| crate::model::ConversationBranch { id: id.into(), fork_context_id: "u1".into(), active, contexts, created_at: "t".into(), updated_at: "t".into() };
        app.doc.workspaces[0].conversations[0].branches = vec![branch("old", false, vec![text("old-answer", "assistant")]), branch("cur", true, vec![])];
        app.select_conversation(&workspace_id, "c");
        app.runs().controller.add_performing_run("c");
        app.select_conversation_branch("u1", "old");
        assert_eq!(app.conversation("c").unwrap().contexts.len(), 2);
        assert_eq!(app.conversation("c").unwrap().contexts[1].id(), "new", "refused while a run is performing");
        app.runs().controller.delete_performing_run("c");
        app.select_conversation_branch("u1", "old");
        assert_eq!(app.conversation("c").unwrap().contexts[1].id(), "old-answer");
        assert!(app.runs().controller.has_preparing_run("c"), "held while it persists");
        pump_until(&mut app, "the persist", |app| !app.runs().controller.has_preparing_run("c"));
        let sent = remote.last("update_conversation");
        assert_eq!(sent["expectedContextIds"], json!(["u1", "new"]));
        assert_eq!(sent["conversation"]["contexts"][1]["id"], "old-answer");
    }

    #[test]
    fn deleting_refuses_a_busy_conversation_and_reparents_its_children() {
        let remote = Remote::default();
        let mut app = app_with(remote.backend(), "delete-guards");
        let workspace_id = add_conversation(&mut app, "parent", vec![text("u", "user")]);
        app.doc.workspaces[0].conversations.insert(0, conversation("child"));
        app.doc.workspaces[0].conversations[0].parent_conversation_id = Some("parent".into());
        app.select_conversation(&workspace_id, "parent");
        app.runs().controller.add_preparing_run("parent");
        app.delete_conversation(&workspace_id, "parent");
        assert!(!app.conversation_is_deleting("parent"));
        assert!(app.conversation("parent").is_some());
        app.runs().controller.delete_preparing_run("parent");
        app.delete_conversation(&workspace_id, "parent");
        assert!(app.conversation_is_deleting("parent"));
        // A second request while the first is under way is ignored.
        app.delete_conversation(&workspace_id, "parent");
        pump_until(&mut app, "the deletion", |app| !app.conversation_is_deleting("parent"));
        flushed(&mut app);
        assert!(app.conversation("parent").is_none());
        assert_eq!(app.conversation("child").unwrap().parent_conversation_id, None);
        assert_eq!(remote.count("delete_conversation"), 1);
        // The conversation on screen went, so its project's draft opens.
        assert_eq!(app.active_conversation_id(), Some(crate::drafts::draft_conversation_id(&workspace_id)));
    }

    #[test]
    fn a_failed_surface_close_stops_the_deletion() {
        let mut app = app_with(Remote::default().backend(), "delete-closer");
        let workspace_id = add_conversation(&mut app, "c", vec![]);
        app.doc_hooks().surface_closers.push(Rc::new(|_, _| vec![Box::new(|_: &Task| Err("the terminal won't close".to_owned())) as SurfaceCloser]));
        app.delete_conversation(&workspace_id, "c");
        pump_until(&mut app, "the deletion to give up", |app| !app.conversation_is_deleting("c"));
        assert!(app.conversation("c").is_some());
    }

    // documentUpdates.test.ts

    fn preset(id: &str) -> ConversationPreset {
        ConversationPreset { id: id.into(), name: id.into(), description: String::new(), template_id: String::new(), settings: crate::presets::empty_conversation_preset_settings() }
    }

    #[test]
    fn evaluates_changes_against_the_document_it_is_given_so_chained_updates_compound() {
        let mut document = document_of(vec![]);
        apply_global_settings_change(&mut document, |doc| doc.presets.conversation_presets.push(preset("preset_a")));
        apply_global_settings_change(&mut document, |doc| doc.presets.conversation_presets.push(preset("preset_b")));
        let ids: Vec<&str> = document.presets.conversation_presets.iter().map(|preset| preset.id.as_str()).collect();
        assert_eq!(ids, vec!["preset_a", "preset_b"]);
    }

    #[test]
    fn falls_back_to_the_first_preset_when_the_default_preset_disappears() {
        let mut document = document_of(vec![]);
        document.presets.conversation_presets = vec![preset("a"), preset("b")];
        document.presets.default_conversation_preset_id = "b".into();
        apply_global_settings_change(&mut document, |doc| doc.presets.conversation_presets.retain(|preset| preset.id != "b"));
        assert_eq!(document.presets.default_conversation_preset_id, "a");
    }

    #[test]
    fn clears_the_default_preset_id_when_no_presets_remain() {
        let mut document = document_of(vec![]);
        document.presets.conversation_presets = vec![preset("a")];
        document.presets.default_conversation_preset_id = "a".into();
        apply_global_settings_change(&mut document, |doc| doc.presets.conversation_presets.clear());
        assert_eq!(document.presets.default_conversation_preset_id, "");
    }

    #[test]
    fn keeps_a_still_valid_default_preset_id_untouched_on_wholesale_replacement() {
        let mut document = document_of(vec![]);
        document.presets.conversation_presets = vec![preset("a"), preset("b")];
        document.presets.default_conversation_preset_id = "b".into();
        apply_global_settings_change(&mut document, |doc| doc.presets.conversation_presets = vec![preset("b"), preset("c")]);
        assert_eq!(document.presets.default_conversation_preset_id, "b");
    }

    fn tool_context(id: &str, nested: Option<Vec<ContextItem>>) -> ContextItem {
        let mut value = json!({ "id": id, "kind": "tool", "toolName": "read", "input": {}, "result": { "success": true, "output": "", "executedAt": "2026-08-09T00:00:00Z", "durationMs": 1 }, "createdAt": "2026-08-09T00:00:00Z" });
        if let Some(nested) = nested {
            value["subagent"] = json!({ "task": "child", "status": "completed", "contexts": nested, "updates": [] });
        }
        serde_json::from_value(value).unwrap()
    }

    fn marker(id: &str) -> ContextItem {
        serde_json::from_value(json!({ "id": id, "kind": "system", "content": "quarantined", "localOnly": true, "createdAt": "2026-08-09T00:00:00Z" })).unwrap()
    }

    fn quarantine(conversation_id: &str, context_id: &str) -> QuarantinedContextReplacement {
        QuarantinedContextReplacement { conversation_id: conversation_id.into(), context_id: context_id.into(), replacement: marker(context_id) }
    }

    fn document_with(contexts: Vec<ContextItem>) -> AppDocument {
        let mut c = conversation("c");
        c.contexts = contexts;
        document_of(vec![vec![c]])
    }

    #[test]
    fn installs_the_committed_marker_and_keeps_everything_else() {
        let kept = text("ctx_keep", "user");
        let mut document = document_with(vec![tool_context("ctx_drop", None), kept.clone()]);
        assert!(apply_quarantined_context_replacements(&mut document, &[quarantine("c", "ctx_drop")]));
        assert_eq!(document.workspaces[0].conversations[0].contexts, vec![marker("ctx_drop"), kept]);
    }

    #[test]
    fn reaches_contexts_nested_inside_a_subagent_transcript() {
        let mut document = document_with(vec![tool_context("ctx_outer", Some(vec![tool_context("ctx_child", None)]))]);
        assert!(apply_quarantined_context_replacements(&mut document, &[quarantine("c", "ctx_child")]));
        let ContextItem::Tool { subagent: Some(subagent), .. } = &document.workspaces[0].conversations[0].contexts[0] else { panic!() };
        assert_eq!(subagent.contexts, vec![marker("ctx_child")]);
    }

    #[test]
    fn keeps_the_same_context_id_in_a_sibling_conversation() {
        let mut document = document_with(vec![tool_context("ctx_shared", None)]);
        let mut sibling = conversation("conv_sibling");
        sibling.contexts = vec![tool_context("ctx_shared", None)];
        document.workspaces[0].conversations.push(sibling.clone());
        assert!(apply_quarantined_context_replacements(&mut document, &[quarantine("c", "ctx_shared")]));
        assert_eq!(document.workspaces[0].conversations[0].contexts, vec![marker("ctx_shared")]);
        assert_eq!(document.workspaces[0].conversations[1], sibling);
    }

    #[test]
    fn answers_false_when_nothing_matches_so_no_save_is_scheduled() {
        let mut document = document_with(vec![tool_context("ctx_only", None)]);
        assert!(!apply_quarantined_context_replacements(&mut document, &[quarantine("c", "ctx_absent")]));
        assert!(!apply_quarantined_context_replacements(&mut document, &[]));
    }

    #[test]
    fn says_which_worktrees_a_deletion_kept() {
        assert_eq!(kept_worktrees_notice(&[]), None);
        let notice = kept_worktrees_notice(&[KeptWorktree { path: "/a".into(), error: None }, KeptWorktree { path: "/b".into(), error: Some("busy".into()) }]).unwrap();
        assert!(notice.contains("/a") && notice.contains("/b") && notice.contains("busy"));
    }
}

#[cfg(test)]
impl App {
    fn commit_for_test(&mut self, conversation_id: &str) {
        let id = conversation_id.to_owned();
        self.flush_conversation_sync_one(&id);
    }
}
