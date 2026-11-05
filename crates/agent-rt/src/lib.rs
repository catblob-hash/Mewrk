//! Streaming agent runtime for OpenAI-compatible servers, built to keep going on bad networks.

pub mod agent;
pub mod error;
pub mod media;
pub mod sse;
pub mod tools;
pub mod transport;
pub mod wire;

pub use agent::{run_agent, AgentConfig, Cancel, Event, RunSummary, Runtime, RuntimeConfig, SessionId, Sink};
pub use error::{FailKind, NetError, RetryPolicy};
pub use media::{MediaInfo, MediaInput, MediaKind};
pub use tools::{Calc, Clock, ReadFile, Tool, ToolRegistry};
pub use transport::{ConnTiming, Transport, TransportConfig};
pub use wire::{Message, Part, Role, Timings, ToolCall, Usage};
