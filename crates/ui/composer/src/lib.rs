//! The composer and the cards docked on it: the `composer-wrap` region of upstream's `App.tsx`, `ComposerAddMenu`, `ComposerCat`,
//! `ContextUsageMeter`, `ImageStrip`, `PastedTextTags`, `lib/pastedText.ts`, `SelectedElementChips`, the composer half of
//! `lib/selectedElement.ts`, `QueuedMessageList`, `ToolApprovalDock`, `QuestionDock`, `ProjectChips`, `AttachmentFeedback`,
//! `DraftLanding` and `LockTone`'s `ModelCacheMark`.
//!
//! Public surface, what `App.tsx` passes as `composer=` and `emptyState=`:
//!
//! ```text
//! pub fn composer(app: &mut App, props: ComposerProps) -> E
//!     Call it every frame the conversation is on screen. It reads the active conversation (draft or persisted) itself.
//! pub fn draft_landing(app: &mut App) -> E
//!     The timeline's empty state on a task nothing has been sent in. Empty when the composer is not an unsent task's.
//!
//! pub fn focus_composer()                                                 `composerTextareaRef.current.focus()`
//! pub fn put_mention_in_composer(app, conversation_id, mention)           `mentionFileInComposer`
//! pub fn stage_element_pick(app, conversation_id, block, crop)            `onElementPicked`
//! pub fn attach_workspace(app, machine, path)                             what the remote directory browser's pick does
//! pub fn image_strip(app, props) -> Option<E>                             the strip, for the timeline's user messages too
//! pub fn composer_add_files(key, disabled, image_input, reason, on_choose) -> E   the add menu, for an inline message editor too
//! ```
//!
//! `ComposerProps` carries what only the shell knows; see its fields. `attach::attach_ports` replaces the file dialog and the
//! system clipboard for tests and tools.

pub mod approval_dock;
pub mod art;
pub mod attach;
pub mod cat;
pub mod chips;
pub mod editor;
pub mod element_chips;
pub mod feedback;
pub mod footer;
pub mod image_strip;
pub mod landing;
pub mod meter;
pub mod pasted_text;
pub mod question_dock;
pub mod queued;
mod rolling;
#[cfg(test)]
mod tests;

pub use chips::attach_workspace;
pub use editor::{focus_composer, put_mention_in_composer};
pub use footer::composer_add_files;
pub use image_strip::{image_strip, ImageStripProps};

use approval_dock::{tool_approval_dock, DockStack};
use attach::{watch_drag, DropZoneState};
use element_chips::{images_without_element_crops, selected_element_chips};
use feedback::{attachment_drop_overlay, attachment_notice};
use mewrk_client::model::{ApiProvider, AttachedWorkspace, ContextItem, ConversationSettings, ConversationWorktree, ModelProfile, QueuedMessage, RunTarget, WorkspaceKind};
use mewrk_client::prelude::*;
use mewrk_client::runs::capabilities::contexts_contain_projected_images;
use mewrk_client::runs::{ComposerFile, ContextUsage, ElementPick};
use mewrk_client::workspaces::{project_workspaces, registered_project_workspaces, worktree_for};
use mewrk_ui_common::popover_menu::probe::say;
use question_dock::question_dock;
use queued::queued_message_list;

