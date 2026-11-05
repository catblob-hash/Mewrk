//! `ConversationTemplateEditor.tsx`: a template's message queue, opened to be worked on rather than merely read.
//!
//! The surface is the ordinary conversation view with its ordinary editors and right-click menu, because a template is a message queue and a second way of editing one would be a second renderer to
//! keep in step. What differs is where the edits go: nothing here touches a conversation, so every change is held here until a save writes the whole body back in one call.
//!
//! A template belongs to no conversation and no workspace, so this is the one surface that withholds running a tool: a card is written down, and applying the template is what later makes it a call in
//! some conversation. What it does have to do is stop claiming what it cannot know: a rewritten card drops the stored attestation and the model's original arguments, and a placed one starts with the
//! result the host would mint for a call nobody ran.

use std::collections::HashSet;

use mewrk_client::model::{ContextItem, FileAttachment, ImageAttachment, JsonObject, ReasoningForm, ToolCategory, ToolDescriptor, ToolResult};
use mewrk_client::model_capabilities::is_encrypted_reasoning;
use mewrk_client::prelude::*;
use mewrk_client::runs::image_short_ids::image_short_ids_in_use;
use mewrk_client::runs::{ConversationTurn, LiveContext};
use mewrk_ui_chat::frame::insert_kind_of;
use mewrk_ui_chat::handlers::message_attachment_adder_against;
use mewrk_ui_chat::inline_text_editor::InsertKind;
use mewrk_ui_chat::orchestration::{answers_from_formatted_content, is_claude_question_input, questions_from_input};
use mewrk_ui_chat::{conversation_view, ConversationViewProps, TimelineEditorState, TimelineQuestionEditorState};

use crate::logic::is_host_derived_tool_name;
use mewrk_client::doc::{create_id, now_iso};

use crate::ops::Done;
use crate::widgets::*;

#[derive(Default)]
pub struct EditorState {
    draft: Option<Vec<ContextItem>>,
    /// The draft as the timeline reads it. The timeline knows a body by this allocation, so it is replaced when the draft changes and at no other time.
    drawn: Rc<Vec<LiveContext>>,
    /// A template has no turns, but the timeline knows those by their allocation too, and a new empty list every frame would have it project the body again every frame.
    turns: Rc<Vec<ConversationTurn>>,
    /// Bumped on every committed edit. Autosave compares it with `written` rather than `dirty`, so a refused write is reported once instead of retried forever, and seeding
    /// a freshly read body writes nothing back.
    draft_version: u64,
    written: u64,
    seeded: bool,
    editor: Option<TimelineEditorState>,
    question_editor: Option<TimelineQuestionEditorState>,
    dirty: bool,
    saving: bool,
    error: Option<String>,
    /// The tools a save would leave uncallable, held while the user answers for them. Never empty.
    unenabled: Option<Vec<String>>,
    /// Names already asked about: autosave has no one save moment to raise the question at, and re-raising it on every edit would make the page unusable.
    asked: HashSet<String>,
}

impl EditorState {
    fn redraw(&mut self) {
        self.drawn = Rc::new(self.draft.iter().flatten().cloned().map(LiveContext::settled).collect());
    }
}

pub struct TemplateEditor {
    /// Names this editor's state. A new key starts it again on a new body.
    pub key: String,
    pub template_id: String,
    /// `None` while the host is still reading the body.
    pub contexts: Option<Vec<ContextItem>>,
    pub tools: Rc<Vec<ToolDescriptor>>,
    /// The tools a card may be placed for: a template can only call what its owner hands the model.
    pub enabled_tools: Vec<String>,
    /// False draws the same body with every mutation affordance withheld.
    pub editable: bool,
    /// Whether the model this body will run on reads images. The owner answers for it.
    pub image_input_supported: bool,
    /// Writes each committed edit back as it lands, instead of drawing a save button under the page.
    pub autosave: bool,
    pub on_save: Option<Rc<dyn Fn(&mut App, Vec<ContextItem>, Done<()>)>>,
    /// Widens the owner's enabled set to cover what a stored body still calls. Its absence is what makes saving skip the question.
    pub on_enable_tools: Option<Rc<dyn Fn(&mut App, Vec<String>)>>,
}

