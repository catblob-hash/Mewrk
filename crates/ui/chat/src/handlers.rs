//! The timeline handlers of `App.tsx`: `handleContextEdit`, `deleteContext`, `saveTextContext`, `saveToolContext`,
//! `saveToolContextEdit`, `saveQuestionContext`, `handleContextInsert`, `deleteTimelineContexts`, `moveTimelineContexts`,
//! `copyTimelineContexts`, `pasteTimelineContexts`, `forkConversationAt`, `openRewindDialog`, and the attest and rewind commands
//! behind them. [`timeline_props`] hands them to a view as props; [`timeline_dialogs`] draws what they ask questions with.

use std::collections::HashSet;
use std::path::PathBuf;

use mewrk_client::conversation_branches::{context_branch_navigations, is_conversation_branch_fork};
use mewrk_client::drafts::is_draft_conversation_id;
use mewrk_client::model::{Conversation, ContextItem, FileAttachment, ImageAttachment, JsonObject, ToolDescriptor, ToolExecutionRequest, ToolResult};
use mewrk_client::model_capabilities::{is_encrypted_reasoning, supports_vision};
use mewrk_client::prelude::*;
use mewrk_client::runs::attachments::{bytes_to_base64, read_document, sort_attachments, AttachmentRejection, AttachmentRejectionReason, ComposerFile, Intake, MessageAttachmentRoom};
use mewrk_client::runs::image_short_ids::{image_short_ids_in_use, next_image_short_id, reserve_queued_message_ids};
use mewrk_client::runs::timeline_history::{insertion_patch, move_patch, paste_patch, placed_contexts, timeline_history_shortcut, IS_APPLE_PLATFORM};
use mewrk_client::runs::util::{create_id, now_iso};
use mewrk_client::runs::{TimelineHistoryAction, TimelinePatch};

use crate::clipboard::{copies_for_paste, timeline_clipboard_text, timeline_copyable, timeline_movable, with_copied_tool_proofs, TimelineClipboard};
use crate::inline_text_editor::{AddAttachments, AddedAttachments, InsertKind};
use crate::inline_tool_editor::ToolDone;
use crate::kit::{copy_text, UiBound};
use crate::orchestration::{answers_from_formatted_content, is_claude_question_input, questions_from_input};
use crate::props::{ConversationViewProps, TimelineEditorState, TimelineQuestionEditorState};
use crate::rewind::{rewind_dialog, rewind_range, rewind_result_summary, summarize_rewind_files, RewindDialogProps, RewindDialogState, RewindFilesResponse, RewindPreview, RewindScope};

/// A question put to the user before a destructive act.
#[derive(Clone)]
pub struct Confirmation {
    pub question: String,
    pub confirm_label: String,
    pub destructive: bool,
    pub on_confirm: Handler<App>,
}

/// What `App.tsx` kept for the timeline: `editor`, `questionEditor`, `timelineClipboard`, `confirmation`, `rewindTarget`.
#[derive(Default)]
pub struct TimelineEdits {
    pub editor: Option<TimelineEditorState>,
    pub question_editor: Option<(String, TimelineQuestionEditorState)>,
    pub clipboard: Option<TimelineClipboard>,
    pub confirmation: Option<Confirmation>,
    pub rewind: Option<RewindDialogState>,
    hooked: bool,
}

fn edits(app: &mut App) -> &mut TimelineEdits {
    app.local::<TimelineEdits>()
}

/// `closeTimelineEditors`.
pub fn close_timeline_editors(app: &mut App) {
    let state = edits(app);
    state.editor = None;
    state.question_editor = None;
    repaint();
}

/// `activeConversation`, with the draft standing in where one is active.
fn active(app: &mut App) -> Option<String> {
    app.active_conversation_id()
}

/// `(!activeWorkspaceId && !draftActive)`: a draft has no workspace until it is sent, yet its hand-written content is still deletable.
fn nowhere_to_edit(app: &mut App, conversation_id: &str) -> bool {
    app.active_workspace_id().is_none() && !is_draft_conversation_id(conversation_id)
}

/// `handleContextInsert`.
pub fn handle_context_insert(app: &mut App, index: usize, kind: InsertKind, tool_name: Option<String>) {
    let Some(conversation) = active(app) else { return };
    if app.context_mutation_is_blocked(&conversation) {
        return;
    }
    let state = edits(app);
    state.question_editor = None;
    state.editor = Some(TimelineEditorState::Insert { kind, index, tool_name });
    repaint();
}

/// `handleContextEdit`.
pub fn handle_context_edit(app: &mut App, item: &ContextItem) {
    let Some(conversation) = active(app) else { return };
    if app.context_mutation_is_blocked(&conversation) {
        return;
    }
    // Encrypted reasoning is delete-only. Guard every edit entry point so user text can never be persisted as model reasoning.
    if let ContextItem::Reasoning { content, form, .. } = item
        && is_encrypted_reasoning(*form, content.as_deref())
    {
        return;
    }
    let contexts = app.timeline_contexts_of(&conversation);
    let index = contexts.iter().position(|context| context.id() == item.id()).unwrap_or(contexts.len());
    let state = edits(app);
    state.question_editor = None;
    state.editor = Some(TimelineEditorState::Edit { kind: crate::frame::insert_kind_of(item), item: item.clone(), index });
    repaint();
}

/// `handleQuestionEdit`.
pub fn handle_question_edit(app: &mut App, item: &ContextItem, answer: Option<&ContextItem>) {
    let Some(conversation) = active(app) else { return };
    if app.context_mutation_is_blocked(&conversation) {
        return;
    }
    let contexts = app.timeline_contexts_of(&conversation);
    if !contexts.iter().any(|context| context.id() == item.id()) || answer.is_some_and(|answer| !contexts.iter().any(|context| context.id() == answer.id())) {
        return;
    }
    let state = edits(app);
    state.editor = None;
    state.question_editor = Some((conversation, TimelineQuestionEditorState { item: item.clone(), answer: answer.cloned() }));
    repaint();
}

fn branch_anchored(app: &App, conversation: &str, id: &str) -> bool {
    app.conversation(conversation).is_some_and(|conversation: &Conversation| is_conversation_branch_fork(conversation, id))
}

fn deleted_messages_notice(count: usize) -> String {
    if count == 1 { t("已删除 1 条消息", "Deleted 1 message").to_owned() } else { tf("已删除 {count} 条消息", "Deleted {count} messages", &[("count", &count)]) }
}

