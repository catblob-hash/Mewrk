//! `commitTimelineEdit` and `stepTimelineHistory` of `App.tsx`: edits on a timeline, and taking them back. Runs matter
//! here twice: a patch lands by id so a reply the model added since an edit is kept, and nothing moves while a turn is
//! in progress.

use std::time::Duration;

use super::seams::workspace_of;
use super::state::RunsState;
use super::timeline_history::*;
use super::util::{create_id, now_iso};
use crate::conversation_branches::is_conversation_branch_fork;
use crate::drafts::is_draft_conversation_id;
use crate::i18n::{t, tf};
use crate::model::ContextItem;
use crate::App;

/// How long a notice stays up.
pub const TIMELINE_NOTICE_MS: u64 = 3200;

/// `TimelineNoticeState`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineNotice {
    pub id: String,
    pub conversation_id: Option<String>,
    pub message: String,
    /// The keys that act on it, and what they do: `(keys, action)`.
    pub hint: Option<(String, String)>,
}

enum PatchRefusal {
    Absent,
    Branch,
}

impl App {
    /// `timelineNotice`.
    pub fn timeline_notice(&mut self) -> Option<TimelineNotice> {
        self.runs().timeline_notice.clone()
    }

    /// `showTimelineNotice`.
    pub fn show_timeline_notice(&mut self, conversation_id: Option<&str>, message: impl Into<String>, hint: Option<(String, String)>, duration_ms: u64) {
        let id = create_id("notice");
        let state = self.runs();
        state.timeline_notice = Some(TimelineNotice { id: id.clone(), conversation_id: conversation_id.map(str::to_owned), message: message.into(), hint });
        if let Some(timer) = self.local::<RunsState>().timeline_notice_timer.take() {
            self.clear_timer(timer);
        }
        let timer = self.set_timer(Duration::from_millis(duration_ms), move |app| {
            let state = app.local::<RunsState>();
            state.timeline_notice_timer = None;
            if state.timeline_notice.as_ref().is_some_and(|notice| notice.id == id) {
                state.timeline_notice = None;
            }
            crate::prelude::repaint();
        });
        self.local::<RunsState>().timeline_notice_timer = Some(timer);
        crate::prelude::repaint();
    }

    /// `timelineHistoryKey`: a draft's history goes with the id it will materialize as.
    fn timeline_history_key(&mut self, conversation_id: &str) -> String {
        self.draft_of(conversation_id).map_or_else(|| conversation_id.to_owned(), |draft| draft.materializes_as)
    }

    /// `timelineContextsOf`.
    pub fn timeline_contexts_of(&mut self, conversation_id: &str) -> Vec<ContextItem> {
        if is_draft_conversation_id(conversation_id) {
            return self.draft_of(conversation_id).map(|draft| draft.contexts).unwrap_or_default();
        }
        self.conversation(conversation_id).map(|conversation| conversation.contexts.clone()).unwrap_or_default()
    }

    /// `writeTimelinePatch`: writes a timeline patch onto a conversation, the draft included, and says what came of it.
    fn write_timeline_patch(&mut self, conversation_id: &str, patch: &TimelinePatch) -> Result<Vec<ContextItem>, PatchRefusal> {
        if is_draft_conversation_id(conversation_id) {
            let Some(draft) = self.draft_of(conversation_id) else { return Err(PatchRefusal::Absent) };
            if self.active_conversation_id().as_deref() != Some(conversation_id) {
                return Err(PatchRefusal::Absent);
            }
            let contexts = apply_timeline_patch(&draft.contexts, patch).ok_or(PatchRefusal::Absent)?;
            let written = contexts.clone();
            self.update_draft(&draft.workspace_id, move |current| {
                let mut next = current.clone();
                next.contexts = written;
                Some(next)
            });
            return Ok(contexts);
        }
        let Some(workspace_id) = workspace_of(&self.doc, conversation_id).map(|workspace| workspace.id.clone()) else { return Err(PatchRefusal::Absent) };
        let Some(conversation) = self.conversation(conversation_id) else { return Err(PatchRefusal::Absent) };
        // A message a branch was taken from anchors that branch; taking it out would orphan it.
        if removed_ids(patch).iter().any(|id| is_conversation_branch_fork(conversation, id)) {
            return Err(PatchRefusal::Branch);
        }
        let contexts = apply_timeline_patch(&conversation.contexts, patch).ok_or(PatchRefusal::Absent)?;
        let written = contexts.clone();
        self.update_conversation(&workspace_id, conversation_id, move |current| {
            current.contexts = written;
            current.updated_at = now_iso();
        });
        Ok(contexts)
    }