/// The tool names a template may be asked to enable on its owner's behalf: exactly the set the features page draws switches for. A card calling a tool the catalog no longer
/// carries is nothing anyone can enable, so it is not reported either.
pub fn unenabled_tools(draft: &[ContextItem], enabled: &[String], tools: &[ToolDescriptor]) -> Vec<String> {
    let enableable: HashSet<&str> = tools.iter().filter(|t| t.category != ToolCategory::Memory && !is_host_derived_tool_name(&t.name)).map(|t| t.name.as_str()).collect();
    let mut missing: Vec<String> = Vec::new();
    for item in draft {
        if let ContextItem::Tool { tool_name, .. } = item
            && !enabled.contains(tool_name)
            && enableable.contains(tool_name.as_str())
            && !missing.contains(tool_name)
        {
            missing.push(tool_name.clone());
        }
    }
    missing
}

/// A placed card: what was typed, and the result the host would mint for a call nobody ran.
pub fn placed_tool(tool_name: String, input: JsonObject, output: String) -> ContextItem {
    ContextItem::Tool {
        id: create_id("ctx"),
        tool_name,
        round: None,
        model_turn_id: None,
        provider_call_id: None,
        requested_input: None,
        input,
        result: ToolResult { success: true, output, images: Vec::new(), diff: None, executed_at: String::new(), duration_ms: 0 },
        subagent: None,
        notice: None,
        attestation: String::new(),
        created_at: now_iso(),
    }
}

/// A placed message. Hand-written reasoning is plaintext by construction: the user supplied every word of it. A user message carries what was pasted into it; every other kind has nowhere to put an image and is never handed one.
pub fn placed_text(kind: InsertKind, content: String, images: Vec<ImageAttachment>, files: Vec<FileAttachment>) -> Option<ContextItem> {
    let (id, created_at) = (create_id("ctx"), now_iso());
    Some(match kind {
        InsertKind::Reasoning => ContextItem::Reasoning { id, content: Some(content), form: Some(ReasoningForm::Plaintext), round: None, model_turn_id: None, interrupted: false, duration_ms: None, tokens: None, replay: None, created_at },
        InsertKind::User => ContextItem::User { id, content, images, files, created_at },
        InsertKind::Assistant => ContextItem::Assistant { id, content, round: None, model_turn_id: None, interrupted: false, sources: Vec::new(), created_at },
        InsertKind::System => ContextItem::System { id, content, local_only: false, hook_execution: None, tools_added: Vec::new(), native_compaction: None, created_at },
        // A tool has no text editor to have come from.
        InsertKind::Tool => return None,
    })
}

/// A prose edit. An edited assistant or reasoning card is no longer an interrupted one: the user finished it.
pub fn with_text(item: &mut ContextItem, value: String, new_images: Vec<ImageAttachment>, new_files: Vec<FileAttachment>) {
    match item {
        ContextItem::System { content, .. } => *content = value,
        ContextItem::User { content, images, files, .. } => {
            *content = value;
            *images = new_images;
            *files = new_files;
        }
        ContextItem::Assistant { content, interrupted, .. } => {
            *content = value;
            *interrupted = false;
        }
        ContextItem::Reasoning { content, interrupted, .. } => {
            *content = Some(value);
            *interrupted = false;
        }
        ContextItem::Tool { .. } => {}
    }
}

/// A rewritten card stops claiming what this surface cannot know: the stored conversation's attestation and the model's original arguments. The host owns the rest of the result.
pub fn with_tool_edit(item: &mut ContextItem, new_input: JsonObject, new_result: Option<(String, Vec<ImageAttachment>)>) {
    if let ContextItem::Tool { requested_input, attestation, input, result, .. } = item {
        *requested_input = None;
        attestation.clear();
        *input = new_input;
        if let Some((output, images)) = new_result {
            result.output = output;
            result.images = images;
        }
    }
}

