//! What's selected: the active project and conversation (`activeWorkspaceId`, `activeConversationId`), back and forward
//! through what was opened, and the per-conversation choices `App.tsx` keeps beside them.
//!
//! "Unseen completions" and the run half of `conversationStatus` live in `runs` (`App::conversation_status`,
//! `RunsState::unseen_completions`), which sees runs end; this module tells it what opened.

use std::collections::{HashMap, HashSet};

use crate::conversation_history::{conversation_history_target, visit_conversation, ConversationHistory, ConversationHistoryEntry, HistoryTarget};
use crate::drafts::{is_draft_conversation_id, visible_conversations};
use crate::model::{AttachedWorkspace, Conversation, Workspace};
use crate::runs::ConversationStatus;
use crate::workspaces::{conversation_workspaces, project_workspaces, registered_project_workspaces};
use crate::App;

#[derive(Default)]
pub(crate) struct SelectionState {
    active_workspace_id: Option<String>,
    active_conversation_id: Option<String>,
    history: ConversationHistory,
    /// `contextScrollRef`: where each timeline was left.
    context_scroll: HashMap<String, f32>,
    /// `selectedWorkspaceMembers`: the composer's workspace chip, 1-based, per conversation.
    pub selected_workspace_members: HashMap<String, usize>,
    /// `reviewPageMembers`: the review pane's open page, per conversation.
    pub review_page_members: HashMap<String, usize>,
    /// `reviewPageOrders`: the order the reader dragged the review pages into.
    pub review_page_orders: HashMap<String, Vec<usize>>,
    /// The conversation whose activation effects last ran, so they run once per opening.
    activated: Option<String>,
}

fn selection(app: &mut App) -> &mut SelectionState {
    app.local::<SelectionState>()
}

/// Selection.
impl App {
    /// `activeWorkspaceId`.
    pub fn active_workspace_id(&mut self) -> Option<String> {
        selection(self).active_workspace_id.clone()
    }

    /// `activeConversationId`: a real conversation's id, or a draft's placeholder.
    pub fn active_conversation_id(&mut self) -> Option<String> {
        selection(self).active_conversation_id.clone()
    }

    /// `setActiveWorkspaceId` and `setActiveConversationId`: moves the selection, with no more said about it.
    pub fn set_active(&mut self, workspace_id: Option<&str>, conversation_id: Option<&str>) {
        let state = selection(self);
        state.active_workspace_id = workspace_id.map(str::to_owned);
        state.active_conversation_id = conversation_id.map(str::to_owned);
        // TODO: the stand-in `runs` reads until its seam calls `active_workspace_id`.
        self.set_runs_active_conversation(workspace_id, conversation_id);
        self.schedule_doc_effects();
        crate::prelude::repaint();
    }

    /// `selectConversation`. The draft, its composer text included, waits where it is: the next new task returns to it.
    pub fn select_conversation(&mut self, workspace_id: &str, conversation_id: &str) {
        self.set_active(Some(workspace_id), Some(conversation_id));
        self.close_editors();
    }

    /// `selectConversation` knowing only the conversation, for what doesn't have its project at hand.
    pub fn open_conversation(&mut self, conversation_id: &str) {
        if let Some(workspace_id) = self.workspace_of_conversation(conversation_id).map(|workspace| workspace.id.clone()) {
            self.select_conversation(&workspace_id, conversation_id);
        }
    }

    /// `contextScrollRef[activeConversationId] = scrollTop`: the timeline saves this as it's left.
    pub fn set_context_scroll(&mut self, conversation_id: &str, top: f32) {
        selection(self).context_scroll.insert(conversation_id.to_owned(), top);
    }

    /// `contextScrollRef[conversationId]`: where to restore a timeline to; `None` means its end.
    pub fn context_scroll(&mut self, conversation_id: &str) -> Option<f32> {
        selection(self).context_scroll.get(conversation_id).copied()
    }

    /// The active workspace, `activeWorkspace`: a draft's project when a draft is on screen.
    pub fn active_workspace(&mut self) -> Option<&Workspace> {
        let id = self.active_workspace_id()?;
        self.workspace(&id)
    }