    /// `commitTimelineEdit`: makes an edit on a timeline and files it in that timeline's history, so the undo keys can
    /// take it back. Every edit made on the timeline goes through here: a delete, a placed or rewritten card, a run of a
    /// tool by hand. `false` when it could not land.
    pub fn commit_timeline_edit(&mut self, conversation_id: &str, patch: TimelinePatch, label: impl Into<String>, notice: Option<String>) -> bool {
        let Ok(contexts) = self.write_timeline_patch(conversation_id, &patch) else { return false };
        let key = self.timeline_history_key(conversation_id);
        self.timeline_history().record(&key, TimelineHistoryEntry { patch, label: label.into() });
        let usage = self.estimate_context_usage(&contexts);
        self.runs().context_usage.insert(conversation_id.to_owned(), usage);
        if let Some(notice) = notice {
            let keys = timeline_history_shortcut(TimelineHistoryAction::Undo, IS_APPLE_PLATFORM);
            self.show_timeline_notice(Some(conversation_id), notice, Some((keys, t("撤回", "to undo").to_owned())), TIMELINE_NOTICE_MS);
        }
        true
    }

    /// `stepTimelineHistory`: takes back the last edit on a timeline, or makes the last one taken back again.
    pub fn step_timeline_history(&mut self, conversation_id: &str, action: TimelineHistoryAction) {
        let key = self.timeline_history_key(conversation_id);
        let undo = action == TimelineHistoryAction::Undo;
        let entry = if undo { self.timeline_history().next_undo(&key).cloned() } else { self.timeline_history().next_redo(&key).cloned() };
        let notice = |app: &mut App, message: String, hint: Option<(String, String)>| app.show_timeline_notice(Some(conversation_id), message, hint, TIMELINE_NOTICE_MS);
        let Some(entry) = entry else {
            let message = if undo { t("没有可撤回的修改", "Nothing to undo") } else { t("没有可重做的修改", "Nothing to redo") };
            return notice(self, message.to_owned(), None);
        };
        if self.context_mutation_is_blocked(conversation_id) {
            return notice(self, t("模型回合进行中，暂时不能修改上下文", "Context cannot be changed while a model turn is in progress").to_owned(), None);
        }
        let patch = if undo { invert_timeline_patch(&entry.patch) } else { entry.patch.clone() };
        let contexts = match self.write_timeline_patch(conversation_id, &patch) {
            Ok(contexts) => contexts,
            Err(PatchRefusal::Branch) => return notice(self, t("有分支从这条消息开始，不能撤回", "A branch starts at this message, so this cannot be undone").to_owned(), None),
            Err(PatchRefusal::Absent) => {
                // Nothing of it is left to change: whatever it touched is gone, or back already.
                if undo {
                    self.timeline_history().drop_undo(&key);
                } else {
                    self.timeline_history().drop_redo(&key);
                }
                return notice(self, t("这次修改已不在时间线上", "That edit is no longer on the timeline").to_owned(), None);
            }
        };
        if undo {
            self.timeline_history().undone(&key);
        } else {
            self.timeline_history().redone(&key);
        }
        self.close_editors();
        let usage = self.estimate_context_usage(&contexts);
        self.runs().context_usage.insert(conversation_id.to_owned(), usage);
        let (message, hint) = if undo {
            (tf("已撤回：{label}", "Undone: {label}", &[("label", &entry.label)]), (timeline_history_shortcut(TimelineHistoryAction::Redo, IS_APPLE_PLATFORM), t("重做", "to redo").to_owned()))
        } else {
            (tf("已重做：{label}", "Redone: {label}", &[("label", &entry.label)]), (timeline_history_shortcut(TimelineHistoryAction::Undo, IS_APPLE_PLATFORM), t("撤回", "to undo").to_owned()))
        };
        notice(self, message, Some(hint));
    }
}
