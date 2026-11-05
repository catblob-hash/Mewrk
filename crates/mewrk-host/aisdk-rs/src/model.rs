//! The language-model boundary (`LanguageModelV4`): what `streamText` sees of a provider.
//!
//! A provider turns [`CallOptions`] into one HTTP request and its response into [`StreamPart`]s. Prompts and
//! tools stay JSON in the SDK's own shape (`role`, `content`, `providerOptions`; camelCase), because the
//! prompts arrive as JSON from the host and the conversion functions read them field by field.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::error::AiError;
use crate::http::{Body, BoxFuture, Cancel};
use crate::json::Json;
use crate::sse::{FrameDecoder, SseJsonDecoder};

/// `LanguageModelV4CallOptions`, for the fields this crate's single caller sets.
#[derive(Clone)]
pub struct CallOptions {
    /// `LanguageModelV4Prompt`: messages with `content` always in its array form, except `system`.
    pub prompt: Vec<Json>,
    /// `None` when no tool is active. Function tools are `{type:"function", name, inputSchema,
    /// description?, providerOptions?, strict?}`; provider tools `{type:"provider", name, id, args}`.
    pub tools: Option<Vec<Json>>,
    /// `{type:"auto"}`, `{type:"none"}`, `{type:"required"}` or `{type:"tool", toolName}`.
    pub tool_choice: Json,
    pub max_output_tokens: Option<u64>,
    /// The shared reasoning level, when the plan uses it. `None` is the SDK's `undefined`.
    pub reasoning: Option<String>,
    /// `{ [provider]: { ... } }`, or `None`.
    pub provider_options: Option<Json>,
    pub cancel: Cancel,
}

impl CallOptions {
    /// `parseProviderOptions` without the schema: the object under `provider`, when there is one. Unknown
    /// keys are ignored because each provider reads only the keys it knows.
    pub fn options_for(&self, provider: &str) -> Option<&Json> {
        self.provider_options.as_ref()?.get(provider).filter(|v| v.is_obj())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FinishReason {
    /// `stop`, `length`, `content-filter`, `tool-calls`, `error`, `other`.
    pub unified: String,
    pub raw: Option<String>,
}

impl FinishReason {
    pub fn new(unified: &str, raw: Option<String>) -> Self {
        Self { unified: unified.to_owned(), raw }
    }

    /// `{ unified: 'other', raw: undefined }`, where every provider's stream starts.
    pub fn other() -> Self {
        Self::new("other", None)
    }
}

/// `LanguageModelV4Usage`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelUsage {
    pub input_total: Option<u64>,
    pub input_no_cache: Option<u64>,
    pub input_cache_read: Option<u64>,
    pub input_cache_write: Option<u64>,
    pub output_total: Option<u64>,
    pub output_text: Option<u64>,
    pub output_reasoning: Option<u64>,
    pub raw: Option<Json>,
}

/// `LanguageModelV4StreamPart`.
#[derive(Clone, Debug)]
pub enum StreamPart {
    StreamStart { warnings: Vec<Json> },
    ResponseMetadata { id: Option<String>, model_id: Option<String> },
    TextStart { id: String, provider_metadata: Option<Json> },
    TextDelta { id: String, delta: String, provider_metadata: Option<Json> },
    TextEnd { id: String, provider_metadata: Option<Json> },
    ReasoningStart { id: String, provider_metadata: Option<Json> },
    ReasoningDelta { id: String, delta: String, provider_metadata: Option<Json> },
    ReasoningEnd { id: String, provider_metadata: Option<Json> },
    ToolInputStart { id: String, tool_name: String, provider_executed: Option<bool>, dynamic: Option<bool> },
    ToolInputDelta { id: String, delta: String },
    ToolInputEnd { id: String },
    /// `input` is the raw JSON text of the arguments.
    ToolCall {
        tool_call_id: String,
        tool_name: String,
        input: String,
        provider_executed: Option<bool>,
        dynamic: Option<bool>,
        provider_metadata: Option<Json>,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        result: Json,
        is_error: Option<bool>,
        dynamic: Option<bool>,
        provider_metadata: Option<Json>,
    },
    /// `{ sourceType, id, url?, title?, mediaType?, filename?, providerMetadata? }`.
    Source(Json),
    /// A provider-specific content part (`openai.compaction`): opaque to the SDK, replayed from the response messages.
    Custom { kind: String, provider_metadata: Option<Json> },
    /// A provider-executed call waits for the user's approval (MCP on Responses).
    ToolApprovalRequest { approval_id: String, tool_call_id: String },
    /// A generated file: `data` is `{type:"data", data: base64}` or `{type:"url", url}`.
    File { media_type: String, data: Json, provider_metadata: Option<Json> },
    ReasoningFile { media_type: String, data: Json, provider_metadata: Option<Json> },
    Finish { finish_reason: FinishReason, usage: ModelUsage, provider_metadata: Option<Json> },
    Error(AiError),
}

/// Turns decoded chunks into stream parts: the body of a provider's `TransformStream`.
pub trait ChunkTransform: Send {
    /// `start(controller)`.
    fn start(&mut self, _out: &mut Vec<StreamPart>) {}
    /// `transform(chunk, controller)`. A chunk that failed to decode arrives as `Err`.
    fn chunk(&mut self, chunk: Result<Json, AiError>, out: &mut Vec<StreamPart>);
    /// `flush(controller)`.
    fn flush(&mut self, _out: &mut Vec<StreamPart>) {}
}

/// `doStream`'s stream.
pub struct ModelStream {
    body: Body,
    decoder: Box<dyn FrameDecoder>,
    transform: Box<dyn ChunkTransform>,
    queue: VecDeque<StreamPart>,
    ended: bool,
}

impl ModelStream {
    /// A response whose body is SSE JSON events.
    pub fn sse(body: Body, transform: Box<dyn ChunkTransform>) -> Self {
        Self::new(body, Box::<SseJsonDecoder>::default(), transform)
    }

