//! Sending a message and following a model run: the port of `lib/sendPipeline.ts`, `lib/modelStream.ts` and the
//! parts of `App.tsx` that start, attach to, stop and retry runs.
//!
//! The public surface is methods on [`App`] (`api.rs`, [`view`], [`timeline_edits`]) plus the plain data they hand out.
//! Nothing in here draws anything.
//!
//! What upstream's effects did when the document or the runs changed is [`App::runs_reconcile`]: adoption, then forks,
//! wakes and queued messages that have become possible. The run code calls it whenever a run or a push event changes
//! something; a document change it did not make itself (a model picked, a provider enabled) needs it called too, and
//! once a frame is cheap enough to be the way.
//!
//! The streamed state is [`App::run_view`]: built at most once per commit, and commits come at 10 per second however
//! fast tokens do, so a view reads it every frame without copying anything.

mod api;
mod bridge;
mod events;
mod pipeline;
mod seams;
mod state;

pub mod attachments;
pub mod capabilities;
pub mod composer;
pub mod context_breakdown;
pub mod context_tokens;
pub mod conversation_queue;
pub mod conversation_turns;
pub mod image_short_ids;
pub mod model_run_controller;
pub mod model_stream;
pub mod run_contexts;
pub mod stream_reveal;
pub mod timeline_edits;
pub mod timeline_history;
pub mod types;
pub mod util;
pub mod view;

#[cfg(test)]
mod tests;

pub use attachments::{AttachmentRejection, AttachmentRejectionReason, ComposerFile};
pub use composer::{ComposerState, ElementPick, PastedText};
pub use context_breakdown::{compute_context_breakdown, ContextBreakdown, ContextSegment, ContextSegmentId};
pub use context_tokens::{estimate_wire_tokens, format_compact_token_count, wire_view, LiveContextTokens, WireView};
pub use conversation_turns::{ConversationTurn, ConversationTurnError, ConversationTurnStatus};
pub use conversation_queue::queue_is_paused;
pub use model_run_controller::{ModelRunController, ModelRunSummary};
pub use model_stream::{LiveContext, ModelRunRetryState, ModelRunState, StreamingHookState, StreamingToolPhase, StreamingToolState, SubagentLiveState, SubagentLiveStatus};
pub use pipeline::ClearErrorScope;
pub use run_contexts::LiveReasoningView;
pub use seams::RunSignals;
pub use state::{EventSink, RunBackend, RunsState};
pub use stream_reveal::{next_stream_reveal_split, reveal_timings, RevealTiming, StreamRevealSplit};
pub use timeline_edits::TimelineNotice;
pub use timeline_history::{TimelineHistory, TimelineHistoryAction, TimelineHistoryEntry, TimelinePatch};
pub use types::*;
pub use view::{ConversationStatus, RenderedContext, RunView};
