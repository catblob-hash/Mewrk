//! What upstream's `ConversationViewProps`, `ContextStreamProps` and `StreamedConversationViewProps` carried, as one struct.
//! An absent handler physically removes its affordance, exactly as an absent prop did upstream.

use std::collections::HashMap;
use std::rc::Rc;

use mewrk_client::conversation_branches::ContextBranchNavigation;
use mewrk_client::model::{ContextItem, ToolDescriptor};
use mewrk_client::prelude::*;
use mewrk_client::runs::{ConversationTurn, LiveContext, LiveReasoningView, ModelRunRetryState};
use mewrk_pane_tasks::subagents::SubagentView;
use mewrk_pane_tasks::workflow_runs::WorkflowRunView;

use crate::inline_question_editor::SaveQuestion;
use crate::inline_text_editor::{AddAttachments, InsertKind, SaveText};
use crate::inline_tool_editor::{RunTool, SaveToolEdit};
use crate::turn_changes::ChangeSpan;

/// An element a caller supplies: a closure, because it is built every frame and may need the app.
pub type Slot = Rc<dyn Fn(&mut App) -> E>;
pub type OnContext = Rc<dyn Fn(&mut App, &ContextItem)>;
pub type OnQuestion = Rc<dyn Fn(&mut App, &ContextItem, Option<&ContextItem>)>;
pub type OnIds = Rc<dyn Fn(&mut App, Vec<String>)>;
pub type OnMove = Rc<dyn Fn(&mut App, Vec<String>, usize)>;
pub type OnPaste = Rc<dyn Fn(&mut App, usize, Vec<String>)>;
pub type OnInsert = Rc<dyn Fn(&mut App, usize, InsertKind, Option<String>)>;
pub type OnIndex = Rc<dyn Fn(&mut App, usize)>;
pub type OnId = Rc<dyn Fn(&mut App, &str)>;
pub type OnSelectBranch = Rc<dyn Fn(&mut App, &str, &str)>;
pub type OpenInsert = Rc<dyn Fn(&mut App, PointerEvent, usize)>;