/// `deleteContext`. A confirmed delete runs again from the top, against the timeline as it is once the user has answered
/// rather than as it was when the question went up.
pub fn delete_context(app: &mut App, item: &ContextItem, confirmed: bool) {
    let Some(conversation) = active(app) else { return };
    if nowhere_to_edit(app, &conversation) || app.context_mutation_is_blocked(&conversation) || branch_anchored(app, &conversation, item.id()) {
        return;
    }
    let contexts = app.timeline_contexts_of(&conversation);
    let removed = placed_contexts(&contexts, &HashSet::from([item.id().to_owned()]));
    if removed.is_empty() {
        return;
    }
    if app.appearance_snapshot().confirm_message_delete && !confirmed {
        let item = item.clone();
        edits(app).confirmation = Some(Confirmation { question: t("确定删除这条上下文？", "Delete this context item?").to_owned(), confirm_label: t("删除", "Delete").to_owned(), destructive: true, on_confirm: Rc::new(move |app| delete_context(app, &item, true)) });
        repaint();
        return;
    }
    let label = if matches!(item, ContextItem::Tool { .. }) { t("删除工具调用", "Delete the tool call") } else { t("删除上下文", "Delete the context") };
    app.commit_timeline_edit(&conversation, TimelinePatch { removed, ..TimelinePatch::default() }, label, Some(deleted_messages_notice(1)));
    repaint();
}

/// `deleteQuestionContext`: the question and its paired answer go as one message.
pub fn delete_question_context(app: &mut App, item: &ContextItem, answer: Option<&ContextItem>) {
    let Some(conversation) = active(app) else { return };
    if app.active_workspace_id().is_none() || app.context_mutation_is_blocked(&conversation) {
        return;
    }
    let mut ids: HashSet<String> = HashSet::from([item.id().to_owned()]);
    ids.extend(answer.map(|answer| answer.id().to_owned()));
    if ids.iter().any(|id| branch_anchored(app, &conversation, id)) {
        return;
    }
    let contexts = app.timeline_contexts_of(&conversation);
    let removed = placed_contexts(&contexts, &ids);
    if removed.len() != ids.len() {
        return;
    }
    edits(app).question_editor = None;
    let label = if answer.is_some() { t("删除整条提问消息", "Delete the complete question message") } else { t("删除提问消息", "Delete the question message") };
    app.commit_timeline_edit(&conversation, TimelinePatch { removed, ..TimelinePatch::default() }, label, Some(deleted_messages_notice(1)));
    repaint();
}

/// `deleteTimelineContexts`: everything a selection box picked out, as one edit with one undo. A message a branch was taken from
/// stays: it anchors that branch, and the single delete refuses it too.
pub fn delete_timeline_contexts(app: &mut App, ids: Vec<String>, confirmed: bool) {
    let Some(conversation) = active(app) else { return };
    if nowhere_to_edit(app, &conversation) || app.context_mutation_is_blocked(&conversation) {
        return;
    }
    let doomed: HashSet<String> = ids.iter().filter(|id| !branch_anchored(app, &conversation, id)).cloned().collect();
    let contexts = app.timeline_contexts_of(&conversation);
    let removed = placed_contexts(&contexts, &doomed);
    if removed.is_empty() {
        return;
    }
    if app.appearance_snapshot().confirm_message_delete && !confirmed {
        let question = tf("确定删除选中的 {count} 条上下文？", "Delete the {count} selected context items?", &[("count", &removed.len())]);
        edits(app).confirmation = Some(Confirmation { question, confirm_label: t("删除", "Delete").to_owned(), destructive: true, on_confirm: Rc::new(move |app| delete_timeline_contexts(app, ids.clone(), true)) });
        repaint();
        return;
    }
    let state = edits(app);
    if matches!(&state.editor, Some(TimelineEditorState::Edit { item, .. }) if doomed.contains(item.id())) {
        state.editor = None;
    }
    if state.question_editor.as_ref().is_some_and(|(_, editor)| doomed.contains(editor.item.id())) {
        state.question_editor = None;
    }
    let count = removed.len();
    let label = if count == 1 { t("删除 1 条消息", "Delete 1 message").to_owned() } else { tf("删除 {count} 条消息", "Delete {count} messages", &[("count", &count)]) };
    app.commit_timeline_edit(&conversation, TimelinePatch { removed, ..TimelinePatch::default() }, label, Some(deleted_messages_notice(count)));
    repaint();
}

/// `moveTimelineContexts`: gathered together in timeline order at the insertion line, as one edit with one undo. A message a
/// branch was taken from stays where it is, and so does a compaction, which every request starts from.
pub fn move_timeline_contexts(app: &mut App, ids: Vec<String>, index: usize) {
    let Some(conversation) = active(app) else { return };
    if nowhere_to_edit(app, &conversation) || app.context_mutation_is_blocked(&conversation) {
        return;
    }
    let picked: HashSet<String> = ids.into_iter().collect();
    let contexts = app.timeline_contexts_of(&conversation);
    let chosen: Vec<&ContextItem> = contexts.iter().filter(|context| picked.contains(context.id())).collect();
    let moving: HashSet<String> = chosen.iter().filter(|context| timeline_movable(context) && !branch_anchored(app, &conversation, context.id())).map(|context| context.id().to_owned()).collect();
    let Some(patch) = move_patch(&contexts, &moving, index as isize) else {
        if moving.len() < chosen.len() {
            app.show_timeline_notice(Some(&conversation), t("这些消息不能移动", "These messages cannot be moved"), None, mewrk_client::runs::timeline_edits::TIMELINE_NOTICE_MS);
        }
        return;
    };
    // A card being placed is placed by index, and the indexes are about to change.
    if matches!(edits(app).editor, Some(TimelineEditorState::Insert { .. })) {
        edits(app).editor = None;
    }
    let count = moving.len();
    let held = chosen.len() - count;
    let label = if count == 1 { t("移动 1 条消息", "Move 1 message").to_owned() } else { tf("移动 {count} 条消息", "Move {count} messages", &[("count", &count)]) };
    let notice = if held > 0 {
        tf("已移动 {count} 条消息，{held} 条不能移动", "Moved {count} messages; {held} cannot be moved", &[("count", &count), ("held", &held)])
    } else if count == 1 {
        t("已移动 1 条消息", "Moved 1 message").to_owned()
    } else {
        tf("已移动 {count} 条消息", "Moved {count} messages", &[("count", &count)])
    };
    app.commit_timeline_edit(&conversation, patch, label, Some(notice));
    repaint();
}