/// What only the shell knows. The default fits a composer alone on screen.
#[derive(Clone)]
pub struct ComposerProps {
    /// `approvalDockOwner === "composer"`. Exactly one surface owns the pending card; the composer takes it only when no open pane
    /// claims it: the plan page for a plan card, or the asking agent's tab while it is the one shown.
    pub owns_tool_prompt: bool,
    /// `gitReviewPanelOpen && activeReviewMember === activeWorkspaceMember`.
    pub git_review_open: bool,
    /// `openGitReview(member)`.
    pub on_open_git_review: Option<Rc<dyn Fn(&mut App, usize)>>,
    /// `setMachineSettings`: opens a machine's settings, `None` being this one.
    pub on_configure_machine: Option<Rc<dyn Fn(&mut App, Option<RunTarget>)>>,
    /// `setWorkspaceSettings`: opens a workspace's settings, by its machine and the directory it is registered at.
    pub on_configure_workspace: Option<Rc<dyn Fn(&mut App, Option<RunTarget>, String)>>,
    /// `setRemoteWorkspacePicker`: opens the remote directory browser for a machine, by its display name. Its pick goes to
    /// [`attach_workspace`].
    pub on_browse_machine: Option<Rc<dyn Fn(&mut App, RunTarget, String)>>,
    /// `hasNativeWorkspacePicker()`.
    pub has_native_workspace_picker: bool,
    /// Whether a terminal of this workspace is busy, which holds a Git write back. TODO(panes/terminal): the shell fills this from
    /// `terminalController` with `session.busy && ownerWorksIn(...)`.
    pub workspace_terminal_busy: Option<Rc<dyn Fn(&mut App, &str) -> bool>>,
}

impl Default for ComposerProps {
    fn default() -> Self {
        Self { owns_tool_prompt: true, git_review_open: false, on_open_git_review: None, on_configure_machine: None, on_configure_workspace: None, on_browse_machine: None, has_native_workspace_picker: true, workspace_terminal_busy: None }
    }
}

/// `@media (max-width: 720px)`: the composer's row and footer tighten up.
pub(crate) fn narrow_window() -> bool {
    window_size().0 <= 720.0
}

/// `.composer-wrap`'s side padding, which the floating context row keeps to as well.
pub(crate) fn wrap_padding() -> f32 {
    if narrow_window() { 10.0 } else { 24.0 }
}

/// The selected model and its provider: `activeModelChoice`.
pub struct ModelChoice {
    pub provider: ApiProvider,
    pub model: ModelProfile,
}

/// `activeModelChoice`: the document-wide selection, if it names an enabled model.
pub fn active_model_choice(app: &App) -> Option<ModelChoice> {
    let (provider, model) = mewrk_client::doc::model_choice(&app.doc);
    let (provider, model) = (provider?, model?);
    provider.enabled.then(|| ModelChoice { provider: provider.clone(), model: model.clone() })
}

/// What the composer reads about the conversation on screen. Fields, not the conversation: a long timeline is not to be copied every
/// frame.
pub(crate) struct Active {
    pub id: String,
    pub workspace_id: Option<String>,
    pub workspace_kind: Option<WorkspaceKind>,
    pub is_draft: bool,
    pub settings: ConversationSettings,
    pub attached_workspaces: Vec<AttachedWorkspace>,
    pub queued_messages: Vec<QueuedMessage>,
    pub project_workspaces: Vec<AttachedWorkspace>,
    pub registered_workspaces: Vec<AttachedWorkspace>,
    pub member: usize,
    pub worktree: Option<ConversationWorktree>,
    pub draft_worktree_members: Vec<u32>,
    /// A draft's hand-written timeline; a persisted conversation's is read where it lies.
    pub draft_contexts: Option<Vec<ContextItem>>,
    pub started: bool,
    pub model_running: bool,
    pub lifecycle_running: bool,
    pub image_input: bool,
}

/// `active_workspace_member` copies the whole conversation to answer, so its answer is kept until something it depends on changes.
/// TODO(mewrk-client): drop this when the selection module can answer without the copy.
#[derive(Default)]
struct MemberMemo {
    conversation: String,
    count: usize,
    member: usize,
    /// Asked again now and then all the same, for a selection some other surface moved.
    age: u32,
}

pub(crate) fn forget_member_memo(app: &mut App) {
    app.local::<MemberMemo>().conversation.clear();
}

fn workspace_member(app: &mut App, conversation_id: &str, count: usize) -> usize {
    let memo = app.local::<MemberMemo>();
    memo.age += 1;
    if memo.conversation == conversation_id && memo.count == count && memo.age < 120 {
        return memo.member;
    }
    let member = app.active_workspace_member();
    let memo = app.local::<MemberMemo>();
    memo.conversation = conversation_id.to_owned();
    memo.count = count;
    memo.member = member;
    memo.age = 0;
    member
}

