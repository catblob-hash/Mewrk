//! The conversation view: upstream's StreamedConversationView, ConversationView, ContextStream and everything drawn inside
//! a timeline.
//!
//! # Public views
//!
//! ```text
//! conversation::streamed_conversation_view(app: &mut App, props: ConversationViewProps) -> E   // the live conversation
//! conversation::conversation_view(app: &mut App, props: ConversationViewProps) -> E            // any list of contexts: a template
//! conversation::transcript_view(app: &mut App, args: TranscriptArgs<'_>, tools, on_open_subagent) -> E   // tasks' `TranscriptView`
//! handlers::timeline_props(app: &mut App, conversation_id: &str, tools, enabled_tools) -> ConversationViewProps   // App.tsx's handlers
//! handlers::timeline_dialogs(app: &mut App) -> Option<E>                                       // a child of the window's root
//! handlers::timeline_notice_view(app: &mut App, conversation_id: &str) -> Option<E>            // floats over the composer's top edge
//! plan_page::plan_pane(key, plan: Option<&ConversationPlan>, single_dollar_math: bool, dock: Option<E>) -> E
//! ```
//!
//! `ConversationViewProps` is upstream's `ConversationViewProps` and `StreamedConversationViewProps` as one struct; slots are
//! `Rc<dyn Fn(&mut App) -> E>` (`composer`, `empty_state`, `before_timeline`, `timeline_placeholder`). An absent handler removes its
//! affordance, as an absent prop did.

pub mod cards;
pub mod conversation;
pub mod handlers;
pub mod rewind;
pub mod clipboard;
pub mod drag;
pub mod frame;
pub mod item;
pub mod nodes;
pub mod props;
pub mod rows;
pub mod stream;
pub mod kit;
pub mod orchestration;
pub mod tool_view;
pub mod tool_detail;
pub mod cat_art;
pub mod image_strip;
pub mod inline_question_editor;
pub mod inline_text_editor;
pub mod inline_tool_editor;
pub mod plan_page;
pub mod question_card;
pub mod reveal;
pub mod rolling_number;
pub mod turn_changes;
pub mod waiting;
pub mod workflow_detail;

pub use conversation::{conversation_view, streamed_conversation_view, transcript_view};
pub use handlers::{timeline_dialogs, timeline_notice_view, timeline_props};
pub use props::{ConversationViewProps, Slot, TimelineEditorState, TimelineQuestionEditorState};