/// `copyTimelineContexts`: the cards themselves for a paste on a timeline, and their text for a paste anywhere else. What cannot be
/// copied is left out, and said to be.
pub fn copy_timeline_contexts(app: &mut App, ids: Vec<String>, tools: &[ToolDescriptor]) {
    let Some(conversation) = active(app) else { return };
    let picked: HashSet<String> = ids.into_iter().collect();
    let all = app.timeline_contexts_of(&conversation);
    let chosen: Vec<&ContextItem> = all.iter().filter(|context| picked.contains(context.id())).collect();
    let contexts: Vec<ContextItem> = chosen.iter().copied().filter(|context| timeline_copyable(context, false, tools)).cloned().collect();
    const MS: u64 = mewrk_client::runs::timeline_edits::TIMELINE_NOTICE_MS;
    if contexts.is_empty() {
        app.show_timeline_notice(Some(&conversation), t("选中的消息不能复制", "The selected messages cannot be copied"), None, MS);
        return;
    }
    copy_text(timeline_clipboard_text(&contexts));
    let count = contexts.len();
    let left = chosen.len() - count;
    edits(app).clipboard = Some(TimelineClipboard { conversation_id: conversation.clone(), contexts });
    let notice = if left > 0 {
        tf("已复制 {count} 条消息，{left} 条不能复制", "Copied {count} messages; {left} cannot be copied", &[("count", &count), ("left", &left)])
    } else if count == 1 {
        t("已复制 1 条消息", "Copied 1 message").to_owned()
    } else {
        tf("已复制 {count} 条消息", "Copied {count} messages", &[("count", &count)])
    };
    app.show_timeline_notice(Some(&conversation), notice, None, MS);
    repaint();
}

/// `conversationForTimelineTool`: the conversation a tool the user runs or records on the timeline goes into, as the host knows it.
/// A tool card belongs to a workspace, so a draft materializes here and is fixed in its project from then on.
fn conversation_for_timeline_tool(app: &mut App, done: impl FnOnce(&mut App, Result<(String, String), String>) + Send + 'static) {
    let Some(conversation) = active(app) else { return done(app, Err(t("没有活动对话", "No active conversation").to_owned())) };
    if !is_draft_conversation_id(&conversation) {
        let Some(workspace) = app.active_workspace_id() else { return done(app, Err(t("没有活动对话", "No active conversation").to_owned())) };
        app.flush_latest_document(false, move |app, flushed| done(app, flushed.map(|()| (workspace, conversation))));
        return;
    }
    // An upload still addressed to the draft would be dropped once the draft is gone.
    if app.composer().image_loading_ids.contains(&conversation) {
        return done(app, Err(t("输入框里的图片还在上传，稍后再运行工具", "Images in the composer are still uploading; run the tool once they finish").to_owned()));
    }
    app.redeem_draft(true, move |app, redeemed| {
        let result = match redeemed {
            Ok(Some(redeemed)) => Ok((redeemed.workspace_id, redeemed.conversation_id)),
            Ok(None) => Err(t("没有活动对话", "No active conversation").to_owned()),
            Err(reason) => Err(tf("工具没有运行：{reason}", "The tool did not run: {reason}", &[("reason", &if reason.is_empty() { t("原因不明", "unknown reason").to_owned() } else { reason })])),
        };
        done(app, result);
    });
}

/// `pasteTimelineContexts`: what was copied, as new cards: at the insertion line at `index`, or, given contexts to replace, where the
/// first of them sat as they go. One edit, one undo. A copied tool card is only kept by the host with a proof issued for its new id
/// in this conversation, so the host is asked for one per card first.
pub fn paste_timeline_contexts(app: &mut App, index: usize, replaced_ids: Vec<String>) {
    let Some(clipboard) = edits(app).clipboard.clone().filter(|clipboard| !clipboard.contexts.is_empty()) else { return };
    let Some(conversation) = active(app) else { return };
    if nowhere_to_edit(app, &conversation) || app.context_mutation_is_blocked(&conversation) {
        return;
    }
    // The line is remembered by the context above it: the wait below can outlast an index.
    let contexts = app.timeline_contexts_of(&conversation);
    let above = (index > 0).then(|| contexts.get(index.min(contexts.len()) - 1).map(|context| context.id().to_owned())).flatten();
    let failed = |app: &mut App, conversation: &str, reason: String| {
        let reason = if reason.is_empty() { t("原因不明", "unknown reason").to_owned() } else { reason };
        app.show_timeline_notice(Some(conversation), tf("没有粘贴：{reason}", "Nothing was pasted: {reason}", &[("reason", &reason)]), None, mewrk_client::runs::timeline_edits::TIMELINE_NOTICE_MS);
    };
    let has_tools = clipboard.contexts.iter().any(|context| matches!(context, ContextItem::Tool { .. }));
    let proceed = move |app: &mut App, target: String| {
        let current = app.timeline_contexts_of(&target);
        let queued = app.conversation(&target).map(|conversation| conversation.queued_messages.clone()).unwrap_or_default();
        let mut taken = reserve_queued_message_ids(image_short_ids_in_use(current.iter()), queued.iter());
        let mut mint = |prefix: &str| create_id(prefix);
        let copies = copies_for_paste(&clipboard, &target, &mut taken, &mut mint);
        if copies.contexts.is_empty() {
            app.show_timeline_notice(Some(&target), t("复制的工具卡带有与这段对话冲突的图片编号，不能粘贴到这里", "The copied tool cards carry image numbers this conversation already uses, so they cannot be pasted here"), None, mewrk_client::runs::timeline_edits::TIMELINE_NOTICE_MS);
            return;
        }
        let finish = move |app: &mut App, target: String, pasted: Vec<ContextItem>, skipped: usize, above: Option<String>, index: usize, replaced_ids: Vec<String>| {
            if app.context_mutation_is_blocked(&target) {
                return;
            }
            let contexts = app.timeline_contexts_of(&target);
            let replaced: HashSet<String> = replaced_ids.into_iter().filter(|id| !branch_anchored(app, &target, id)).collect();
            let above_at = above.as_deref().and_then(|above| contexts.iter().position(|context| context.id() == above));
            let at = if index == 0 { 0 } else { above_at.map_or(index.min(contexts.len()), |at| at + 1) };
            let Some(patch) = paste_patch(&contexts, &pasted, at as isize, &replaced) else { return };
            let state = edits(app);
            if matches!(&state.editor, Some(TimelineEditorState::Insert { .. })) || matches!(&state.editor, Some(TimelineEditorState::Edit { item, .. }) if replaced.contains(item.id())) {
                state.editor = None;
            }
            if state.question_editor.as_ref().is_some_and(|(_, editor)| replaced.contains(editor.item.id())) {
                state.question_editor = None;
            }
            let count = pasted.len();
            let label = if count == 1 { t("粘贴 1 条消息", "Paste 1 message").to_owned() } else { tf("粘贴 {count} 条消息", "Paste {count} messages", &[("count", &count)]) };
            let notice = if skipped > 0 {
                tf("已粘贴 {count} 条消息，{skipped} 条不能粘贴到这里", "Pasted {count} messages; {skipped} cannot be pasted here", &[("count", &count), ("skipped", &skipped)])
            } else if count == 1 {
                t("已粘贴 1 条消息", "Pasted 1 message").to_owned()
            } else {
                tf("已粘贴 {count} 条消息", "Pasted {count} messages", &[("count", &count)])
            };
            app.commit_timeline_edit(&target, patch, label, Some(notice));
            repaint();
        };
        if copies.tools.is_empty() {
            let (pasted, skipped) = (copies.contexts.clone(), copies.skipped);
            finish(app, target, pasted, skipped, above, index, replaced_ids);
            return;
        }
        let request = json!({ "conversationId": target, "copies": copies.tools.iter().map(|(context_id, source)| json!({ "sourceConversationId": clipboard.conversation_id, "source": source, "contextId": context_id })).collect::<Vec<_>>() });
        let held_target = target.clone();
        app.invoke::<Vec<String>>("attest_copied_tool_contexts", json!({ "request": request }), move |app, proofs| match proofs {
            Ok(proofs) => {
                let pasted = with_copied_tool_proofs(&copies, &proofs);
                finish(app, held_target, pasted, copies.skipped, above, index, replaced_ids);
            }
            Err(reason) => app.show_timeline_notice(Some(&held_target), tf("没有粘贴：{reason}", "Nothing was pasted: {reason}", &[("reason", &reason)]), None, mewrk_client::runs::timeline_edits::TIMELINE_NOTICE_MS),
        });
    };
    if has_tools {
        let origin = conversation.clone();
        conversation_for_timeline_tool(app, move |app, home| match home {
            Ok((_, target)) => {
                if !app.context_mutation_is_blocked(&target) {
                    proceed(app, target);
                }
            }
            Err(reason) => failed(app, &origin, reason),
        });
    } else {
        proceed(app, conversation);
    }
}