fn read_active(app: &mut App, chosen: Option<&ModelChoice>) -> Option<Active> {
    let id = app.active_conversation_id()?;
    let workspace_id = app.active_workspace_id();
    let draft = app.draft_of(&id);
    let is_draft = draft.is_some();
    let (settings, attached_workspaces, queued_messages, worktrees) = match &draft {
        Some(draft) => (draft.settings.clone(), draft.attached_workspaces.clone(), Vec::new(), Vec::new()),
        None => {
            let conversation = workspace_id.as_deref().and_then(|workspace| app.find_conversation(workspace, &id))?;
            (conversation.settings.clone(), conversation.attached_workspaces.clone(), conversation.queued_messages.clone(), conversation.worktrees.clone())
        }
    };
    let workspace = workspace_id.as_deref().and_then(|workspace| app.workspace(workspace));
    let workspace_kind = workspace.map(|workspace| workspace.kind);
    let project = project_workspaces(workspace, &worktrees);
    let registered = registered_project_workspaces(workspace);
    let member = workspace_member(app, &id, project.len());
    // The worktree standing in for the workspace the chip has selected; a temporary project's conversation has its scratch directory
    // as workspace 1, and a draft none yet.
    let worktree = if is_draft { None } else { registered.get(member.saturating_sub(1)).and_then(|registered| worktree_for(&worktrees, member, registered)).cloned() };
    let summaries = app.model_run_summaries();
    let summary = summaries.iter().find(|(conversation, _)| *conversation == id).map(|(_, summary)| summary.clone());
    let model_running = summary.is_some();
    let queued_messages = match &summary {
        Some(summary) => queued_messages.into_iter().filter(|message| !summary.steered_message_ids.contains(&message.id)).collect(),
        None => queued_messages,
    };
    let started = app.active_conversation_started();
    let deleting = workspace_id.as_deref().is_some_and(|workspace| app.workspace_is_deleting(workspace));
    let mut peers: Vec<String> = workspace_id.as_deref().and_then(|workspace| app.workspace(workspace)).map(|workspace| workspace.conversations.iter().map(|conversation| conversation.id.clone()).collect()).unwrap_or_default();
    if let Some(workspace) = workspace_id.as_deref()
        && app.draft_of_workspace(workspace).is_some()
    {
        peers.push(mewrk_client::drafts::draft_conversation_id(workspace));
    }
    let git_running = {
        let controller = mewrk_pane_git::git_controller(app);
        peers.iter().any(|peer| controller.mutation_is_active(peer))
    };
    Some(Active {
        draft_worktree_members: draft.as_ref().map_or_else(Vec::new, |draft| draft.worktree_members.clone()),
        draft_contexts: draft.map(|draft| draft.contexts),
        id,
        workspace_id,
        workspace_kind,
        is_draft,
        settings,
        attached_workspaces,
        queued_messages,
        project_workspaces: project,
        registered_workspaces: registered,
        member,
        worktree,
        started,
        model_running,
        lifecycle_running: deleting || git_running,
        image_input: chosen.is_some_and(|choice| mewrk_client::model_capabilities::supports_vision(&choice.model)),
    })
}

/// `activeContextUsage`, kept until the timeline or the run changes: estimating walks the whole timeline.
#[derive(Default)]
struct UsageMemo {
    key: Option<(String, String, usize, u64, Option<String>, Option<String>)>,
    usage: ContextUsage,
}

pub(crate) fn context_usage_of(app: &mut App, active: &Active, chosen: Option<&ModelChoice>) -> ContextUsage {
    let target = chosen.map(|choice| (&choice.provider, &choice.model));
    if let Some(contexts) = &active.draft_contexts {
        let view = mewrk_client::runs::wire_view(contexts, target);
        return ContextUsage { tokens: mewrk_client::runs::estimate_wire_tokens(view), estimated: true, unprojectable: false, provider_id: chosen.map(|choice| choice.provider.id.clone()), family: chosen.map(|choice| choice.provider.family) };
    }
    let (updated_at, count) = app.conversation(&active.id).map_or((String::new(), 0), |conversation| (conversation.updated_at.clone(), conversation.contexts.len()));
    let revision = app.run_view(&active.id).map_or(0, |run| run.revision);
    let key = (active.id.clone(), updated_at, count, revision, chosen.map(|choice| choice.provider.id.clone()), chosen.map(|choice| choice.model.id.clone()));
    if app.local::<UsageMemo>().key.as_ref() == Some(&key) {
        return app.local::<UsageMemo>().usage.clone();
    }
    let usage = app.context_usage_view(&active.id);
    let memo = app.local::<UsageMemo>();
    memo.key = Some(key);
    memo.usage = usage.clone();
    usage
}

