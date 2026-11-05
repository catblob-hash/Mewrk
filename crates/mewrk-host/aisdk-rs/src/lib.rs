//! Mewrk's AI SDK sidecar, in process: one model step per frame, no Node.
//!
//! The Node sidecar (`aisdk-service/`) ran the Vercel AI SDK behind a stdio NDJSON protocol. This crate takes the
//! same host frames and produces the same sidecar frames, and carries the SDK's wire behaviour for every family
//! itself. See [`service::Sidecar`] for the boundary.
//!
//! Layers, top down, as in the TypeScript:
//!
//! - [`service`], [`step`]: `main.ts`.
//! - [`stream_text`], [`prompt`]: the `ai` package's `streamText` and prompt conversion.
//! - [`providers`]: `providers.ts` and each `@ai-sdk/*` language model.
//! - [`dialect`]: the sidecar's `fetch` wrappers per endpoint family.
//! - [`http`], [`sse`], [`api`]: `fetch`, the event-stream parser, `postJsonToApi`.


pub mod json;
pub mod api;
pub mod async_tools;
pub mod claude_agent;
pub mod dialect;
pub mod error;
pub mod http;
pub mod model;
pub mod prompt;
pub mod protocol;
pub mod providers;
pub mod reasoning;
pub mod search;
pub mod service;
pub mod sse;
pub mod step;
pub mod stream_text;
pub mod system_append;
pub mod tool_append;

pub use protocol::{MAX_LINE_BYTES, PROTOCOL_VERSION};
pub use service::Sidecar;