    /// `activeConversation`: the persisted conversation, or the draft projected as one, so the normal timeline, composer
    /// and settings surfaces render unchanged. A clone: use [`App::find_conversation`] in a view that runs every frame
    /// on a long timeline, once it knows it isn't a draft.
    pub fn active_conversation_view(&mut self) -> Option<Conversation> {
        let id = self.active_conversation_id()?;
        if is_draft_conversation_id(&id) {
            return self.draft_of(&id).map(|draft| crate::drafts::draft_as_conversation(&draft, crate::t("新任务", "New task")));
        }
        let workspace_id = self.active_workspace_id()?;
        self.find_conversation(&workspace_id, &id).cloned()
    }

    /// `draftActive`.
    pub fn draft_is_active(&mut self) -> bool {
        self.active_conversation_id().is_some_and(|id| self.draft_of(&id).is_some())
    }

    /// `activeBodyUnloaded`: the conversation on screen is waiting for its body.
    pub fn active_body_unloaded(&mut self) -> bool {
        !self.draft_is_active() && self.active_conversation_id().is_some_and(|id| self.is_body_unloaded(&id))
    }

    /// `composerIsUnsentTask`: whether the composer belongs to a task that has never been sent. Mostly the draft, but a
    /// real conversation can be just as empty, and it's still withheld from the sidebar and still the same empty desk.
    pub fn composer_is_unsent_task(&mut self) -> bool {
        if self.draft_is_active() {
            return true;
        }
        let (Some(workspace_id), Some(id)) = (self.active_workspace_id(), self.active_conversation_id()) else { return false };
        let Some(workspace) = self.workspace(&workspace_id) else { return false };
        let Some(conversation) = workspace.conversations.iter().find(|conversation| conversation.id == id) else { return false };
        let has_children = workspace.conversations.iter().any(|candidate| candidate.parent_conversation_id.as_deref() == Some(id.as_str()));
        crate::drafts::is_unsent_conversation(conversation, has_children, &self.unloaded)
    }

    /// `activeConversationStarted`: whether the conversation has started. The project it belongs to is settled from
    /// then on, so the composer stops offering to change it.
    pub fn active_conversation_started(&mut self) -> bool {
        if self.draft_is_active() {
            return false;
        }
        let (Some(workspace_id), Some(id)) = (self.active_workspace_id(), self.active_conversation_id()) else { return false };
        self.find_conversation(&workspace_id, &id).is_some_and(|conversation| !self.conversation_has_no_contexts(conversation))
    }

    /// `activeProjectWorkspaces`: the project's own workspaces as this conversation uses them: its worktree of each stands in for it.
    pub fn active_project_workspaces(&mut self) -> Vec<AttachedWorkspace> {
        let conversation = self.active_conversation_view();
        let worktrees = conversation.as_ref().map_or(&[][..], |conversation| &conversation.worktrees);
        project_workspaces(self.active_workspace(), worktrees)
    }

    /// `activeRegisteredWorkspaces`: the project's workspaces as registered: what worktrees are made from, and named after.
    pub fn active_registered_workspaces(&mut self) -> Vec<AttachedWorkspace> {
        registered_project_workspaces(self.active_workspace())
    }

    /// `conversationWorkspaces(activeWorkspace, activeConversation)`: in the order the model addresses them.
    pub fn active_conversation_workspaces(&mut self) -> Vec<AttachedWorkspace> {
        let conversation = self.active_conversation_view();
        conversation_workspaces(self.active_workspace(), conversation.as_ref())
    }

    /// `activeWorkspaceMember`: the project workspace the composer's workspace chip has selected, 1-based. A selection
    /// past the end, a workspace removed from the project since, falls back to the first.
    pub fn active_workspace_member(&mut self) -> usize {
        let count = self.active_project_workspaces().len();
        let selected = self.active_conversation_id().and_then(|id| selection(self).selected_workspace_members.get(&id).copied()).unwrap_or(1);
        if selected >= 1 && selected <= count { selected } else { 1 }
    }

    /// `setSelectedWorkspaceMembers`.
    pub fn select_workspace_member(&mut self, conversation_id: &str, member: usize) {
        selection(self).selected_workspace_members.insert(conversation_id.to_owned(), member);
        crate::prelude::repaint();
    }

    /// `reviewPageMembers[id]`: the page the reader opened in the review pane.
    pub fn review_page_member(&mut self, conversation_id: &str) -> Option<usize> {
        selection(self).review_page_members.get(conversation_id).copied()
    }

    /// `setReviewPageMembers`.
    pub fn set_review_page_member(&mut self, conversation_id: &str, member: usize) {
        selection(self).review_page_members.insert(conversation_id.to_owned(), member);
        crate::prelude::repaint();
    }