/// Writes a placed or rewritten tool card into the draft. Nothing is executed and nothing is attested here: the body is not a conversation yet, and the host re-issues every card's credential when the template is applied.
pub fn write_tool_card(draft: &mut Vec<ContextItem>, editor: Option<&TimelineEditorState>, input: JsonObject, output: String, images: Vec<ImageAttachment>) -> Result<(), String> {
    match editor {
        None => Err(t("没有正在编辑的上下文", "No context is being edited").to_owned()),
        Some(TimelineEditorState::Insert { kind: InsertKind::Tool, index, tool_name: Some(tool_name) }) => {
            draft.insert((*index).min(draft.len()), placed_tool(tool_name.clone(), input, output));
            Ok(())
        }
        Some(TimelineEditorState::Insert { .. }) => Err(t("没有选择工具", "No tool was chosen").to_owned()),
        Some(TimelineEditorState::Edit { item: ContextItem::Tool { id, .. }, .. }) => {
            if let Some(card) = draft.iter_mut().find(|item| item.id() == id) {
                with_tool_edit(card, input, Some((output, images)));
            }
            Ok(())
        }
        Some(TimelineEditorState::Edit { .. }) => Err(t("这条上下文不是工具调用", "This context is not a tool call").to_owned()),
    }
}

/// Rewrites a question card and, when it has one, the answer that settled it. A question is a tool card like any other, so the host owns the result and the draft owns the arguments. The shape rules are the
/// card's own, the same count of questions and no answer emptied, because a card whose answer no longer covers its questions renders as a broken one wherever the template is applied.
pub fn write_question_card(draft: &mut [ContextItem], editor: Option<&TimelineQuestionEditorState>, input: JsonObject, answer_content: Option<String>) -> Result<(), String> {
    let no_question = || t("没有活动提问", "No active question").to_owned();
    let editor = editor.ok_or_else(no_question)?;
    let ContextItem::Tool { input: original, .. } = &editor.item else { return Err(no_question()) };
    let next = questions_from_input(&input);
    if !is_claude_question_input(&input) || next.len() != questions_from_input(original).len() {
        return Err(t("提问数量必须保持不变，且每一项都必须符合提问格式", "The number of questions must stay unchanged and every item must remain valid").to_owned());
    }
    if editor.answer.is_some() {
        let answers = answers_from_formatted_content(answer_content.as_deref().unwrap_or(""), &next);
        if !answers.is_some_and(|answers| answers.len() == next.len() && answers.iter().all(|answer| !answer.trim().is_empty())) {
            return Err(t("每一个问题都必须保留非空回答", "Every question must keep a non-empty answer").to_owned());
        }
    }
    let answer_id = editor.answer.as_ref().map(ContextItem::id);
    for item in draft {
        if item.id() == editor.item.id() {
            with_tool_edit(item, input.clone(), None);
        } else if answer_id == Some(item.id())
            && let ContextItem::User { content, .. } = item
            && let Some(answer) = &answer_content
        {
            content.clone_from(answer);
        }
    }
    Ok(())
}