/// The one editor open on a timeline. Editing happens in the card itself, so the owner keeps the state and the stream
/// decides which card gives up its body for it.
#[derive(Clone, Debug, PartialEq)]
pub enum TimelineEditorState {
    Insert { kind: InsertKind, index: usize, tool_name: Option<String> },
    Edit { kind: InsertKind, item: ContextItem, index: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimelineQuestionEditorState {
    pub item: ContextItem,
    pub answer: Option<ContextItem>,
}

#[derive(Clone)]
pub struct ConversationViewProps {
    // ---- ConversationView
    /// Controls only mutation affordances; message and tool rendering stays identical.
    pub editable: bool,
    pub before_timeline: Option<Slot>,
    pub composer: Option<Slot>,
    /// Drawn in place of the timeline while the conversation's body is not in memory yet.
    pub timeline_placeholder: Option<Slot>,

    // ---- StreamedConversationView
    /// The conversation whose persisted contexts and live run this view shows. Set for `streamed_conversation_view`.
    pub conversation_id: Option<String>,
    /// The conversation's agent roster, as the shell already derives it for the task container.
    pub agents: Rc<Vec<Rc<SubagentView>>>,

    // ---- ContextStream
    /// The contexts of a view with no conversation behind it: a template, a subagent's transcript.
    pub contexts: Rc<Vec<LiveContext>>,
    /// Frontend-only presentation spans; never used to assemble model context.
    pub turns: Rc<Vec<ConversationTurn>>,
    /// Finished stretches of work whose changed files are listed at their end. When absent, the stretches between the
    /// user's messages, read off the timeline itself; a subagent's transcript brings its own.
    pub change_spans: Option<Rc<Vec<ChangeSpan>>>,
    pub tools: Rc<Vec<ToolDescriptor>>,
    pub enabled_tools: Rc<Vec<String>>,
    /// Distinguishes wholesale timeline switches from appended output.
    pub timeline_id: Option<String>,
    /// Temporarily locks timeline edits while preserving main-surface controls such as Run and branch navigation.
    pub timeline_mutation_locked: bool,
    /// True for the full model round, including the gaps before the first and between provider stream events.
    pub streaming: bool,
    pub thinking: Option<LiveReasoningView>,
    pub retry_notice: Option<ModelRunRetryState>,
    pub running_task_count: usize,
    pub on_open_tasks: Option<Handler<App>>,
    pub aria_label: Option<String>,
    /// Drawn in place of the stock notice while the timeline holds nothing. It still sits on the timeline, so a right-click on it inserts.
    pub empty_state: Option<Slot>,
    pub on_edit: Option<OnContext>,
    pub on_delete: Option<OnContext>,
    pub on_edit_question: Option<OnQuestion>,
    pub on_delete_question: Option<OnQuestion>,
    pub editor: Option<TimelineEditorState>,
    pub question_editor: Option<TimelineQuestionEditorState>,
    pub on_cancel_edit: Option<Handler<App>>,
    pub on_save_text: Option<SaveText>,
    pub on_add_attachments: Option<AddAttachments>,
    pub attachment_image_input: Option<bool>,
    pub on_save_tool: Option<RunTool>,
    pub on_save_tool_edit: Option<SaveToolEdit>,
    pub on_save_question: Option<SaveQuestion>,
    pub on_branch_from: Option<OnContext>,
    pub branch_from_disabled_reason: Option<String>,
    pub on_rewind_to: Option<OnContext>,
    pub rewind_disabled_reason: Option<String>,
    pub branch_navigations: Rc<HashMap<String, ContextBranchNavigation>>,
    pub on_select_branch: Option<OnSelectBranch>,
    pub branch_switch_disabled_reason: Option<String>,
    pub on_insert: Option<OnInsert>,
    pub on_delete_contexts: Option<OnIds>,
    pub on_move_contexts: Option<OnMove>,
    pub on_copy_contexts: Option<OnIds>,
    pub on_paste_contexts: Option<OnPaste>,
    pub paste_count: usize,
    pub on_undo: Option<Handler<App>>,
    pub on_redo: Option<Handler<App>>,
    pub on_fork_at: Option<OnIndex>,
    pub fork_disabled_reason: Option<String>,
    pub on_open_subagent: Option<OnId>,
    /// Workflow runs, keyed by the owning `workflow` tool call id. A call with no entry renders no card.
    pub workflow_run_by_call: Rc<HashMap<String, Rc<WorkflowRunView>>>,
    pub on_open_workflow_run: Option<OnId>,
    pub on_retry_turn_error: Option<Handler<App>>,
    pub retryable_turn_request_id: Option<String>,
    pub on_dismiss_turn_error: Option<Handler<App>>,
    /// Working directory that relative paths in model output resolve against. Absolute paths are clickable without it.
    pub path_base_dir: Option<String>,
}

impl Default for ConversationViewProps {
    fn default() -> Self {
        Self {
            editable: true,
            before_timeline: None,
            composer: None,
            timeline_placeholder: None,
            conversation_id: None,
            agents: Rc::default(),
            contexts: Rc::default(),
            turns: Rc::default(),
            change_spans: None,
            tools: Rc::default(),
            enabled_tools: Rc::default(),
            timeline_id: None,
            timeline_mutation_locked: false,
            streaming: false,
            thinking: None,
            retry_notice: None,
            running_task_count: 0,
            on_open_tasks: None,
            aria_label: None,
            empty_state: None,
            on_edit: None,
            on_delete: None,
            on_edit_question: None,
            on_delete_question: None,
            editor: None,
            question_editor: None,
            on_cancel_edit: None,
            on_save_text: None,
            on_add_attachments: None,
            attachment_image_input: None,
            on_save_tool: None,
            on_save_tool_edit: None,
            on_save_question: None,
            on_branch_from: None,
            branch_from_disabled_reason: None,
            on_rewind_to: None,
            rewind_disabled_reason: None,
            branch_navigations: Rc::default(),
            on_select_branch: None,
            branch_switch_disabled_reason: None,
            on_insert: None,
            on_delete_contexts: None,
            on_move_contexts: None,
            on_copy_contexts: None,
            on_paste_contexts: None,
            paste_count: 0,
            on_undo: None,
            on_redo: None,
            on_fork_at: None,
            fork_disabled_reason: None,
            on_open_subagent: None,
            workflow_run_by_call: Rc::default(),
            on_open_workflow_run: None,
            on_retry_turn_error: None,
            retryable_turn_request_id: None,
            on_dismiss_turn_error: None,
            path_base_dir: None,
        }
    }
}
