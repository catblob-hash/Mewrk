//! `components/RewindDialog.tsx` and `lib/conversationRewind.ts`: the window "rewind to before this message" opens. It is the
//! confirmation too: it lists what the files would go through (a dry run, nothing touched), then the user picks which of the
//! conversation and the files to put back.

use mewrk_client::conversation_branches::is_conversation_branch_fork;
use mewrk_client::model::{ContextItem, Conversation};
use mewrk_client::prelude::*;
use serde::{Deserialize, Serialize};

/// `RewindFileEntry`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindFileEntry {
    pub path: String,
    pub display_path: String,
    /// `restore` or `delete`.
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// `RewindFilesResponse`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindFilesResponse {
    pub files: Vec<RewindFileEntry>,
}

/// Which halves of "back to before this message" the user chose to carry out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewindScope {
    Both,
    Conversation,
    Files,
}

/// `RewindRange`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindRange {
    pub ids: Vec<String>,
    /// A branch was taken from one of them.
    pub anchored: bool,
}

/// `rewindRange`: what rewinding to before user message `context_id` removes, or `None` when the timeline has no such message.
pub fn rewind_range(conversation: &Conversation, context_id: &str) -> Option<RewindRange> {
    let start = conversation.contexts.iter().position(|context| context.id() == context_id && matches!(context, ContextItem::User { .. }))?;
    let ids: Vec<String> = conversation.contexts[start..].iter().map(|context| context.id().to_owned()).collect();
    let anchored = ids.iter().any(|id| is_conversation_branch_fork(conversation, id));
    Some(RewindRange { ids, anchored })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RewindFilesSummary {
    pub restored: usize,
    pub deleted: usize,
    pub skipped: usize,
    pub first_skip_reason: Option<String>,
}

pub fn summarize_rewind_files(files: &[RewindFileEntry]) -> RewindFilesSummary {
    let mut summary = RewindFilesSummary::default();
    for entry in files {
        if let Some(reason) = &entry.skipped {
            summary.skipped += 1;
            summary.first_skip_reason.get_or_insert_with(|| reason.clone());
        } else if entry.action == "delete" {
            summary.deleted += 1;
        } else {
            summary.restored += 1;
        }
    }
    summary
}

fn restore_clause(count: usize, done: bool) -> String {
    match (done, count == 1) {
        (true, true) => t("恢复了 1 个文件", "restored 1 file").to_owned(),
        (true, false) => tf("恢复了 {count} 个文件", "restored {count} files", &[("count", &count)]),
        (false, true) => t("恢复 1 个文件", "restore 1 file").to_owned(),
        (false, false) => tf("恢复 {count} 个文件", "restore {count} files", &[("count", &count)]),
    }
}

fn delete_clause(count: usize, done: bool) -> String {
    match (done, count == 1) {
        (true, true) => t("删除了 1 个之后新建的文件", "deleted 1 file created since then").to_owned(),
        (true, false) => tf("删除了 {count} 个之后新建的文件", "deleted {count} files created since then", &[("count", &count)]),
        (false, true) => t("删除 1 个之后新建的文件", "delete 1 file created since then").to_owned(),
        (false, false) => tf("删除 {count} 个之后新建的文件", "delete {count} files created since then", &[("count", &count)]),
    }
}

fn clauses(summary: &RewindFilesSummary, done: bool) -> Vec<String> {
    let mut parts = Vec::new();
    if summary.restored > 0 {
        parts.push(restore_clause(summary.restored, done));
    }
    if summary.deleted > 0 {
        parts.push(delete_clause(summary.deleted, done));
    }
    parts
}

/// The one line the dialog says over its file list, before anything is touched.
pub fn rewind_preview_summary(summary: &RewindFilesSummary) -> String {
    let parts = clauses(summary, false);
    let lead = if parts.is_empty() { t("没有文件会被恢复", "No file will be restored").to_owned() } else { tf("将{parts}", "Will {parts}", &[("parts", &parts.join(t("，", ", ")))]) };
    if summary.skipped > 0 { tf("{lead}；{count} 个会被跳过", "{lead}; {count} will be skipped", &[("lead", &lead), ("count", &summary.skipped)]) } else { lead }
}

/// What a restore did, for the notice: the counts, and the first reason a file was left alone.
pub fn rewind_result_summary(summary: &RewindFilesSummary) -> String {
    let parts = clauses(summary, true);
    let done = if parts.is_empty() { t("没有文件被恢复", "no file was restored").to_owned() } else { parts.join(t("，", ", ")) };
    if summary.skipped == 0 {
        return done;
    }
    tf("{done}；跳过 {count} 个：{reason}", "{done}; skipped {count}: {reason}", &[("done", &done), ("count", &summary.skipped), ("reason", &summary.first_skip_reason.clone().unwrap_or_default())])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RewindPreview {
    Loading,
    Ready(Vec<RewindFileEntry>),
    Error(String),
}

/// What the dialog keeps while it is up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindDialogState {
    pub conversation_id: String,
    pub context_id: String,
    pub preview: RewindPreview,
    pub busy: bool,
    pub action_error: Option<String>,
}

pub struct RewindDialogProps {
    pub state: RewindDialogState,
    /// Why nothing may be rewound right now (a model turn is running).
    pub blocked_reason: Option<String>,
    /// Why the conversation's own timeline cannot give the messages up (a branch was taken from one of them).
    pub conversation_blocked_reason: Option<String>,
    pub on_run: Rc<dyn Fn(&mut App, RewindScope)>,
    pub on_close: Handler<App>,
}

const TEXT: TypeStyle = TypeStyle { size: 12.5, line: 18.0, weight: Weight::Regular, mono: false, italic: false, serif: false };

pub fn rewind_dialog(props: RewindDialogProps) -> E {
    let t_ = theme();
    let RewindDialogProps { state, blocked_reason, conversation_blocked_reason, on_run, on_close } = props;
    let blocked = blocked_reason.is_some();
    let busy = state.busy;
    let files: &[RewindFileEntry] = if let RewindPreview::Ready(files) = &state.preview { files } else { &[] };
    let summary = summarize_rewind_files(files);
    let has_files = summary.restored + summary.deleted > 0;
    let files_allowed = !blocked && !busy && matches!(state.preview, RewindPreview::Ready(_)) && has_files;
    let conversation_allowed = !blocked && !busy && conversation_blocked_reason.is_none();
    let warning = |s: &str| text(s.to_owned()).ty(TEXT).color(t_.caution).role(Role::Status);
    let failure = |s: &str| text(s.to_owned()).ty(TEXT).color(t_.critical).role(Role::Alert);
    let mut body = col().p(20.0).gap(10.0).min_w(0.0).child(text(t("回退到这条消息之前", "Rewind to before this message")).ty(SUBTITLE).color(t_.text_primary));
    if let Some(reason) = &blocked_reason {
        body = body.child(warning(reason));
    }
    if !blocked {
        match &state.preview {
            RewindPreview::Loading => body = body.child(text(t("正在检查文件改动…", "Checking file changes…")).ty(TEXT).color(t_.text_secondary).role(Role::Status)),
            RewindPreview::Error(message) => {
                let unknown = t("未知错误", "Unknown error");
                body = body.child(failure(&tf("无法读取文件改动：{error}", "Could not read the file changes: {error}", &[("error", &if message.is_empty() { unknown } else { message.as_str() })])));
            }
            RewindPreview::Ready(files) if files.is_empty() => body = body.child(text(t("这条消息之后没有可恢复的文件改动", "There are no file changes to restore after this message")).ty(TEXT).color(t_.text_secondary)),
            RewindPreview::Ready(files) => {
                body = body.child(text(rewind_preview_summary(&summary)).ty(TEXT).color(t_.text_primary));
                let mut list = col().key(("rewind-files", state.context_id.as_str())).max_h(280.0).scroll_y().gap(2.0);
                for entry in files {
                    let tag = if entry.skipped.is_some() {
                        t("跳过", "Skipped")
                    } else if entry.action == "delete" {
                        t("删除", "Delete")
                    } else {
                        t("恢复", "Restore")
                    };
                    let mut line = row().items_center().gap(6.0).min_h(22.0).full_text(entry.path.clone()).child(mewrk_pane_files::icons::file_icon(&entry.path, 13.0)).child(text(entry.display_path.clone()).ty(TEXT).color(t_.text_primary).ellipsis().flex1()).child(text(tag).ty(FINE).color(if entry.action == "delete" && entry.skipped.is_none() { t_.critical } else { t_.text_tertiary }).nowrap());
                    if let Some(reason) = &entry.skipped {
                        line = line.child(text(reason.clone()).ty(FINE).color(t_.text_tertiary).nowrap());
                    }
                    list = list.child(line);
                }
                body = body.child(list);
            }
        }
    }
    body = body.child(row().wrap().items_center().gap(4.0).child(text(t("只恢复 write / edit 工具的改动。", "Only write and edit tool changes are restored.")).ty(CAPTION).color(t_.text_tertiary)).child(crate::kit::docs_link("context", "undo")));
    if let (Some(reason), false) = (&conversation_blocked_reason, blocked) {
        body = body.child(warning(reason));
    }
    if let Some(error) = &state.action_error {
        body = body.child(failure(error));
    }
    let close = on_close.clone();
    let cancel = button(t("取消", "Cancel")).key("rewind-cancel").disabled(busy).on_click(move |app| {
        if !busy {
            close(app);
        }
    });
    let action = |look: E, key: &'static str, allowed: bool, scope: RewindScope| {
        let run = on_run.clone();
        look.key(key).disabled(!allowed).on_click(move |app| {
            if allowed {
                run(app, scope);
            }
        })
    };
    body = body.child(
        row()
            .wrap()
            .gap(6.0)
            .justify_end()
            .mt(4.0)
            .child(cancel)
            .child(action(button(t("仅恢复文件", "Restore files only")), "rewind-files-only", files_allowed, RewindScope::Files))
            .child(action(button(t("仅恢复对话", "Restore conversation only")), "rewind-conversation-only", conversation_allowed, RewindScope::Conversation))
            .child(action(danger_button(t("恢复对话和文件", "Restore conversation and files")), "rewind-both", files_allowed && conversation_allowed, RewindScope::Both)),
    );
    let backdrop = on_close.clone();
    dialog(560.0, body, (!busy).then(|| -> Handler<App> { Rc::new(move |app| backdrop(app)) })).aria_label(t("回退到这条消息之前", "Rewind to before this message"))
}