impl TemplateEditor {
    pub fn view(self, app: &mut App) -> E {
        let t_ = theme();
        let state = use_state(("template-editor", self.key.as_str()), EditorState::default);
        // The body arrives once, after the page is already mounted. Seeding on every change of the prop would discard the edits made since it landed.
        if !state.with(|s| s.seeded)
            && let Some(contexts) = &self.contexts
        {
            let mut s = state.borrow_mut();
            s.seeded = true;
            s.draft = Some(contexts.clone());
            s.redraw();
            s.written = s.draft_version;
        }
        let this = Rc::new(self);
        // Autosave writes what the last committed edit produced. An emptied body is skipped: the host refuses it, and there is no button here to draw disabled instead.
        let wants_write = state.with(|s| this.autosave && this.on_save.is_some() && !s.saving && s.draft.as_ref().is_some_and(|d| !d.is_empty()) && s.written != s.draft_version);
        if wants_write {
            state.update(|s| s.written = s.draft_version);
            request_save(app, &this, &state);
        }
        let mut out = col().flex1().min_h(0.0).min_w(0.0);
        out = out.child(if state.with(|s| s.draft.is_none()) { label(t("正在读取…", "Loading…")).px(16.0).py(20.0) } else { timeline(app, &this, &state) });
        // Without autosave the page has nowhere else to put its save, so it takes a strip along the bottom.
        if this.editable && this.on_save.is_some() && !this.autosave {
            let (empty, dirty, saving, error) = state.with(|s| (s.draft.as_ref().is_none_or(Vec::is_empty), s.dirty, s.saving, s.error.clone()));
            let (th, st) = (this.clone(), state.clone());
            let mut save = btn(("template-save", this.key.as_str()), Look::Subtle, if saving { t("正在保存…", "Saving…") } else { t("保存模板", "Save template") }).disabled(empty || !dirty || saving).when(empty || !dirty || saving, |e| e.opacity(0.5));
            // An emptied template is refused by the host, and saying so before the round trip is kinder than saying it after.
            if empty {
                let why = t("模板至少要保留一条消息。", "A template must keep at least one message.");
                save = save.tip(why).disabled_reason(why);
            }
            out = out.child(row().items_center().gap(10.0).min_h(52.0).px(14.0).py(9.0).border_top(t_.divider).child(save.on_click(move |app: &mut App| request_save(app, &th, &st))).child(error.map(|e| error_text(e).role(Role::Alert))));
        }
        if this.autosave
            && let Some(error) = state.with(|s| s.error.clone())
        {
            // With no action row there is no free space to report into, so a refused write takes a strip of its own rather than vanishing.
            out = out.child(div().px(14.0).py(7.0).border_top(t_.divider).child(error_text(error).role(Role::Alert)));
        }
        if let Some(names) = state.with(|s| s.unenabled.clone()) {
            out = out.child(unenabled_dialog(&this, &state, names));
        }
        out
    }
}

/// The ordinary conversation view over the draft.
fn timeline(app: &mut App, this: &Rc<TemplateEditor>, state: &Local<EditorState>) -> E {
    let timeline_id = if this.template_id.is_empty() { "template:unsaved".to_owned() } else { this.template_id.clone() };
    let (contexts, turns, editor, question_editor) = state.with(|s| (s.drawn.clone(), s.turns.clone(), s.editor.clone(), s.question_editor.clone()));
    let handlers = if this.editable { edit_handlers(state, this.image_input_supported) } else { ConversationViewProps::default() };
    conversation_view(
        app,
        ConversationViewProps {
            editable: this.editable,
            contexts,
            turns,
            tools: this.tools.clone(),
            enabled_tools: Rc::new(this.enabled_tools.clone()),
            timeline_id: Some(timeline_id),
            aria_label: Some(t("模板消息队列", "Template message queue").to_owned()),
            editor: editor.filter(|_| this.editable),
            question_editor: question_editor.filter(|_| this.editable),
            attachment_image_input: Some(this.image_input_supported),
            ..handlers
        },
    )
}