/// `saveTextContext`.
pub fn save_text_context(app: &mut App, content: String, images: Vec<ImageAttachment>, files: Vec<FileAttachment>) {
    let Some(editor) = edits(app).editor.clone() else { return };
    let Some(conversation) = active(app) else { return };
    if app.context_mutation_is_blocked(&conversation) {
        return;
    }
    let contexts = app.timeline_contexts_of(&conversation);
    match editor {
        TimelineEditorState::Edit { item, .. } => {
            let Some(before) = contexts.iter().find(|context| context.id() == item.id()) else { return };
            let mut after = before.clone();
            match &mut after {
                ContextItem::Tool { .. } => return,
                ContextItem::Assistant { content: text_, interrupted, .. } => {
                    *text_ = content;
                    *interrupted = false;
                }
                ContextItem::Reasoning { content: text_, interrupted, .. } => {
                    *text_ = Some(content);
                    *interrupted = false;
                }
                ContextItem::User { content: text_, images: held_images, files: held_files, .. } => {
                    *text_ = content;
                    *held_images = images;
                    *held_files = files;
                }
                ContextItem::System { content: text_, .. } => *text_ = content,
            }
            app.commit_timeline_edit(&conversation, TimelinePatch { replaced: vec![(before.clone(), after)], ..TimelinePatch::default() }, t("编辑上下文", "Edit the context"), None);
        }
        TimelineEditorState::Insert { kind, index, .. } => {
            let (id, created_at) = (create_id("ctx"), now_iso());
            let item = match kind {
                // Manually inserted reasoning is explicitly plaintext because the user supplied all of its text.
                InsertKind::Reasoning => ContextItem::Reasoning { id, content: Some(content), form: Some(mewrk_client::model::ReasoningForm::Plaintext), round: None, model_turn_id: None, interrupted: false, duration_ms: None, tokens: None, replay: None, created_at },
                // A placed message carries what was pasted into it; every other kind has nowhere to put an image.
                InsertKind::User => ContextItem::User { id, content, images, files, created_at },
                InsertKind::Assistant => ContextItem::Assistant { id, content, round: None, model_turn_id: None, interrupted: false, sources: Vec::new(), created_at },
                _ => ContextItem::System { id, content, local_only: false, hook_execution: None, tools_added: Vec::new(), native_compaction: None, created_at },
            };
            app.commit_timeline_edit(&conversation, insertion_patch(&contexts, item, index as isize), t("插入上下文", "Insert a context"), None);
        }
    }
    edits(app).editor = None;
    repaint();
}

#[derive(serde::Deserialize)]
struct AttestedTool {
    input: JsonObject,
    result: ToolResult,
    attestation: String,
}