    pub fn new(body: Body, decoder: Box<dyn FrameDecoder>, mut transform: Box<dyn ChunkTransform>) -> Self {
        let mut started = Vec::new();
        transform.start(&mut started);
        Self { body, decoder, transform, queue: started.into(), ended: false }
    }

    /// The next part. A transport failure is the last part: `Error`, then `None`, the same as the SDK's
    /// stream rejecting mid-read.
    pub async fn next(&mut self) -> Option<StreamPart> {
        loop {
            if let Some(part) = self.queue.pop_front() {
                return Some(part);
            }
            if self.ended {
                return None;
            }
            let mut frames = Vec::new();
            match self.body.next_chunk().await {
                Ok(Some(chunk)) => self.decoder.feed(&chunk, &mut frames),
                Ok(None) => {
                    self.ended = true;
                    self.decoder.finish(&mut frames);
                }
                Err(error) => {
                    self.ended = true;
                    self.queue.push_back(StreamPart::Error(error));
                    continue;
                }
            }
            let mut parts = Vec::new();
            for frame in frames {
                self.transform.chunk(frame, &mut parts);
            }
            if self.ended {
                self.transform.flush(&mut parts);
            }
            self.queue.extend(parts);
        }
    }

    /// The parts up to and including the first that is not `stream-start`, kept in the queue. Lets a
    /// provider do what its `tee()` did: look at the first chunk, and turn an error there into a thrown
    /// `APICallError`.
    pub async fn peek_first(&mut self) -> Option<&StreamPart> {
        loop {
            if let Some(at) = self.queue.iter().position(|p| !matches!(p, StreamPart::StreamStart { .. })) {
                return self.queue.get(at);
            }
            if self.ended {
                return None;
            }
            let mut frames = Vec::new();
            match self.body.next_chunk().await {
                Ok(Some(chunk)) => self.decoder.feed(&chunk, &mut frames),
                Ok(None) => {
                    self.ended = true;
                    self.decoder.finish(&mut frames);
                }
                Err(error) => {
                    self.ended = true;
                    self.queue.push_back(StreamPart::Error(error));
                    continue;
                }
            }
            let mut parts = Vec::new();
            for frame in frames {
                self.transform.chunk(frame, &mut parts);
            }
            if self.ended {
                self.transform.flush(&mut parts);
            }
            self.queue.extend(parts);
        }
    }
}

/// Which URLs a model takes as they are (`supportedUrls`), as patterns over the lower-cased URL: the
/// media-type key (`image/*`, `*`) and the regular expression source.
pub type SupportedUrls = Vec<(&'static str, &'static str)>;

/// `LanguageModelV4`.
pub trait LanguageModel: Send + Sync {
    /// `anthropic.messages`, `openai.responses`… The part before the first dot names its provider options
    /// and its legacy file ids.
    fn provider(&self) -> &str;
    fn model_id(&self) -> &str;
    fn supported_urls(&self) -> SupportedUrls {
        Vec::new()
    }
    fn do_stream(&self, options: CallOptions) -> BoxFuture<'_, Result<ModelStream, AiError>>;
}

pub type DynModel = Arc<dyn LanguageModel>;

/// A provider tool the model executes itself: what a `tools.*` factory returns.
#[derive(Clone, Debug)]
pub struct ProviderTool {
    /// `anthropic.web_search_20250305`.
    pub id: &'static str,
    /// The factory's arguments, in the SDK's camelCase.
    pub args: Json,
    /// `supportsDeferredResults`: a result may arrive in a later step, which keeps the step loop going.
    pub supports_deferred_results: bool,
}