    /// `reviewPageOrders[id]`.
    pub fn review_page_order(&mut self, conversation_id: &str) -> Vec<usize> {
        selection(self).review_page_orders.get(conversation_id).cloned().unwrap_or_default()
    }

    /// `setReviewPageOrders`.
    pub fn set_review_page_order(&mut self, conversation_id: &str, order: Vec<usize>) {
        selection(self).review_page_orders.insert(conversation_id.to_owned(), order);
        crate::prelude::repaint();
    }

    pub(crate) fn forget_selection_of(&mut self, conversation_id: &str) {
        let state = selection(self);
        state.context_scroll.remove(conversation_id);
        state.selected_workspace_members.remove(conversation_id);
        state.review_page_members.remove(conversation_id);
        state.review_page_orders.remove(conversation_id);
    }

    /// `adoptDraftConversationId`'s part: the workspace chip's choice is where the draft's terminals and Git surface were
    /// pointed, and the review pane's page and page order are the reader's; all three carry over.
    pub(crate) fn adopt_selection_of(&mut self, draft_id: &str, conversation_id: &str) {
        let state = selection(self);
        if let Some(member) = state.selected_workspace_members.remove(draft_id) {
            state.selected_workspace_members.insert(conversation_id.to_owned(), member);
        }
        if let Some(member) = state.review_page_members.remove(draft_id) {
            state.review_page_members.insert(conversation_id.to_owned(), member);
        }
        if let Some(order) = state.review_page_orders.remove(draft_id) {
            state.review_page_orders.insert(conversation_id.to_owned(), order);
        }
    }

    /// `pinnedBodyIds`, `conversationBodies.sync`, the touch and load on opening, and `visitConversation`, as upstream's
    /// effects did. Conversation bodies that must stay loaded: the one on screen, and every one with a run in flight,
    /// whose settlement splices into the body it started from.
    pub(crate) fn selection_effects(&mut self) {
        let draft_active = self.draft_is_active();
        let active = self.active_conversation_id();
        let workspace = self.active_workspace_id();
        let mut pinned: HashSet<String> = self.runs().controller.summaries().keys().cloned().collect();
        if let (false, Some(id)) = (draft_active, &active) {
            pinned.insert(id.clone());
        }
        self.sync_conversation_bodies(&pinned);
        let opened = selection(self).activated.as_ref() != active.as_ref();
        if opened {
            selection(self).activated = active.clone();
            if let (false, Some(id)) = (draft_active, &active) {
                // Opening a conversation is what counts as using its body.
                self.touch_conversation_body(id);
                self.note_conversation_activated(id);
                // Opening one that failed before tries again.
                if self.is_body_unloaded(id) && !self.conversation_body_is_loading(id) {
                    self.load_active_body(id);
                }
            }
            // Only real conversations are recorded; the draft is not.
            if let (Some(workspace_id), Some(id)) = (workspace, active.clone()) {
                if !is_draft_conversation_id(&id) {
                    visit_conversation(&mut selection(self).history, ConversationHistoryEntry { workspace_id, conversation_id: id });
                }
            }
        } else if let (false, Some(id)) = (draft_active, &active) {
            // A body that became unloaded under the one on screen; a failure waits for the retry button.
            if self.is_body_unloaded(id) && !self.conversation_body_is_loading(id) && self.body_load_failure(id).is_none() {
                self.load_active_body(id);
            }
        }
    }
}

/// `DRAFT_LANDING_TASK_LIMIT`.
pub const DRAFT_LANDING_TASK_LIMIT: usize = 5;

/// One row of what a task nothing has been sent in shows where its timeline will be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftLandingTask {
    pub id: String,
    pub title: String,
    pub updated_at: String,
    pub status: ConversationStatus,
}

/// History.
impl App {
    /// `draftLandingTasks`: the project's tasks, most recently updated first (the sidebar lists them in the user's own
    /// order), for a task nothing has been sent in.
    pub fn draft_landing_tasks(&mut self) -> Vec<DraftLandingTask> {
        if !self.composer_is_unsent_task() {
            return Vec::new();
        }
        let Some(workspace_id) = self.active_workspace_id() else { return Vec::new() };
        let Some(workspace) = self.workspace(&workspace_id) else { return Vec::new() };
        let mut listed: Vec<(String, String, String)> = visible_conversations(&workspace.conversations, &self.unloaded).iter().map(|conversation| (conversation.id.clone(), conversation.title.clone(), conversation.updated_at.clone())).collect();
        listed.sort_by(|left, right| right.2.cmp(&left.2));
        listed.truncate(DRAFT_LANDING_TASK_LIMIT);
        listed.into_iter().map(|(id, title, updated_at)| {
            let status = self.sidebar_conversation_status(&id);
            DraftLandingTask { id, title, updated_at, status }
        }).collect()
    }