/// `saveToolContextEdit`: writes hand-written arguments and result onto a card, recorded or brand new. Nothing is executed, so the
/// card would not be provable; the host issues its attestation over what was typed and returns the exact bytes to store.
pub fn save_tool_context_edit(app: &mut App, input: JsonObject, output: String, images: Vec<ImageAttachment>, done: ToolDone) {
    let Some(editor) = edits(app).editor.clone() else { return done(app, Err(t("没有活动对话", "No active conversation").to_owned())) };
    let Some(conversation) = active(app) else { return done(app, Err(t("没有活动对话", "No active conversation").to_owned())) };
    if app.context_mutation_is_blocked(&conversation) {
        return done(app, Err(t("模型回合进行中，无法修改上下文", "Context cannot be changed while a model turn is in progress").to_owned()));
    }
    enum Target {
        Insert { tool_name: String, index: usize, context_id: String },
        Edit { tool_name: String, context_id: String },
    }
    let target = match &editor {
        TimelineEditorState::Insert { kind: InsertKind::Tool, tool_name: Some(tool_name), index } => Target::Insert { tool_name: tool_name.clone(), index: *index, context_id: create_id("ctx") },
        TimelineEditorState::Insert { .. } => return done(app, Err(t("没有选择工具", "No tool was chosen").to_owned())),
        TimelineEditorState::Edit { item: ContextItem::Tool { id, tool_name, .. }, .. } => Target::Edit { tool_name: tool_name.clone(), context_id: id.clone() },
        TimelineEditorState::Edit { .. } => return done(app, Err(t("没有活动对话", "No active conversation").to_owned())),
    };
    let done = UiBound::new(done);
    conversation_for_timeline_tool(app, move |app, home| {
        let (_, target_conversation) = match home {
            Ok(home) => home,
            Err(reason) => return done.into_inner()(app, Err(reason)),
        };
        match target {
            Target::Insert { tool_name, index, context_id } => {
                let request = json!({ "conversationId": target_conversation, "contextId": context_id, "toolName": tool_name, "input": input, "output": output });
                app.invoke::<AttestedTool>("attest_inserted_tool_context", json!({ "request": request }), move |app, attested| {
                    let attested = match attested {
                        Ok(attested) => attested,
                        Err(reason) => return done.into_inner()(app, Err(reason)),
                    };
                    let placed = ContextItem::Tool { id: context_id, tool_name, round: None, model_turn_id: None, provider_call_id: None, requested_input: None, input: attested.input, result: attested.result, subagent: None, notice: None, attestation: attested.attestation, created_at: now_iso() };
                    let current = app.timeline_contexts_of(&target_conversation);
                    app.commit_timeline_edit(&target_conversation, insertion_patch(&current, placed, index as isize), t("插入工具调用", "Insert a tool call"), None);
                    edits(app).editor = None;
                    repaint();
                    done.into_inner()(app, Ok(()));
                });
            }
            Target::Edit { tool_name, context_id } => {
                let request = json!({ "conversationId": target_conversation, "contextId": context_id, "toolName": tool_name, "input": input, "output": output, "images": images });
                app.invoke::<AttestedTool>("attest_edited_tool_context", json!({ "request": request }), move |app, attested| {
                    let attested = match attested {
                        Ok(attested) => attested,
                        Err(reason) => return done.into_inner()(app, Err(reason)),
                    };
                    let current = app.timeline_contexts_of(&target_conversation);
                    if let Some(before) = current.iter().find(|context| context.id() == context_id) {
                        let mut after = before.clone();
                        if let ContextItem::Tool { requested_input, input, result, attestation, .. } = &mut after {
                            *requested_input = None;
                            *input = attested.input;
                            *result = attested.result;
                            *attestation = attested.attestation;
                            app.commit_timeline_edit(&target_conversation, TimelinePatch { replaced: vec![(before.clone(), after)], ..TimelinePatch::default() }, t("编辑工具调用", "Edit the tool call"), None);
                        }
                    }
                    edits(app).editor = None;
                    repaint();
                    done.into_inner()(app, Ok(()));
                });
            }
        }
    });
}

/// `saveQuestionContext`.
pub fn save_question_context(app: &mut App, input: JsonObject, answer_content: Option<String>, done: ToolDone) {
    let active_id = active(app);
    let Some((conversation, editor)) = edits(app).question_editor.clone().filter(|(conversation, _)| Some(conversation) == active_id.as_ref()) else {
        return done(app, Err(t("没有活动提问", "No active question").to_owned()));
    };
    if app.context_mutation_is_blocked(&conversation) {
        return done(app, Err(t("模型回合进行中，无法修改上下文", "Context cannot be changed while a model turn is in progress").to_owned()));
    }
    let ContextItem::Tool { input: original_input, .. } = &editor.item else { return done(app, Err(t("没有活动提问", "No active question").to_owned())) };
    let original = questions_from_input(original_input);
    let next = questions_from_input(&input);
    if !is_claude_question_input(&input) || next.len() != original.len() {
        return done(app, Err(t("提问数量必须保持不变，且每一项都必须符合提问格式", "The number of questions must stay unchanged and every item must remain valid").to_owned()));
    }
    if editor.answer.is_some() {
        let answers = answers_from_formatted_content(answer_content.as_deref().unwrap_or(""), &next);
        if !answers.is_some_and(|answers| answers.len() == next.len() && answers.iter().all(|answer| !answer.trim().is_empty())) {
            return done(app, Err(t("每一个问题都必须保留非空回答", "Every question must keep a non-empty answer").to_owned()));
        }
    }
    let contexts = app.timeline_contexts_of(&conversation);
    let question = contexts.iter().find(|context| context.id() == editor.item.id());
    let answer = editor.answer.as_ref().and_then(|answer| contexts.iter().find(|context| context.id() == answer.id()));
    if question.is_none() || (editor.answer.is_some() && answer.is_none()) {
        return done(app, Err(t("提问消息已不存在", "The question message no longer exists").to_owned()));
    }
    let mut replaced = Vec::new();
    if let Some(before) = question {
        let mut after = before.clone();
        if let ContextItem::Tool { requested_input, input: held, .. } = &mut after {
            *requested_input = None;
            *held = input;
            replaced.push((before.clone(), after));
        }
    }
    if let Some(before) = answer {
        let mut after = before.clone();
        if let (ContextItem::User { content, .. }, Some(text_)) = (&mut after, answer_content) {
            *content = text_;
        }
        replaced.push((before.clone(), after));
    }
    app.commit_timeline_edit(&conversation, TimelinePatch { replaced, ..TimelinePatch::default() }, t("编辑提问", "Edit the questions"), None);
    edits(app).question_editor = None;
    repaint();
    done(app, Ok(()));
}