/// `selectedAgentRoleCount` of the document's catalog.
pub(crate) fn selected_role_count(app: &App, agent_ids: &[String], workspace_keys: &[String]) -> usize {
    let reachable = |location: Option<&str>| location.is_none_or(|key| key.is_empty() || workspace_keys.iter().any(|known| known == key));
    let known: std::collections::HashSet<&str> = app.doc.capabilities.agents.iter().filter(|agent| reachable(agent.descriptor.workspace_key.as_deref())).map(|agent| agent.descriptor.id.as_str()).collect();
    agent_ids.iter().map(String::as_str).filter(|id| known.contains(id)).collect::<std::collections::HashSet<_>>().len()
}

/// `activeCompactNowBlocked`: why "compact now" is out of reach for the conversation on screen: a run is going, or nothing has been
/// answered since it was last compacted, so the host would have nothing new to compact.
pub(crate) fn compact_now_blocked(app: &mut App, active: &Active, chosen: Option<&ModelChoice>) -> Option<String> {
    if active.is_draft {
        return Some(t("对话还没有可压缩的内容", "There is nothing in this conversation to compact yet").to_owned());
    }
    if app.model_run_is_active(&active.id) {
        return Some(t("对话正在运行，停下后才能压缩", "The conversation is running; stop it to compact").to_owned());
    }
    let target = chosen.map(|choice| (&choice.provider, &choice.model));
    let contexts = app.conversation(&active.id).map_or(&[][..], |conversation| conversation.contexts.as_slice());
    let answered = mewrk_client::runs::wire_view(contexts, target).contexts.iter().any(|context| matches!(context, ContextItem::Assistant { .. } | ContextItem::Reasoning { .. } | ContextItem::Tool { .. }));
    (!answered).then(|| t("上次压缩之后还没有新的内容可以压缩", "Nothing has been added since the last compaction").to_owned())
}

/// `.composer-run-error`.
fn run_error(key: &str, title: String, detail: String, actions: Vec<E>) -> E {
    let t_ = theme();
    say(&title);
    say(&detail);
    row().key(("composer-run-error", key)).role(Role::Alert).items_center().gap(7.0).mb(9.0).mt(-3.0).pl(9.0).pr(7.0).py(7.0).radius(8.0).bg(t_.critical_bg).border(t_.critical.fade(0.3)).child(icon("CircleAlert", 15.0).color(t_.critical).shrink0()).child(col().flex1().min_w(0.0).child(text(title).ty(FINE_STRONG).color(t_.critical).ellipsis()).child(text(detail).ty(FINE).color(t_.text_secondary).mt(1.0))).children(actions)
}

pub fn send_button_key() -> u64 {
    hash_key("composer-send")
}

pub fn stop_button_key() -> u64 {
    hash_key("composer-stop")
}

pub fn run_retry_key() -> u64 {
    hash_key("composer-run-retry")
}

pub fn run_dismiss_key() -> u64 {
    hash_key("composer-run-dismiss")
}

pub fn fork_retry_key() -> u64 {
    hash_key("composer-fork-retry")
}

pub fn composer_box_key() -> u64 {
    hash_key("composer-box")
}

/// What the primary button is doing, and the standing stop control beside it when queuing has repurposed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendButtonState {
    /// A running conversation must retain a visible stop control while the primary button queues.
    pub separate_stop: bool,
    /// The primary button stops instead of sending.
    pub primary_stops: bool,
    pub primary_disabled: bool,
}