/// What an editable body hands the view. Upstream gives this surface no move, copy, paste, undo or fork, so those stay absent. Nor is there an `on_save_tool`: that handler is the one that executes, and its absence is what keeps the run button off the card editor.
fn edit_handlers(state: &Local<EditorState>, image_input: bool) -> ConversationViewProps {
    let st = || state.clone();
    let (on_edit, on_delete, on_edit_question, on_delete_question, on_delete_contexts, on_insert) = (st(), st(), st(), st(), st(), st());
    let (on_cancel, on_save_text, numbers, on_save_tool, on_save_question) = (st(), st(), st(), st(), st());
    ConversationViewProps {
        on_edit: Some(Rc::new(move |_, item| begin_edit(&on_edit, item))),
        on_delete: Some(Rc::new(move |_, item| remove_ids(&on_delete, &[item.id().to_owned()]))),
        on_edit_question: Some(Rc::new(move |_, item, answer| on_edit_question.update(|s| s.question_editor = Some(TimelineQuestionEditorState { item: item.clone(), answer: answer.cloned() })))),
        on_delete_question: Some(Rc::new(move |_, item, answer| {
            let mut ids = vec![item.id().to_owned()];
            ids.extend(answer.map(|answer| answer.id().to_owned()));
            remove_ids(&on_delete_question, &ids);
        })),
        on_delete_contexts: Some(Rc::new(move |_, ids| remove_ids(&on_delete_contexts, &ids))),
        on_insert: Some(Rc::new(move |_, index, kind, tool_name| on_insert.update(|s| s.editor = Some(TimelineEditorState::Insert { kind, index, tool_name })))),
        on_cancel_edit: Some(Rc::new(move |_| {
            on_cancel.update(|s| {
                s.editor = None;
                s.question_editor = None;
            });
        })),
        on_save_text: Some(Rc::new(move |_, content, images, files| save_text(&on_save_text, content, images, files))),
        // A template has no transcript behind it, so its own body is the whole of what a number can already be spoken for by.
        on_add_attachments: Some(message_attachment_adder_against(image_input, Rc::new(move |_| numbers.with(|s| image_short_ids_in_use(s.draft.iter().flatten()))))),
        on_save_tool_edit: Some(Rc::new(move |app, input, output, images, done| {
            let editor = on_save_tool.with(|s| s.editor.clone());
            let result = try_amend(&on_save_tool, |draft| write_tool_card(draft, editor.as_ref(), input, output, images));
            if result.is_ok() {
                on_save_tool.update(|s| s.editor = None);
            }
            done(app, result);
        })),
        on_save_question: Some(Rc::new(move |app, input, answer, done| {
            let editor = on_save_question.with(|s| s.question_editor.clone());
            let result = try_amend(&on_save_question, |draft| write_question_card(draft, editor.as_ref(), input, answer));
            if result.is_ok() {
                on_save_question.update(|s| s.question_editor = None);
            }
            done(app, result);
        })),
        ..ConversationViewProps::default()
    }
}

fn begin_edit(state: &Local<EditorState>, item: &ContextItem) {
    // Encrypted reasoning is delete-only: its body never reached this client, so anything typed in its place would be invented history.
    if let ContextItem::Reasoning { form, content, .. } = item
        && is_encrypted_reasoning(*form, content.as_deref())
    {
        return;
    }
    state.update(|s| {
        if let Some(index) = s.draft.iter().flatten().position(|context| context.id() == item.id()) {
            s.editor = Some(TimelineEditorState::Edit { kind: insert_kind_of(item), item: item.clone(), index });
        }
    });
}

fn remove_ids(state: &Local<EditorState>, ids: &[String]) {
    let dropped = |id: &str| ids.iter().any(|held| held == id);
    state.update(|s| {
        if matches!(&s.editor, Some(TimelineEditorState::Edit { item, .. }) if dropped(item.id())) {
            s.editor = None;
        }
        if s.question_editor.as_ref().is_some_and(|editor| dropped(editor.item.id())) {
            s.question_editor = None;
        }
    });
    amend(state, |draft| draft.retain(|context| !dropped(context.id())));
}

fn save_text(state: &Local<EditorState>, content: String, images: Vec<ImageAttachment>, files: Vec<FileAttachment>) {
    let Some(editor) = state.with(|s| s.editor.clone()) else { return };
    match editor {
        TimelineEditorState::Edit { item, .. } => amend(state, |draft| {
            if let Some(held) = draft.iter_mut().find(|context| context.id() == item.id()) {
                with_text(held, content, images, files);
            }
        }),
        TimelineEditorState::Insert { kind, index, .. } => {
            if let Some(item) = placed_text(kind, content, images, files) {
                amend(state, |draft| draft.insert(index.min(draft.len()), item));
            }
        }
    }
    state.update(|s| s.editor = None);
}