/// `saveToolContext`: runs the call and writes its arguments and result together.
pub fn save_tool_context(app: &mut App, tool_name: String, input: JsonObject, tools: &[ToolDescriptor], enabled: &[String], done: ToolDone) {
    let Some(editor) = edits(app).editor.clone() else { return done(app, Err(t("没有活动对话", "No active conversation").to_owned())) };
    let Some(conversation) = active(app) else { return done(app, Err(t("没有活动对话", "No active conversation").to_owned())) };
    if app.context_mutation_is_blocked(&conversation) {
        return done(app, Err(t("模型回合进行中，无法修改上下文", "Context cannot be changed while a model turn is in progress").to_owned()));
    }
    if !tools.iter().any(|tool| tool.name == tool_name) || !enabled.contains(&tool_name) {
        return done(app, Err(tf("当前工作区模式不可使用工具 {tool}", "Tool {tool} is unavailable in the current workspace mode", &[("tool", &tool_name)])));
    }
    if let TimelineEditorState::Edit { item: ContextItem::Tool { id, tool_name: edited, .. }, .. } = &editor
        && edited == "ask_user"
    {
        if !is_claude_question_input(&input) {
            return done(app, Err(t("提问参数不符合 Claude Code AskUserQuestion 格式", "The questions do not match the Claude Code AskUserQuestion format").to_owned()));
        }
        let contexts = app.timeline_contexts_of(&conversation);
        if let Some(before) = contexts.iter().find(|context| context.id() == id) {
            let mut after = before.clone();
            if let ContextItem::Tool { requested_input, input: held, .. } = &mut after {
                *requested_input = None;
                *held = input;
                app.commit_timeline_edit(&conversation, TimelinePatch { replaced: vec![(before.clone(), after)], ..TimelinePatch::default() }, t("编辑提问", "Edit the questions"), None);
            }
        }
        edits(app).editor = None;
        repaint();
        return done(app, Ok(()));
    }
    let done = UiBound::new(done);
    conversation_for_timeline_tool(app, move |app, home| {
        let (workspace_id, target) = match home {
            Ok(home) => home,
            Err(reason) => return done.into_inner()(app, Err(reason)),
        };
        let workspace_path = app.doc.workspaces.iter().find(|workspace| workspace.id == workspace_id).map_or_else(String::new, |workspace| workspace.path.clone());
        let request = ToolExecutionRequest { conversation_id: target.clone(), workspace_path, tool_name: tool_name.clone(), input: input.clone() };
        // Rust classifies every operation from the persisted security policy. Calls that need consent come back with a prompt to draw.
        let held_target = target.clone();
        app.run_manual_tool(&target, request, move |app, execution| {
            let execution = match execution {
                Ok(execution) => execution,
                Err(reason) => return done.into_inner()(app, Err(reason)),
            };
            let current = app.timeline_contexts_of(&held_target);
            match &editor {
                TimelineEditorState::Edit { item, .. } => {
                    if let Some(before) = current.iter().find(|context| context.id() == item.id()) {
                        let mut after = before.clone();
                        if let ContextItem::Tool { requested_input, input: held, result, .. } = &mut after {
                            *requested_input = None;
                            *held = input;
                            *result = execution;
                            app.commit_timeline_edit(&held_target, TimelinePatch { replaced: vec![(before.clone(), after)], ..TimelinePatch::default() }, t("重新运行工具调用", "Run the tool call again"), None);
                        }
                    }
                }
                TimelineEditorState::Insert { index, .. } => {
                    let item = ContextItem::Tool { id: create_id("ctx"), tool_name, round: None, model_turn_id: None, provider_call_id: None, requested_input: None, input, result: execution, subagent: None, notice: None, attestation: String::new(), created_at: now_iso() };
                    app.commit_timeline_edit(&held_target, insertion_patch(&current, item, *index as isize), t("运行工具调用", "Run a tool call"), None);
                }
            }
            edits(app).editor = None;
            repaint();
            done.into_inner()(app, Ok(()));
        });
    });
}

/// `messageAttachmentAdder`: the attach handler for a user message edited in place. The same gates the composer applies, because this
/// is the same kind of message arriving through a different box. Numbering happens here, against the live transcript, so an attached
/// image is citable the moment the card is saved.
pub fn message_attachment_adder(image_input: bool, conversation_id: String) -> AddAttachments {
    message_attachment_adder_against(
        image_input,
        Rc::new(move |app| {
            let contexts = app.timeline_contexts_of(&conversation_id);
            let queued = app.conversation(&conversation_id).map(|conversation| conversation.queued_messages.clone()).unwrap_or_default();
            reserve_queued_message_ids(image_short_ids_in_use(contexts.iter()), queued.iter())
        }),
    )
}

/// The same with `taken` naming the image numbers already spoken for, for a surface whose messages are not a conversation's: a
/// template's own body is the whole of what its numbers answer to.
pub fn message_attachment_adder_against(image_input: bool, taken: Rc<dyn Fn(&mut App) -> HashSet<u32>>) -> AddAttachments {
    Rc::new(move |app, paths: Vec<PathBuf>, existing: Vec<ImageAttachment>, done| {
        let mut taken = taken(app);
        taken.extend(existing.iter().filter_map(|image| image.short_id));
        let room = MessageAttachmentRoom::new(&existing, &[]);
        let done = UiBound::new(done);
        app.spawn(move |task| {
            let mut rejected = Vec::new();
            let mut files = Vec::new();
            for path in paths {
                let name = path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
                match std::fs::read(&path) {
                    Ok(bytes) => files.push(ComposerFile { media_type: String::new(), name, bytes }),
                    Err(_) => rejected.push(AttachmentRejection { name: Some(name), reason: AttachmentRejectionReason::Failed }),
                }
            }
            let mut added = AddedAttachments { images: Vec::new(), files: Vec::new(), rejected };
            for intake in sort_attachments(files, image_input, room) {
                match intake {
                    Intake::Rejected(rejection) => added.rejected.push(rejection),
                    Intake::Image(file) => {
                        let uploaded: Result<ImageAttachment, String> = task.invoke("image_attachment_upload", json!({ "name": file.name, "data": bytes_to_base64(&file.bytes) }));
                        match uploaded {
                            Ok(mut image) => {
                                let short_id = next_image_short_id(&taken);
                                taken.insert(short_id);
                                image.short_id = Some(short_id);
                                added.images.push(image);
                            }
                            Err(_) => added.rejected.push(AttachmentRejection { name: Some(file.name), reason: AttachmentRejectionReason::ImageRejected }),
                        }
                    }
                    Intake::Document { file, kind } => match read_document(file, kind) {
                        Err(rejection) => added.rejected.push(rejection),
                        Ok(document) => {
                            let uploaded: Result<FileAttachment, String> = task.invoke("file_attachment_upload", json!({ "name": document.name, "data": bytes_to_base64(&document.bytes), "format": document.format, "text": document.extracted.as_ref().map(|extracted| extracted.text.clone()), "pages": document.extracted.as_ref().map(|extracted| extracted.pages) }));
                            match uploaded {
                                Ok(file) => added.files.push(file),
                                Err(_) => added.rejected.push(AttachmentRejection { name: Some(document.name), reason: AttachmentRejectionReason::Failed }),
                            }
                        }
                    },
                }
            }
            task.post(move |app| done.into_inner()(app, added));
        });
    })
}

/// `openRewindDialog`: nothing is touched until the user picks what to put back. The host puts the files back from the timeline it has
/// saved, and conversation commands are debounced, so what is saved is flushed first; the window is modal, so what is saved when it
/// opens holds for the preview and for the rewind that follows.
pub fn open_rewind_dialog(app: &mut App, item: &ContextItem, disabled_reason: Option<&str>) {
    let Some(conversation) = active(app) else { return };
    if is_draft_conversation_id(&conversation) || !matches!(item, ContextItem::User { .. }) || disabled_reason.is_some() {
        return;
    }
    let context_id = item.id().to_owned();
    app.flush_latest_document(false, move |app, _| {
        edits(app).rewind = Some(RewindDialogState { conversation_id: conversation.clone(), context_id: context_id.clone(), preview: RewindPreview::Loading, busy: false, action_error: None });
        repaint();
        let request = json!({ "conversationId": conversation, "contextId": context_id, "dryRun": true });
        app.invoke::<RewindFilesResponse>("rewind_files", json!({ "request": request }), move |app, response| {
            if let Some(state) = edits(app).rewind.as_mut().filter(|state| state.context_id == context_id) {
                state.preview = match response {
                    Ok(response) => RewindPreview::Ready(response.files),
                    Err(message) => RewindPreview::Error(message),
                };
            }
            repaint();
        });
    });
}