    fn locate_listed_conversation(&self, conversation_id: &str) -> Option<String> {
        // Where each listed conversation lives now; a history entry may have moved project since.
        self.doc.workspaces.iter().find(|workspace| visible_conversations(&workspace.conversations, &self.unloaded).iter().any(|conversation| conversation.id == conversation_id)).map(|workspace| workspace.id.clone())
    }

    /// `historyBack`/`historyForward`: where one step (`-1` or `1`) would land, or `None`, which greys out the button.
    pub fn conversation_history_target(&mut self, step: isize) -> Option<HistoryTarget> {
        let active = self.active_conversation_id();
        let history = selection(self).history.clone();
        conversation_history_target(&history, active.as_deref(), step, |id| self.locate_listed_conversation(id))
    }

    /// `stepConversationHistory`: takes one step, for the title bar's arrows and a mouse's back and forward buttons.
    pub fn step_conversation_history(&mut self, step: isize) {
        let Some(target) = self.conversation_history_target(step) else { return };
        selection(self).history.index = target.index;
        self.select_conversation(&target.workspace_id, &target.conversation_id);
    }

    /// `stepConversation` of the shortcut table: next or previous in the project, wrapping, because these commands move
    /// to the next conversation rather than stopping at either end.
    pub fn step_conversation(&mut self, delta: isize) {
        let (Some(workspace_id), Some(active)) = (self.active_workspace_id(), self.active_conversation_id()) else { return };
        let Some(workspace) = self.workspace(&workspace_id) else { return };
        let count = workspace.conversations.len() as isize;
        if count < 2 {
            return;
        }
        let Some(index) = workspace.conversations.iter().position(|conversation| conversation.id == active) else { return };
        let next = workspace.conversations[(index as isize + delta + count).rem_euclid(count) as usize].id.clone();
        self.select_conversation(&workspace_id, &next);
    }