/// Starts a save: asks about tools the owner turned off afterwards, or writes. Nothing to ask when the answer could not be acted on.
fn request_save(app: &mut App, this: &Rc<TemplateEditor>, state: &Local<EditorState>) {
    let Some(draft) = state.with(|s| if s.saving { None } else { s.draft.clone() }) else { return };
    if this.on_save.is_none() {
        return;
    }
    let missing = unenabled_tools(&draft, &this.enabled_tools, &this.tools);
    let pending: Vec<String> = if this.autosave { state.with(|s| missing.iter().filter(|n| !s.asked.contains(*n)).cloned().collect()) } else { missing };
    if !pending.is_empty() && this.on_enable_tools.is_some() {
        state.update(|s| {
            s.asked.extend(pending.iter().cloned());
            s.unenabled = Some(pending);
        });
        return;
    }
    commit(app, this, state, draft);
}

fn commit(app: &mut App, this: &Rc<TemplateEditor>, state: &Local<EditorState>, body: Vec<ContextItem>) {
    let Some(on_save) = this.on_save.clone() else { return };
    state.update(|s| {
        s.saving = true;
        s.error = None;
    });
    let state = state.clone();
    on_save(
        app,
        body,
        Box::new(move |_app, result| {
            state.update(|s| {
                s.saving = false;
                match result {
                    Ok(()) => s.dirty = false,
                    Err(reason) => s.error = Some(reason),
                }
            });
        }),
    );
}

/// An edit that may be refused: a refusal leaves the draft, and whether it counts as changed, as they were.
fn try_amend(state: &Local<EditorState>, change: impl FnOnce(&mut Vec<ContextItem>) -> Result<(), String>) -> Result<(), String> {
    state.update(|s| {
        let Some(draft) = &mut s.draft else { return Ok(()) };
        change(draft)?;
        s.redraw();
        s.draft_version += 1;
        s.dirty = true;
        s.error = None;
        Ok(())
    })
}

fn amend(state: &Local<EditorState>, change: impl FnOnce(&mut Vec<ContextItem>)) {
    let _ = try_amend(state, |draft| {
        change(draft);
        Ok(())
    });
}

/// Turning a tool off after a template was written for it is a thing the user is entitled to do, so this asks rather than refuses, and offers the repair.
fn unenabled_dialog(this: &Rc<TemplateEditor>, state: &Local<EditorState>, names: Vec<String>) -> E {
    let list = names.iter().fold(col().gap(6.0), |c, name| {
        let lbl = this.tools.iter().find(|tool| tool.name == *name).map_or_else(|| name.clone(), |tool| tool.label.clone());
        c.child(col().child(strong(lbl)).child(small(name.clone())))
    });
    let (th1, st1) = (this.clone(), state.clone());
    let (th2, st2) = (this.clone(), state.clone());
    let names2 = names;
    let st3 = state.clone();
    plain_dialog(
        ("template-unenabled", this.key.as_str()),
        560.0,
        t("模板里有未启用的工具", "This template uses tools that are not enabled").to_owned(),
        Some(t("这些工具当前是关闭的，套用后模型无法调用。", "These tools are off, so the model can't call them once applied.").to_owned()),
        Some(list),
        vec![
            btn(("template-save-anyway", this.key.as_str()), Look::Standard, t("正常保存", "Save anyway")).on_click(move |app: &mut App| {
                st1.update(|s| s.unenabled = None);
                if let Some(d) = st1.with(|s| s.draft.clone()) {
                    commit(app, &th1, &st1, d);
                }
            }),
            btn(("template-enable-tools", this.key.as_str()), Look::Accent, t("自动启用所需工具", "Enable the tools it needs")).on_click(move |app: &mut App| {
                if let Some(f) = &th2.on_enable_tools {
                    f(app, names2.clone());
                }
                st2.update(|s| s.unenabled = None);
                if let Some(d) = st2.with(|s| s.draft.clone()) {
                    commit(app, &th2, &st2, d);
                }
            }),
        ],
        Rc::new(move |_| st3.update(|s| s.unenabled = None)),
    )
}