/// `rewindToBeforeMessage`: the rest of a rewind, once the window has put the files back. The message and all that followed leave the
/// timeline and the message goes back to the composer: an ordinary timeline edit, so the undo keys take it back, the files not included.
fn rewind_to_before_message(app: &mut App, conversation_id: &str, context_id: &str, scope: RewindScope, restored: Option<RewindFilesResponse>) {
    const MS: u64 = mewrk_client::runs::timeline_edits::TIMELINE_NOTICE_MS;
    let files_result = restored.as_ref().map_or_else(String::new, |restored| rewind_result_summary(&summarize_rewind_files(&restored.files)));
    if scope == RewindScope::Files {
        app.show_timeline_notice(Some(conversation_id), tf("已回退文件：{result}", "Files rewound: {result}", &[("result", &files_result)]), None, MS);
        return;
    }
    let refuse = |app: &mut App, reason: String| {
        let message = if scope == RewindScope::Both { tf("文件已回退（{result}），但对话没有回退：{reason}", "Files rewound ({result}), but the conversation was not: {reason}", &[("result", &files_result), ("reason", &reason)]) } else { tf("对话没有回退：{reason}", "The conversation was not rewound: {reason}", &[("reason", &reason)]) };
        app.show_timeline_notice(Some(conversation_id), message, None, MS);
    };
    let range = app.conversation(conversation_id).and_then(|conversation| rewind_range(conversation, context_id));
    let message = app.conversation(conversation_id).and_then(|conversation| conversation.contexts.iter().find(|context| context.id() == context_id)).cloned();
    let (Some(range), Some(message)) = (range, message.filter(|message| matches!(message, ContextItem::User { .. }))) else {
        return refuse(app, t("这条消息已不在时间线上", "That message is no longer on the timeline").to_owned());
    };
    if range.anchored {
        return refuse(app, t("有分支从这些消息开始", "A branch starts at one of these messages").to_owned());
    }
    if app.context_mutation_is_blocked(conversation_id) {
        return refuse(app, t("模型回合进行中", "A model turn is in progress").to_owned());
    }
    close_timeline_editors(app);
    let contexts = app.timeline_contexts_of(conversation_id);
    let removed = placed_contexts(&contexts, &range.ids.iter().cloned().collect());
    let committed = app.commit_timeline_edit(conversation_id, TimelinePatch { removed, ..TimelinePatch::default() }, t("回退到这条消息之前", "Rewind to before this message"), None);
    if !committed {
        return refuse(app, t("无法修改时间线", "The timeline could not be changed").to_owned());
    }
    app.put_message_in_composer(conversation_id, &message);
    let notice = if scope == RewindScope::Both { tf("已回退对话和文件：{result}", "Conversation and files rewound: {result}", &[("result", &files_result)]) } else { t("已回退到这条消息之前", "Rewound to before this message").to_owned() };
    let action = if scope == RewindScope::Both { t("撤回对话", "to undo the conversation") } else { t("撤回", "to undo") };
    app.show_timeline_notice(Some(conversation_id), notice, Some((timeline_history_shortcut(TimelineHistoryAction::Undo, IS_APPLE_PLATFORM), action.to_owned())), MS);
}

fn run_rewind(app: &mut App, scope: RewindScope) {
    let Some(state) = edits(app).rewind.as_mut() else { return };
    state.busy = true;
    state.action_error = None;
    let (conversation, context) = (state.conversation_id.clone(), state.context_id.clone());
    repaint();
    let settle = move |app: &mut App, restored: Option<RewindFilesResponse>| {
        rewind_to_before_message(app, &conversation, &context, scope, restored);
        edits(app).rewind = None;
        repaint();
    };
    if scope == RewindScope::Conversation {
        return settle(app, None);
    }
    let (conversation, context) = {
        let state = edits(app).rewind.as_ref().expect("still up");
        (state.conversation_id.clone(), state.context_id.clone())
    };
    let request = json!({ "conversationId": conversation, "contextId": context, "dryRun": false });
    app.invoke::<RewindFilesResponse>("rewind_files", json!({ "request": request }), move |app, restored| match restored {
        Ok(restored) => settle(app, Some(restored)),
        // Nothing of the conversation has moved: the files are the first half, and a failure stops here.
        Err(message) => {
            if let Some(state) = edits(app).rewind.as_mut() {
                state.busy = false;
                state.action_error = Some(if message.is_empty() { t("文件回退失败", "The files could not be rewound").to_owned() } else { message });
            }
            repaint();
        }
    });
}

/// The delete confirmation and the rewind window. A child of the window's root, wherever the timeline is.
pub fn timeline_dialogs(app: &mut App) -> Option<E> {
    let t_ = theme();
    if let Some(confirmation) = edits(app).confirmation.clone() {
        let cancel: Handler<App> = Rc::new(|app| {
            edits(app).confirmation = None;
            repaint();
        });
        let backdrop = cancel.clone();
        let confirm = confirmation.on_confirm.clone();
        let accept = move |app: &mut App| {
            edits(app).confirmation = None;
            confirm(app);
            repaint();
        };
        let body = col()
            .p(20.0)
            .gap(14.0)
            .child(text(confirmation.question.clone()).ty(BODY_SMALL).color(t_.text_primary))
            .child(
                row()
                    .gap(6.0)
                    .justify_end()
                    .child(button(t("取消", "Cancel")).key("timeline-confirm-cancel").on_click(move |app| cancel(app)))
                    .child(if confirmation.destructive { danger_button(confirmation.confirm_label.clone()) } else { accent_button(confirmation.confirm_label.clone()) }.key("timeline-confirm-accept").on_click(accept)),
            );
        return Some(dialog(380.0, body, Some(backdrop)).aria_label(confirmation.question.clone()));
    }
    let state = edits(app).rewind.clone()?;
    // Why nothing may be rewound right now; the host would refuse the dry run as it refuses the real one.
    let blocked_reason = app.context_mutation_is_blocked(&state.conversation_id).then(|| t("模型回合进行中，完成或停止后才能回退", "Wait for the model turn to finish or stop before rewinding").to_owned());
    let range = app.conversation(&state.conversation_id).and_then(|conversation| rewind_range(conversation, &state.context_id));
    let conversation_blocked_reason = range.filter(|range| range.anchored).map(|_| t("有分支从这条消息或其之后的消息开始，不能回退对话；仍可以只恢复文件。", "A branch starts at this message or one after it, so the conversation cannot be rewound. The files can still be restored.").to_owned());
    Some(rewind_dialog(RewindDialogProps {
        state,
        blocked_reason,
        conversation_blocked_reason,
        on_run: Rc::new(run_rewind),
        on_close: Rc::new(|app| {
            edits(app).rewind = None;
            repaint();
        }),
    }))
}