/// The inputs of the send button's logic, by upstream's names.
#[derive(Clone, Copy, Debug, Default)]
pub struct SendInputs {
    pub model_running: bool,
    pub queues_message: bool,
    pub stopping: bool,
    pub image_loading: bool,
    pub lifecycle_running: bool,
    pub has_model: bool,
    pub images_unsupported: bool,
}

pub fn send_button_state(inputs: SendInputs) -> SendButtonState {
    let stops_run = !inputs.queues_message && inputs.model_running;
    SendButtonState {
        separate_stop: inputs.model_running && inputs.queues_message,
        primary_stops: stops_run,
        primary_disabled: inputs.stopping || (inputs.image_loading && !stops_run) || (!inputs.model_running && (inputs.lifecycle_running || !inputs.has_model || inputs.images_unsupported)),
    }
}

fn send_button_label(inputs: SendInputs, question_pending: bool, workspace_deleting: bool) -> &'static str {
    if inputs.queues_message {
        if question_pending { t("发送", "Send") } else { t("加入排队消息", "Queue message") }
    } else if inputs.model_running {
        if inputs.stopping { t("正在停止生成", "Stopping generation") } else { t("停止生成", "Stop generating") }
    } else if workspace_deleting {
        t("项目删除中", "Project deletion in progress")
    } else {
        t("发送", "Send")
    }
}

fn stop_mark(color: Color) -> E {
    div().size(10.0, 10.0).radius(3.0).bg(color).pass_through()
}

/// `onElementPicked`: the crop rides the ordinary attachment path, so it passes the same vision, size and budget gates as any
/// dragged-in image. It just never shows up as one: the chip is what the user made, so the chip is the only thing the composer draws,
/// and recording the attachment on the pick is what lets the chip take it away. `crop` is the picture's file name and bytes.
pub fn stage_element_pick(app: &mut App, conversation_id: &str, block: String, crop: Option<(String, Vec<u8>)>) {
    let mut picks = app.composer().element_picks.get(conversation_id).cloned().unwrap_or_default();
    picks.push(ElementPick { block, screenshot_image_id: None });
    let index = picks.len() - 1;
    app.set_composer_element_picks(conversation_id, picks);
    let Some((name, bytes)) = crop else { return };
    let conversation = conversation_id.to_owned();
    app.add_composer_images(conversation_id, vec![ComposerFile { name, media_type: "image/png".to_owned(), bytes }], move |app, added| {
        let Some(image) = added.first() else { return };
        let mut picks = app.composer().element_picks.get(&conversation).cloned().unwrap_or_default();
        if let Some(pick) = picks.get_mut(index) {
            pick.screenshot_image_id = Some(image.id.clone());
        }
        app.set_composer_element_picks(&conversation, picks);
    });
}

/// Why the last send could not make a draft's worktree, for the conversation it was for.
#[derive(Default)]
pub(crate) struct SendWorktreeError(pub Option<(String, String)>);

/// The zone's state, as the drag leaf left it for this frame.
#[derive(Default)]
struct ComposerDrop(DropZoneState);