    /// `conversationStatus`: the sidebar's mark: blocked on the user, running (a model run or anything the panes report
    /// through [`DocHooks::live_activity`](crate::doc::DocHooks)), a run that finished unseen, or idle.
    pub fn sidebar_conversation_status(&mut self, conversation_id: &str) -> ConversationStatus {
        let hooks = self.doc_hooks().live_activity.clone();
        let other = hooks.iter().any(|active| active(self, conversation_id));
        self.conversation_status(conversation_id, other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::test_support::{app_with, conversation, pump_until, text};
    use serde_json::Value;
    use std::sync::Arc;

    fn app(tag: &str) -> App {
        app_with(Arc::new(|_, _| Ok(Value::Null)), tag)
    }

    fn with_conversations(app: &mut App, ids: &[&str]) -> String {
        for id in ids.iter().rev() {
            let mut c = conversation(id);
            c.contexts = vec![text(&format!("{id}-u"), "user")];
            app.doc.workspaces[0].conversations.insert(0, c);
        }
        app.doc.workspaces[0].id.clone()
    }

    fn settle(app: &mut App) {
        for _ in 0..3 {
            app.tick();
        }
    }

    #[test]
    fn records_opened_conversations_and_walks_back_and_forward() {
        let mut app = app("history");
        let workspace = with_conversations(&mut app, &["a", "b", "c"]);
        for id in ["a", "b", "c"] {
            app.select_conversation(&workspace, id);
            settle(&mut app);
        }
        assert_eq!(app.conversation_history_target(1), None);
        app.step_conversation_history(-1);
        settle(&mut app);
        assert_eq!(app.active_conversation_id().as_deref(), Some("b"));
        assert_eq!(app.conversation_history_target(1).unwrap().conversation_id, "c");
        app.step_conversation_history(1);
        settle(&mut app);
        assert_eq!(app.active_conversation_id().as_deref(), Some("c"));
    }

    #[test]
    fn steps_over_a_deleted_conversation() {
        let mut app = app("history-deleted");
        let workspace = with_conversations(&mut app, &["a", "gone", "c"]);
        for id in ["a", "gone", "c"] {
            app.select_conversation(&workspace, id);
            settle(&mut app);
        }
        app.doc.workspaces[0].conversations.retain(|conversation| conversation.id != "gone");
        assert_eq!(app.conversation_history_target(-1).unwrap().conversation_id, "a");
    }

    #[test]
    fn stepping_through_a_project_wraps_at_either_end() {
        let mut app = app("step");
        let workspace = with_conversations(&mut app, &["a", "b"]);
        app.select_conversation(&workspace, "b");
        app.step_conversation(1);
        assert_eq!(app.active_conversation_id().as_deref(), Some("a"));
        app.step_conversation(-1);
        assert_eq!(app.active_conversation_id().as_deref(), Some("b"));
    }

    #[test]
    fn opening_an_unloaded_conversation_loads_its_body() {
        let mut stored = conversation("c");
        stored.contexts = vec![text("u", "user")];
        let reply = serde_json::to_value(&stored).unwrap();
        let mut app = app_with(Arc::new(move |command, _| if command == "load_conversation" { Ok(reply.clone()) } else { Ok(Value::Null) }), "open-unloaded");
        app.doc.workspaces[0].conversations.insert(0, conversation("c"));
        app.unloaded.insert("c".into());
        let workspace = app.doc.workspaces[0].id.clone();
        app.select_conversation(&workspace, "c");
        assert!(app.active_body_unloaded());
        pump_until(&mut app, "the body", |app| !app.is_body_unloaded("c"));
        assert_eq!(app.conversation("c").unwrap().contexts.len(), 1);
    }

    #[test]
    fn opening_a_conversation_that_failed_to_load_tries_again() {
        let fail = Arc::new(std::sync::Mutex::new(true));
        let failing = fail.clone();
        let reply = serde_json::to_value(conversation("c")).unwrap();
        let mut app = app_with(Arc::new(move |command, _| {
            if command != "load_conversation" {
                return Ok(Value::Null);
            }
            if *failing.lock().unwrap() { Err("gone".to_owned()) } else { Ok(reply.clone()) }
        }), "reopen-after-failure");
        let workspace = with_conversations(&mut app, &["other"]);
        app.doc.workspaces[0].conversations.insert(0, conversation("c"));
        app.unloaded.insert("c".into());
        app.select_conversation(&workspace, "c");
        pump_until(&mut app, "the failure", |app| app.body_load_failure("c").is_some());
        // Staying on it doesn't hammer the host: the retry button is for that.
        settle(&mut app);
        assert!(app.body_load_failure("c").is_some());
        app.select_conversation(&workspace, "other");
        settle(&mut app);
        *fail.lock().unwrap() = false;
        app.select_conversation(&workspace, "c");
        pump_until(&mut app, "the retried body", |app| !app.is_body_unloaded("c"));
        assert_eq!(app.body_load_failure("c"), None);
    }

    #[test]
    fn lists_the_projects_tasks_for_the_landing_most_recent_first() {
        let mut app = app("landing");
        let workspace = with_conversations(&mut app, &["old", "new"]);
        app.doc.workspaces[0].conversations[0].updated_at = "2026-01-01T00:00:00Z".into();
        app.doc.workspaces[0].conversations[1].updated_at = "2026-02-01T00:00:00Z".into();
        app.doc.workspaces[0].conversations.insert(0, conversation("empty"));
        app.select_conversation(&workspace, "old");
        assert!(app.draft_landing_tasks().is_empty(), "only a task nothing has been sent in shows them");
        app.open_draft_conversation(Some(&workspace));
        let tasks: Vec<String> = app.draft_landing_tasks().into_iter().map(|task| task.id).collect();
        assert_eq!(tasks, vec!["new", "old"]);
    }

    #[test]
    fn a_selection_past_the_end_falls_back_to_the_first_workspace() {
        let mut app = app("member");
        let workspace = with_conversations(&mut app, &["a"]);
        app.doc.workspaces[0].kind = crate::model::WorkspaceKind::Directory;
        app.doc.workspaces[0].id = "project".into();
        let _ = workspace;
        app.select_conversation("project", "a");
        app.select_workspace_member("a", 5);
        assert_eq!(app.active_workspace_member(), 1);
        app.doc.workspaces[0].additional_workspaces = vec![AttachedWorkspace { machine: None, path: "/two".into() }];
        app.select_workspace_member("a", 2);
        assert_eq!(app.active_workspace_member(), 2);
    }
}