/// Everything `App.tsx` handed the view for the active conversation. The shell adds the slots (`composer`, `empty_state`,
/// `before_timeline`, `timeline_placeholder`), `agents`, the disabled reasons it alone can compute (Git, deletion) and what opens panes.
pub fn timeline_props(app: &mut App, conversation_id: &str, tools: Rc<Vec<ToolDescriptor>>, enabled_tools: Rc<Vec<String>>) -> ConversationViewProps {
    let state = edits(app);
    if !state.hooked {
        state.hooked = true;
        app.doc_hooks().editors_closed.push(Rc::new(close_timeline_editors));
    }
    let state = edits(app);
    let editor = state.editor.clone();
    let question_editor = state.question_editor.as_ref().filter(|(conversation, _)| conversation == conversation_id).map(|(_, editor)| editor.clone());
    let paste_count = state.clipboard.as_ref().map_or(0, |clipboard| clipboard.contexts.len());
    let draft = is_draft_conversation_id(conversation_id);
    let mutation_blocked = app.context_mutation_is_blocked(conversation_id);
    let rewind_disabled_reason = mutation_blocked.then(|| t("模型回合进行中，完成或停止后才能回退", "Wait for the model turn to finish or stop before rewinding").to_owned());
    let branch_switch_disabled_reason = mutation_blocked.then(|| t("模型回合进行中，完成后才能切换对话分支", "Wait for the model turn to finish before switching conversation branches.").to_owned());
    let branch_navigations = Rc::new(app.conversation(conversation_id).map(context_branch_navigations).unwrap_or_default());
    let image_input = {
        let (_, model) = mewrk_client::doc::model_choice(&app.doc);
        model.is_some_and(supports_vision)
    };
    let error = app.model_run_error(conversation_id);
    let retryable_turn_request_id = error.as_ref().filter(|error| error.retryable).and_then(|error| error.request_id.clone());
    let (copy_tools, run_tools, run_enabled) = (tools.clone(), tools.clone(), enabled_tools.clone());
    let dismissed = conversation_id.to_owned();
    let (undo_id, redo_id) = (conversation_id.to_owned(), conversation_id.to_owned());
    let rewind_reason = rewind_disabled_reason.clone();
    ConversationViewProps {
        conversation_id: Some(conversation_id.to_owned()),
        timeline_mutation_locked: mutation_blocked,
        tools,
        enabled_tools,
        editor,
        question_editor,
        paste_count,
        branch_navigations,
        rewind_disabled_reason,
        branch_switch_disabled_reason,
        retryable_turn_request_id,
        attachment_image_input: Some(image_input),
        on_edit: Some(Rc::new(handle_context_edit)),
        on_delete: Some(Rc::new(|app, item| delete_context(app, item, false))),
        on_edit_question: Some(Rc::new(handle_question_edit)),
        on_delete_question: Some(Rc::new(delete_question_context)),
        on_cancel_edit: Some(Rc::new(close_timeline_editors)),
        on_save_text: Some(Rc::new(save_text_context)),
        on_add_attachments: Some(message_attachment_adder(image_input, conversation_id.to_owned())),
        on_save_tool: Some(Rc::new(move |app, tool_name, input, done| save_tool_context(app, tool_name, input, &run_tools, &run_enabled, done))),
        on_save_tool_edit: Some(Rc::new(save_tool_context_edit)),
        on_save_question: Some(Rc::new(save_question_context)),
        on_branch_from: Some(Rc::new(|app, item| app.branch_from_user_context(item))),
        on_rewind_to: if draft { None } else { Some(Rc::new(move |app, item| open_rewind_dialog(app, item, rewind_reason.as_deref()))) },
        on_fork_at: if draft { None } else { Some(Rc::new(|app, index| app.fork_conversation_at(index))) },
        on_select_branch: Some(Rc::new(|app, fork, branch| app.select_conversation_branch(fork, branch))),
        on_insert: Some(Rc::new(handle_context_insert)),
        on_delete_contexts: Some(Rc::new(|app, ids| delete_timeline_contexts(app, ids, false))),
        on_move_contexts: Some(Rc::new(move_timeline_contexts)),
        on_copy_contexts: Some(Rc::new(move |app, ids| copy_timeline_contexts(app, ids, &copy_tools))),
        on_paste_contexts: Some(Rc::new(paste_timeline_contexts)),
        on_undo: Some(Rc::new(move |app| app.step_timeline_history(&undo_id, TimelineHistoryAction::Undo))),
        on_redo: Some(Rc::new(move |app| app.step_timeline_history(&redo_id, TimelineHistoryAction::Redo))),
        on_retry_turn_error: Some(Rc::new(|app| app.retry_model_run())),
        on_dismiss_turn_error: Some(Rc::new(move |app| app.dismiss_model_run_error(&dismissed))),
        ..ConversationViewProps::default()
    }
}

/// `.timeline-notice`: what an edit on the timeline just did, and the keys that take it back. It floats over the composer's top edge,
/// so the shell places it there: a child of the composer's wrapper.
pub fn timeline_notice_view(app: &mut App, conversation_id: &str) -> Option<E> {
    let t_ = theme();
    let notice = app.timeline_notice()?;
    if notice.conversation_id.as_deref().is_some_and(|id| id != conversation_id) {
        return None;
    }
    let ty_ = TypeStyle { size: 12.0, line: 16.0, weight: Weight::Regular, mono: false, italic: false, serif: false };
    let mut view = row().key(("timeline-notice", notice.id.as_str())).h(26.0).px(10.0).items_center().gap(8.0).radius(13.0).bg(t_.flyout).border(t_.flyout_stroke).shadow(8.0, 2.0, t_.shadow).child(text(notice.message.clone()).ty(ty_).color(t_.text_primary).ellipsis());
    if let Some((keys, action)) = notice.hint {
        view = view
            .child(row().px(5.0).py(2.0).radius(4.0).border(t_.card_stroke).bg(t_.subtle_hover).child(text(keys).ty(TypeStyle { size: 11.0, line: 11.0, weight: Weight::Semibold, mono: false, italic: false, serif: false }).color(t_.text_primary).nowrap()))
            .child(text(action).ty(ty_).color(t_.text_secondary).nowrap());
    }
    Some(view)
}