/// The composer of the conversation on screen.
pub fn composer(app: &mut App, props: ComposerProps) -> E {
    if std::mem::take(&mut app.composer().focus_pending) {
        focus_composer();
    }
    let chosen = active_model_choice(app);
    let Some(active) = read_active(app, chosen.as_ref()) else { return div() };
    let t_ = theme();
    let id = active.id.clone();

    // ---- docks
    let prompt = app.active_tool_prompt(&id);
    let stack = prompt.as_ref().map(|(_, index, total)| DockStack { index: *index, total: *total });
    let owned = prompt.as_ref().filter(|_| props.owns_tool_prompt).map(|(prompt, _, _)| prompt.clone());
    let question_pending = app.question_is_pending(&id);
    let approval = tool_approval_dock(app, &id, owned.as_ref(), stack);
    let question = question_dock(app, &id, owned.as_ref(), stack, active.lifecycle_running);
    let queued = queued_message_list(app, active.workspace_id.as_deref().unwrap_or(""), &id, &active.queued_messages);
    let context_row = chips::composer_context(app, &active, &props);

    // ---- what stands above the box
    let mut above: Vec<E> = Vec::new();
    if let Some(error) = app.fork_start_error() {
        say(&error);
        let retry = t("重试分叉首轮", "Retry pending fork runs");
        say(retry);
        above.push(row().key("composer-fork-error").role(Role::Alert).w_full().max_w(820.0).mx_auto().items_center().gap(7.0).mb(9.0).pl(9.0).pr(7.0).py(7.0).radius(8.0).bg(t_.critical_bg).border(t_.critical.fade(0.3)).child(text(error).ty(FINE).color(t_.critical).flex1().min_w(0.0)).child(button(retry).key(fork_retry_key()).h(25.0).px(8.0).on_click(|app: &mut App| app.retry_fork_starts())));
    }
    // The send path's reason a worktree could not be made, and the chips' own. The signal is one-shot, so what it said is kept here
    // until the draft's checkbox moves or the conversation changes.
    if let Some(error) = app.run_signals().branch_chip_error.take() {
        app.local::<SendWorktreeError>().0 = Some((id.clone(), error));
    }
    let signalled = app.local::<SendWorktreeError>().0.as_ref().filter(|(conversation, _)| *conversation == id).map(|(_, error)| error.clone());
    let branch_error = mewrk_pane_git::branch_chip_error(app, &id).or(signalled);
    if let Some(error) = branch_error {
        say(&error);
        above.push(row().key("composer-context-error").role(Role::Alert).w_full().max_w(820.0).mx_auto().items_start().gap(6.0).mt(-3.0).mb(7.0).px(2.0).child(icon("CircleAlert", 13.0).color(t_.critical).mt(1.0)).child(text(error).ty(FINE).color(t_.critical).flex1().min_w(0.0)));
    }

    // ---- the box
    let composer_state = app.composer();
    let draft = composer_state.drafts.get(&id).cloned().unwrap_or_default();
    let images = composer_state.image_drafts.get(&id).cloned().unwrap_or_default();
    let files = composer_state.file_drafts.get(&id).cloned().unwrap_or_default();
    let notice = composer_state.attachment_notices.get(&id).cloned().unwrap_or_default();
    let picks = composer_state.element_picks.get(&id).cloned().unwrap_or_default();
    let image_loading = composer_state.image_loading_ids.contains(&id);
    let visible_images = images_without_element_crops(&images, &picks);
    let has_payload = !draft.trim().is_empty() || !images.is_empty() || !files.is_empty();
    let queues_message = has_payload && (active.model_running || !active.queued_messages.is_empty());
    let stopping = app.model_is_stopping(&id);
    let workspace_deleting = active.workspace_id.as_deref().is_some_and(|workspace| app.workspace_is_deleting(workspace));
    // Images can outlive the model that accepted them: attach under a vision model, switch to a text-only one, and the request still
    // carries them. Sending then refuses deep in the pipeline, which would happen with no visible reason at all.
    let images_unsupported = chosen.as_ref().is_some_and(|choice| {
        !mewrk_client::model_capabilities::supports_vision(&choice.model)
            && (!images.is_empty()
                || match &active.draft_contexts {
                    Some(contexts) => contexts_contain_projected_images(contexts),
                    None => app.conversation(&id).is_some_and(|conversation| contexts_contain_projected_images(&conversation.contexts)),
                })
    });
    let inputs = SendInputs { model_running: active.model_running, queues_message, stopping, image_loading, lifecycle_running: active.lifecycle_running, has_model: chosen.is_some(), images_unsupported };
    let send_state = send_button_state(inputs);
    let reads_documents = chosen.as_ref().is_some_and(|choice| mewrk_client::model_capabilities::reads_pdf_documents(choice.provider.family, mewrk_client::model_capabilities::supports_vision(&choice.model)));

    let drop_state = app.local::<ComposerDrop>().0.clone();
    let placeholder = if question_pending { t("发送消息会先交回卡片上已填的回答…", "Sending hands back the card's answers first…") } else { t("向 Agent 发送消息…", "Message the Agent…") };
    say(placeholder);
    let appearance = &app.doc.global_settings.appearance;
    let (send_binding, newline_binding, spell_check) = (appearance.send_shortcut.clone(), appearance.newline_shortcut.clone(), appearance.spell_check);
    let card = owned.as_ref().filter(|prompt| prompt.kind == mewrk_client::runs::PromptKind::Question).map(|prompt| question_dock::dock_key(&prompt.prompt_id));
    let area = editor::composer_text_area(&id, draft, placeholder, active.lifecycle_running, spell_check, send_binding, newline_binding, card);

    let mut box_children: Vec<E> = Vec::new();
    if app.composer_is_unsent_task() {
        box_children.push(cat::composer_cat());
    }
    if let Some(overlay) = attachment_drop_overlay(&drop_state, active.image_input) {
        box_children.push(overlay);
    }
    // A failure that reached a turn is read in the timeline, where it happened. Only failures with no turn to land on, a send refused
    // before the run started, still need the composer to carry them.
    if let Some(error) = app.model_run_error(&id).filter(|error| error.request_id.is_none() && !active.model_running) {
        let dismissing = id.clone();
        let mut actions: Vec<E> = Vec::new();
        if error.retryable {
            let retry = t("重试", "Retry");
            say(retry);
            actions.push(button(retry).key(run_retry_key()).h(25.0).px(8.0).on_click(|app: &mut App| app.retry_model_run()));
        }
        let dismiss = t("关闭模型错误", "Dismiss model error");
        say(dismiss);
        actions.push(icon_button("X", dismiss).key(run_dismiss_key()).on_click(move |app: &mut App| app.dismiss_model_run_error(&dismissing)));
        box_children.push(run_error("model", format!("{} · {}", error.provider_name, error.model_name), error.message.clone(), actions));
    }
    if images_unsupported && let Some(choice) = &chosen {
        let detail = if images.is_empty() {
            t("对话里有图片（某条消息或工具截图中），而当前模型没有启用图片输入，因此无法发送。请换一个支持视觉的模型，或从时间线上删除这些图片。", "This conversation has images (in a message or a tool's screenshot), and the selected model has no image input enabled, so nothing can be sent. Switch to a vision model, or delete those images from the timeline.")
        } else {
            t("这条消息带有图片，但当前模型没有启用图片输入，因此无法发送。请换一个支持视觉的模型，或移除图片。", "This message carries images, but the selected model has no image input enabled, so it cannot be sent. Switch to a vision model, or remove the images.")
        };
        box_children.push(run_error("images", format!("{} · {}", choice.provider.name, choice.model.id), detail.to_owned(), Vec::new()));
    }
    if let Some(chips) = selected_element_chips(&id, &picks) {
        box_children.push(chips);
    }
    let dismissing = id.clone();
    if let Some(notice) = attachment_notice(&notice, move |app: &mut App| {
        app.composer().attachment_notices.remove(&dismissing);
        repaint();
    }) {
        box_children.push(notice);
    }
    let (removing_image, removing_file) = (id.clone(), id.clone());
    if let Some(strip) = image_strip(app, ImageStripProps {
        key: format!("composer:{id}"),
        images: visible_images,
        files,
        compact: true,
        busy: image_loading,
        on_remove: Some(Rc::new(move |app, image| app.remove_composer_image(&removing_image, image))),
        on_remove_file: Some(Rc::new(move |app, file| app.remove_composer_file(&removing_file, file))),
        reads_documents,
    }) {
        box_children.push(strip.mb(8.0));
    }

    // ---- send
    let mut send_row: E = row().shrink0().items_center().gap(6.0);
    if send_state.separate_stop {
        // Queuing repurposes the primary button, but a running conversation must retain a visible stop control.
        let label = if stopping { t("正在停止生成", "Stopping generation") } else { t("停止生成", "Stop generating") };
        say(label);
        let stopping_id = id.clone();
        let stop: E = row().key(stop_button_key()).center().size(29.0, 29.0).radius(7.0).tooltip(label).child(stop_mark(if stopping { t_.text_disabled } else { t_.text_primary }));
        send_row = send_row.child(if stopping { stop.disabled(true).on_click(|_: &mut App| {}) } else { stop.hover_bg(t_.subtle_hover).on_click(move |app: &mut App| app.cancel_run(&stopping_id, |_, _| {})) });
    }
    let primary_label = send_button_label(inputs, question_pending, workspace_deleting);
    say(primary_label);
    let primary_color = if send_state.primary_disabled { t_.text_disabled } else { t_.text_primary };
    let primary: E = row().key(send_button_key()).center().size(29.0, 29.0).radius(7.0).tooltip(primary_label).child(if send_state.primary_stops { stop_mark(primary_color) } else { icon("ArrowUp", 17.0).color(primary_color) });
    let stopping_id = id.clone();
    let primary_stops = send_state.primary_stops;
    send_row = send_row.child(if send_state.primary_disabled {
        primary.disabled(true).on_click(|_: &mut App| {}).when(workspace_deleting && !inputs.queues_message && !inputs.model_running, |e| e.disabled_reason(primary_label))
    } else {
        primary.hover_bg(t_.subtle_hover).keep_focus().on_click(move |app: &mut App| {
            if primary_stops {
                app.cancel_run(&stopping_id, |_, _| {});
            } else {
                app.send_message();
            }
        })
    });

    box_children.push(row().items_end().gap(8.0).child(area.element).child(send_row).child(area.menu));

    // The drag's state for next frame, read where the input and the box's rectangle both are.
    let (drag_id, image_input, drag_disabled) = (id.clone(), active.image_input, active.lifecycle_running);
    box_children.push(
        custom(move |app: &mut App, ui: &mut Ui<'_>, _: Rect| {
            let zone = rect_of(composer_box_key()).unwrap_or(rect(0.0, 0.0, 0.0, 0.0));
            let next = watch_drag(app, ui, zone, &drag_id, image_input, drag_disabled);
            let slot = app.local::<ComposerDrop>();
            if slot.0 != next {
                slot.0 = next;
                repaint();
            }
        })
        .w(0.0)
        .h(0.0)
        .pass_through(),
    );

    let edge = if drop_state.dragging && !drop_state.over {
        t_.control_strong_stroke
    } else if area.focused {
        t_.accent.fade(0.6)
    } else {
        t_.card_stroke
    };
    let composer_box: E = col().key(composer_box_key()).w_full().max_w(820.0).mx_auto().px(12.0).py(8.0).radius(14.0).bg(t_.card).border(edge).children(box_children);

    let footer = footer::composer_footer(app, &active, chosen.as_ref());

    let mut wrap: E = col().key("composer-wrap").w_full().shrink0().px(wrap_padding()).pb(12.0).child(approval).child(question).child(queued).child(context_row);
    for element in above {
        wrap = wrap.child(element);
    }
    wrap.child(composer_box).child(footer)
}

/// The timeline's empty state on a task nothing has been sent in.
pub fn draft_landing(app: &mut App) -> E {
    if !app.composer_is_unsent_task() {
        return div();
    }
    let recent_tasks = app.draft_landing_tasks();
    let settings = &app.doc.global_settings;
    let (shortcuts, send_shortcut, newline_shortcut) = (settings.shortcuts.clone(), settings.appearance.send_shortcut.clone(), settings.appearance.newline_shortcut.clone());
    // `activeTimelineMutationBlocked`: a right-click writes to the timeline, which a run or a project-wide operation holds.
    let chosen = active_model_choice(app);
    let can_insert = read_active(app, chosen.as_ref()).is_some_and(|active| !app.model_run_is_active(&active.id) && !active.lifecycle_running);
    let workspace = app.active_workspace_id();
    landing::draft_landing_view(landing::DraftLandingProps {
        recent_tasks,
        shortcuts,
        send_shortcut,
        newline_shortcut,
        can_insert,
        now: None,
        on_open_task: Rc::new(move |app, conversation| {
            if let Some(workspace) = &workspace {
                app.select_conversation(workspace, conversation);
            }
        }),
    })
}
