//! Mewrk's engine: everything except the window.
//!
//! The UI thread and the engine meet at a per-run [`inbox::Inbox`]. The engine pushes events, adjacent text
//! merges, and the UI drains once per frame, so a stream cannot build up a backlog however fast it runs.

pub mod engine;
pub mod inbox;
pub mod model;
pub mod prompt;
pub mod provider;
pub mod secrets;
pub mod store;
pub mod tasks;
pub mod tools;

pub use engine::{Engine, RunHandle, RunSpec};
pub use inbox::{ApprovalDecision, ApprovalRequest, Inbox, Notifier, QuestionRequest, RunEnd, RunEvent, RunStatus};
pub use model::*;
pub use store::Store;
