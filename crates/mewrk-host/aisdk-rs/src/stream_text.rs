//! `streamText` for the one way the sidecar calls it: tools without `execute`, no retries, a request-local
//! step loop only for provider-executed continuation.
//!
//! The result is pulled part by part, with the same part vocabulary `fullStream` has, so `step.rs` reads as
//! `main.ts` does.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::AiError;
use crate::http::Cancel;
use crate::jobj;
use crate::json::Json;
use crate::model::{CallOptions, DynModel, ModelStream, ModelUsage, ProviderTool, StreamPart};
use crate::prompt::{convert_prompt, standardize_prompt};

/// A tool as `streamText` takes it, in declaration order.
#[derive(Clone, Debug)]
pub enum ToolEntry {
    Function {
        name: String,
        description: Json,
        input_schema: Json,
        strict: Option<bool>,
        provider_options: Option<Json>,
    },
    Provider {
        name: String,
        tool: ProviderTool,
    },
}

impl ToolEntry {
    pub fn name(&self) -> &str {
        match self {
            ToolEntry::Function { name, .. } | ToolEntry::Provider { name, .. } => name,
        }
    }

    fn to_model_tool(&self) -> Json {
        match self {
            ToolEntry::Function { name, description, input_schema, strict, provider_options } => jobj! {
                "type": "function",
                "name": name.as_str(),
                "inputSchema": input_schema.clone(),
                "description": description.clone(),
                "providerOptions"?: provider_options.clone(),
                "strict"?: *strict,
            },
            ToolEntry::Provider { name, tool } => jobj! {
                "type": "provider",
                "name": name.as_str(),
                "id": tool.id,
                "args": tool.args.clone(),
            },
        }
    }
}

/// `LanguageModelUsage`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
}

fn add_counts(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    if a.is_none() && b.is_none() { None } else { Some(a.unwrap_or(0) + b.unwrap_or(0)) }
}

impl Usage {
    fn from_model(usage: &ModelUsage) -> Self {
        Self {
            input_tokens: usage.input_total,
            output_tokens: usage.output_total,
            total_tokens: add_counts(usage.input_total, usage.output_total),
            reasoning_tokens: usage.output_reasoning,
            cache_read_tokens: usage.input_cache_read,
        }
    }

    fn add(&self, other: &Self) -> Self {
        Self {
            input_tokens: add_counts(self.input_tokens, other.input_tokens),
            output_tokens: add_counts(self.output_tokens, other.output_tokens),
            total_tokens: add_counts(self.total_tokens, other.total_tokens),
            reasoning_tokens: add_counts(self.reasoning_tokens, other.reasoning_tokens),
            cache_read_tokens: add_counts(self.cache_read_tokens, other.cache_read_tokens),
        }
    }
}

/// A parsed tool call as `step.toolCalls` holds it.
#[derive(Clone, Debug)]
pub struct ToolCallPart {
    pub tool_call_id: String,
    pub tool_name: String,
    pub input: Json,
    pub provider_executed: Option<bool>,
    pub invalid: bool,
    pub provider_metadata: Option<Json>,
}

/// `TextStreamPart`, for the parts `main.ts` reads.
#[derive(Clone, Debug)]
pub enum TextStreamPart {
    StartStep,
    TextStart { id: String, provider_metadata: Option<Json> },
    TextDelta { id: String, text: String },
    TextEnd { id: String, provider_metadata: Option<Json> },
    ReasoningStart { id: String, provider_metadata: Option<Json> },
    ReasoningDelta { id: String, text: String, provider_metadata: Option<Json> },
    ReasoningEnd { id: String },
    ToolInputStart { id: String, tool_name: String, provider_executed: Option<bool> },
    ToolCall(ToolCallPart),
    ToolResult { tool_call_id: String, tool_name: String, output: Json, provider_executed: bool },
    ToolError { tool_name: String, error: Json, provider_executed: bool },
    Source(Json),
    FinishStep { usage: Usage },
    Finish { usage: Usage, finish_reason: String, raw_finish_reason: Option<String> },
    Error(AiError),
}

/// What one finished step leaves behind for the loop's conditions and the final result.
pub struct StepRecord {
    pub tool_calls: Vec<ToolCallPart>,
    pub response_messages: Vec<Json>,
    pub response_model_id: String,
}

/// `prepareStep`'s return: what changes for the next step.
#[derive(Default)]
pub struct PreparedStep {
    pub active_tools: Option<Vec<String>>,
    pub tool_choice: Option<Json>,
}

/// `prepareStep` and `stopWhen`.
pub trait StepControl: Send {
    fn prepare_step(&mut self, steps: &[StepRecord], tools: &mut Vec<ToolEntry>) -> PreparedStep;
    fn stop_when(&mut self, steps: &[StepRecord]) -> bool;
}

pub struct StreamTextArgs {
    pub model: DynModel,
    pub system: Option<String>,
    pub messages: Vec<Json>,
    pub allow_system_in_messages: bool,
    pub tools: Vec<ToolEntry>,
    pub max_output_tokens: Option<u64>,
    pub reasoning: Option<String>,
    pub provider_options: Option<Json>,
    pub cancel: Cancel,
    pub control: Box<dyn StepControl>,
}

enum ContentPart {
    Text { text: String, provider_metadata: Option<Json> },
    Reasoning { text: String, provider_metadata: Option<Json> },
    File { base64: Json, media_type: String, provider_metadata: Option<Json> },
    ReasoningFile { base64: Json, media_type: String, provider_metadata: Option<Json> },
    Custom { kind: String, provider_metadata: Option<Json> },
    ToolApprovalRequest { approval_id: String, tool_call_id: String },
    ToolCall(ToolCallPart),
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        output: Json,
        provider_executed: bool,
        provider_metadata: Option<Json>,
    },
    ToolError {
        tool_call_id: String,
        tool_name: String,
        error: Json,
        provider_executed: bool,
        provider_metadata: Option<Json>,
    },
}

/// What is being read: a step's stream, or nothing between steps.
enum Phase {
    /// The next step has to be set up.
    Starting,
    Streaming(Box<ModelStream>),
    Closed,
}

pub struct StreamText {
    args: StreamTextArgs,
    phase: Phase,
    queue: VecDeque<TextStreamPart>,
    steps: Vec<StepRecord>,
    step_messages: Vec<Json>,
    active_tools_for_step: Option<Vec<String>>,
    total_usage: Usage,
    pending_deferred: HashSet<String>,
    used_text_ids: HashSet<String>,
    used_reasoning_ids: HashSet<String>,
    step: StepState,
}

#[derive(Default)]
struct StepState {
    content: Vec<ContentPart>,
    active_text: HashMap<String, usize>,
    active_reasoning: HashMap<String, usize>,
    text_ids: HashMap<String, String>,
    reasoning_ids: HashMap<String, String>,
    tool_inputs: HashMap<String, Json>,
    tool_calls: Vec<ToolCallPart>,
    /// Non-preliminary outputs (`stepToolOutputs`): their ids and whether the provider ran them.
    tool_outputs: Vec<(Option<String>, bool)>,
    started: bool,
    terminal: bool,
    output_seen: bool,
    finish_reason: Option<String>,
    raw_finish_reason: Option<String>,
    usage: Option<Usage>,
    response_model_id: Option<String>,
}

fn reserve_id(used: &mut HashSet<String>, id: &str) -> String {
    if used.insert(id.to_owned()) {
        return id.to_owned();
    }
    let generated = format!("aitxt-{}", crate::api::random_id(24));
    let mut unique = generated.clone();
    let mut suffix = 0;
    while used.contains(&unique) {
        suffix += 1;
        unique = format!("{generated}-{suffix}");
    }
    used.insert(unique.clone());
    unique
}

/// `secureJsonParse`: `JSON.parse`, refusing `__proto__` and `constructor.prototype` anywhere in the value.
pub fn secure_json_parse(text: &str) -> Result<Json, String> {
    let value = Json::parse(text)?;
    fn walk(value: &Json) -> Result<(), String> {
        match value {
            Json::Obj(object) => {
                if object.contains_key("__proto__") {
                    return Err("Object contains forbidden prototype property".to_owned());
                }
                if object.get("constructor").is_some_and(|c| c.is_obj() && c.get("prototype").is_some()) {
                    return Err("Object contains forbidden prototype property".to_owned());
                }
                object.iter().try_for_each(|(_, v)| walk(v))
            }
            Json::Arr(items) => items.iter().try_for_each(walk),
            _ => Ok(()),
        }
    }
    walk(&value)?;
    Ok(value)
}

impl StreamText {
    pub fn new(args: StreamTextArgs) -> Self {
        let step_messages = args.messages.clone();
        Self {
            args,
            phase: Phase::Starting,
            queue: VecDeque::new(),
            steps: Vec::new(),
            step_messages,
            active_tools_for_step: None,
            total_usage: Usage::default(),
            pending_deferred: HashSet::new(),
            used_text_ids: HashSet::new(),
            used_reasoning_ids: HashSet::new(),
            step: StepState::default(),
        }
    }

    /// `(await result.response)`: the final step's model and messages.
    pub fn response(&self) -> Option<&StepRecord> {
        self.steps.last()
    }

    pub async fn next(&mut self) -> Option<TextStreamPart> {
        loop {
            if let Some(part) = self.queue.pop_front() {
                return Some(part);
            }
            match &mut self.phase {
                Phase::Closed => return None,
                Phase::Starting => {
                    if let Err(error) = self.start_step().await {
                        self.queue.push_back(TextStreamPart::Error(error));
                        self.phase = Phase::Closed;
                    }
                }
                Phase::Streaming(stream) => {
                    let part = stream.next().await;
                    match part {
                        Some(part) => self.on_model_part(part),
                        None => self.on_step_stream_end(),
                    }
                }
            }
        }
    }

    async fn start_step(&mut self) -> Result<(), AiError> {
        if self.args.cancel.is_cancelled() {
            return Err(AiError::Abort);
        }
        let prepared = self.args.control.prepare_step(&self.steps, &mut self.args.tools);
        let tools = match &prepared.active_tools {
            Some(active) => self.args.tools.iter().filter(|t| active.iter().any(|name| name == t.name())).cloned().collect(),
            None => self.args.tools.clone(),
        };
        self.active_tools_for_step = prepared.active_tools;
        let tool_choice = prepared.tool_choice.unwrap_or_else(|| jobj! { "type": "auto" });

        standardize_prompt(&self.step_messages, self.args.allow_system_in_messages)?;
        let provider = self.args.model.provider().split('.').next().unwrap_or("").to_owned();
        let prompt = convert_prompt(
            self.args.system.as_deref(),
            &self.step_messages,
            &self.args.model.supported_urls(),
            &provider,
            &self.args.cancel,
        )
        .await?;
        let model_tools: Vec<Json> = tools.iter().map(ToolEntry::to_model_tool).collect();
        let options = CallOptions {
            prompt,
            tools: (!model_tools.is_empty()).then_some(model_tools),
            tool_choice,
            max_output_tokens: self.args.max_output_tokens,
            reasoning: self.args.reasoning.clone(),
            provider_options: self.args.provider_options.clone(),
            cancel: self.args.cancel.clone(),
        };
        let model = self.args.model.clone();
        let stream = model.do_stream(options).await?;
        self.step = StepState::default();
        self.phase = Phase::Streaming(Box::new(stream));
        Ok(())
    }

    fn start_once(&mut self) {
        if !self.step.started {
            self.step.started = true;
            self.queue.push_back(TextStreamPart::StartStep);
        }
    }

    fn fail(&mut self, error: AiError) {
        self.step.terminal = true;
        self.step.finish_reason = Some("error".to_owned());
        self.queue.push_back(TextStreamPart::Error(error));
    }

    fn on_model_part(&mut self, part: StreamPart) {
        if matches!(part, StreamPart::StreamStart { .. }) {
            return;
        }
        self.start_once();
        match part {
            StreamPart::StreamStart { .. } => {}
            StreamPart::ResponseMetadata { model_id, .. } => {
                if model_id.is_some() {
                    self.step.response_model_id = model_id;
                }
            }
            StreamPart::TextStart { id, provider_metadata } => {
                let unique = reserve_id(&mut self.used_text_ids, &id);
                self.step.text_ids.insert(id, unique.clone());
                self.step.content.push(ContentPart::Text { text: String::new(), provider_metadata: provider_metadata.clone() });
                self.step.active_text.insert(unique.clone(), self.step.content.len() - 1);
                self.queue.push_back(TextStreamPart::TextStart { id: unique, provider_metadata });
            }
            StreamPart::TextDelta { id, delta, provider_metadata } => {
                if !delta.is_empty() {
                    self.step.output_seen = true;
                }
                if delta.is_empty() && provider_metadata.is_none() {
                    return;
                }
                let unique = self.step.text_ids.get(&id).cloned().unwrap_or(id);
                self.queue.push_back(TextStreamPart::TextDelta { id: unique.clone(), text: delta.clone() });
                match self.step.active_text.get(&unique).copied() {
                    Some(at) => {
                        if let ContentPart::Text { text, provider_metadata: slot } = &mut self.step.content[at] {
                            text.push_str(&delta);
                            if provider_metadata.is_some() {
                                *slot = provider_metadata;
                            }
                        }
                    }
                    None => self.queue.push_back(TextStreamPart::Error(AiError::other(format!("text part {unique} not found")))),
                }
            }
            StreamPart::TextEnd { id, provider_metadata } => {
                let unique = self.step.text_ids.remove(&id).unwrap_or(id);
                self.queue.push_back(TextStreamPart::TextEnd { id: unique.clone(), provider_metadata: provider_metadata.clone() });
                match self.step.active_text.remove(&unique) {
                    Some(at) => {
                        if let (ContentPart::Text { provider_metadata: slot, .. }, true) = (&mut self.step.content[at], provider_metadata.is_some()) {
                            *slot = provider_metadata;
                        }
                    }
                    None => self.queue.push_back(TextStreamPart::Error(AiError::other(format!("text part {unique} not found")))),
                }
            }
            StreamPart::ReasoningStart { id, provider_metadata } => {
                let unique = reserve_id(&mut self.used_reasoning_ids, &id);
                self.step.reasoning_ids.insert(id, unique.clone());
                self.step.content.push(ContentPart::Reasoning { text: String::new(), provider_metadata: provider_metadata.clone() });
                self.step.active_reasoning.insert(unique.clone(), self.step.content.len() - 1);
                self.queue.push_back(TextStreamPart::ReasoningStart { id: unique, provider_metadata });
            }
            StreamPart::ReasoningDelta { id, delta, provider_metadata } => {
                if !delta.is_empty() {
                    self.step.output_seen = true;
                }
                let unique = self.step.reasoning_ids.get(&id).cloned().unwrap_or(id);
                self.queue.push_back(TextStreamPart::ReasoningDelta { id: unique.clone(), text: delta.clone(), provider_metadata: provider_metadata.clone() });
                match self.step.active_reasoning.get(&unique).copied() {
                    Some(at) => {
                        if let ContentPart::Reasoning { text, provider_metadata: slot } = &mut self.step.content[at] {
                            text.push_str(&delta);
                            if provider_metadata.is_some() {
                                *slot = provider_metadata;
                            }
                        }
                    }
                    None => self.queue.push_back(TextStreamPart::Error(AiError::other(format!("reasoning part {unique} not found")))),
                }
            }
            StreamPart::ReasoningEnd { id, provider_metadata } => {
                let unique = self.step.reasoning_ids.remove(&id).unwrap_or(id);
                self.queue.push_back(TextStreamPart::ReasoningEnd { id: unique.clone() });
                match self.step.active_reasoning.remove(&unique) {
                    Some(at) => {
                        if let (ContentPart::Reasoning { provider_metadata: slot, .. }, true) = (&mut self.step.content[at], provider_metadata.is_some()) {
                            *slot = provider_metadata;
                        }
                    }
                    None => self.queue.push_back(TextStreamPart::Error(AiError::other(format!("reasoning part {unique} not found")))),
                }
            }
            StreamPart::ToolInputStart { id, tool_name, provider_executed, .. } => {
                self.queue.push_back(TextStreamPart::ToolInputStart { id, tool_name, provider_executed });
            }
            StreamPart::ToolInputDelta { delta, .. } => {
                if !delta.is_empty() {
                    self.step.output_seen = true;
                }
            }
            StreamPart::ToolInputEnd { .. } => {}
            StreamPart::ToolCall { tool_call_id, tool_name, input, provider_executed, dynamic, provider_metadata } => {
                self.step.output_seen = true;
                self.on_tool_call(tool_call_id, tool_name, input, provider_executed, dynamic, provider_metadata);
            }
            StreamPart::ToolResult { tool_call_id, tool_name, result, is_error, provider_metadata, .. } => {
                self.step.tool_outputs.push((Some(tool_call_id.clone()), true));
                if is_error == Some(true) {
                    self.queue.push_back(TextStreamPart::ToolError { tool_name: tool_name.clone(), error: result.clone(), provider_executed: true });
                    self.step.content.push(ContentPart::ToolError { tool_call_id, tool_name, error: result, provider_executed: true, provider_metadata });
                } else {
                    self.queue.push_back(TextStreamPart::ToolResult { tool_call_id: tool_call_id.clone(), tool_name: tool_name.clone(), output: result.clone(), provider_executed: true });
                    self.step.content.push(ContentPart::ToolResult { tool_call_id, tool_name, output: result, provider_executed: true, provider_metadata });
                }
            }
            StreamPart::Source(source) => {
                self.queue.push_back(TextStreamPart::Source(source));
            }
            StreamPart::Custom { kind, provider_metadata } => {
                self.step.content.push(ContentPart::Custom { kind, provider_metadata });
            }
            StreamPart::ToolApprovalRequest { approval_id, tool_call_id } => {
                self.step.content.push(ContentPart::ToolApprovalRequest { approval_id, tool_call_id });
            }
            StreamPart::File { media_type, data, provider_metadata } => {
                self.step.output_seen = true;
                let base64 = data.get("data").or_else(|| data.get("url")).cloned().unwrap_or_default();
                self.step.content.push(ContentPart::File { base64, media_type, provider_metadata });
            }
            StreamPart::ReasoningFile { media_type, data, provider_metadata } => {
                self.step.output_seen = true;
                let base64 = data.get("data").or_else(|| data.get("url")).cloned().unwrap_or_default();
                self.step.content.push(ContentPart::ReasoningFile { base64, media_type, provider_metadata });
            }
            StreamPart::Finish { finish_reason, usage, .. } => {
                self.step.terminal = true;
                self.step.usage = Some(Usage::from_model(&usage));
                self.step.finish_reason = Some(finish_reason.unified);
                self.step.raw_finish_reason = finish_reason.raw;
            }
            StreamPart::Error(error) => {
                self.fail(error);
            }
        }
    }

    /// `parseToolCall`, and the invalid-call handling after it.
    fn on_tool_call(
        &mut self,
        tool_call_id: String,
        tool_name: String,
        input: String,
        provider_executed: Option<bool>,
        dynamic: Option<bool>,
        provider_metadata: Option<Json>,
    ) {
        let known = self.step_tools_contain(&tool_name);
        let dynamic_provider = provider_executed == Some(true) && dynamic == Some(true);
        let parsed = if !known && !dynamic_provider {
            let available = self.step_tool_names();
            let list = if available.is_empty() {
                "No tools are available.".to_owned()
            } else {
                format!("Available tools: {}.", available.join(", "))
            };
            Err(format!("AI_NoSuchToolError: Model tried to call unavailable tool '{tool_name}'. {list}"))
        } else if input.trim().is_empty() {
            Ok(Json::obj())
        } else {
            secure_json_parse(&input).map_err(|cause| {
                format!(
                    "AI_InvalidToolInputError: Invalid input for tool {tool_name}: AI_JSONParseError: JSON parsing failed: Text: {input}.\nError message: SyntaxError: {cause}"
                )
            })
        };
        let (value, invalid, error) = match parsed {
            Ok(value) => (value, false, None),
            Err(message) => {
                let value = secure_json_parse(&input).unwrap_or_else(|_| Json::from(input.as_str()));
                (value, true, Some(message))
            }
        };
        let call = ToolCallPart { tool_call_id: tool_call_id.clone(), tool_name: tool_name.clone(), input: value.clone(), provider_executed, invalid, provider_metadata };
        self.step.tool_inputs.insert(tool_call_id.clone(), value.clone());
        self.queue.push_back(TextStreamPart::ToolCall(call.clone()));
        self.step.content.push(ContentPart::ToolCall(call.clone()));
        self.step.tool_calls.push(call);
        if let Some(message) = error {
            if provider_executed != Some(true) {
                let error = Json::from(message);
                self.step.tool_outputs.push((Some(tool_call_id.clone()), false));
                self.queue.push_back(TextStreamPart::ToolError { tool_name: tool_name.clone(), error: error.clone(), provider_executed: false });
                self.step.content.push(ContentPart::ToolError { tool_call_id, tool_name, error, provider_executed: false, provider_metadata: None });
            }
        }
    }

    fn step_tools_contain(&self, name: &str) -> bool {
        self.args.tools.iter().any(|tool| tool.name() == name)
            && self.active_tools_for_step.as_ref().is_none_or(|active| active.iter().any(|n| n == name))
    }

    fn step_tool_names(&self) -> Vec<&str> {
        self.args
            .tools
            .iter()
            .map(ToolEntry::name)
            .filter(|name| self.active_tools_for_step.as_ref().is_none_or(|active| active.iter().any(|n| n == name)))
            .collect()
    }

    /// The step's stream ended: its `flush`.
    fn on_step_stream_end(&mut self) {
        if !self.step.terminal && !self.step.output_seen {
            self.queue.push_back(TextStreamPart::Error(AiError::other(
                "No output generated. The model stream ended without a finish chunk.",
            )));
            self.phase = Phase::Closed;
            return;
        }
        let step_usage = self.step.usage.clone().unwrap_or_default();
        self.queue.push_back(TextStreamPart::FinishStep { usage: step_usage.clone() });
        let combined = self.total_usage.add(&step_usage);
        self.total_usage = combined.clone();

        let step = std::mem::take(&mut self.step);
        let response_messages = to_response_messages(&step);
        let record = StepRecord {
            tool_calls: step.tool_calls.clone(),
            response_messages: response_messages.clone(),
            response_model_id: step.response_model_id.clone().unwrap_or_else(|| self.args.model.model_id().to_owned()),
        };
        self.steps.push(record);
        self.step_messages.extend(response_messages);

        let client_calls = step.tool_calls.iter().filter(|c| c.provider_executed != Some(true)).count();
        let client_outputs = step.tool_outputs.iter().filter(|(_, provider)| !provider).count();
        // Provider-executed calls whose result may come in a later step.
        for call in &step.tool_calls {
            if call.provider_executed != Some(true) {
                continue;
            }
            let deferred = self.args.tools.iter().any(|tool| match tool {
                ToolEntry::Provider { name, tool } => {
                    *name == call.tool_name
                        && tool.supports_deferred_results
                        && self.active_tools_for_step.as_ref().is_none_or(|active| active.iter().any(|n| n == name))
                }
                ToolEntry::Function { .. } => false,
            });
            if deferred && !step.tool_outputs.iter().any(|(id, _)| id.as_deref() == Some(call.tool_call_id.as_str())) {
                self.pending_deferred.insert(call.tool_call_id.clone());
            }
        }
        for (id, _) in &step.tool_outputs {
            if let Some(id) = id {
                self.pending_deferred.remove(id);
            }
        }

        let finish_reason = step.finish_reason.clone().unwrap_or_else(|| "other".to_owned());
        let stopped = self.args.control.stop_when(&self.steps);
        if client_calls == client_outputs && (client_calls > 0 || !self.pending_deferred.is_empty()) && !stopped {
            self.phase = Phase::Starting;
        } else {
            self.queue.push_back(TextStreamPart::Finish {
                usage: combined,
                finish_reason,
                raw_finish_reason: step.raw_finish_reason,
            });
            self.phase = Phase::Closed;
        }
    }
}

fn tool_output_json(output: &Json) -> Json {
    match output {
        Json::Str(text) => jobj! { "type": "text", "value": text.as_str() },
        other => jobj! { "type": "json", "value": other.clone() },
    }
}

/// `toResponseMessages`.
fn to_response_messages(step: &StepState) -> Vec<Json> {
    let mut messages = Vec::new();
    let mut order: HashMap<&str, usize> = HashMap::new();
    let mut content: Vec<Json> = Vec::new();
    for part in &step.content {
        match part {
            ContentPart::Text { text, provider_metadata } => {
                if text.is_empty() {
                    continue;
                }
                content.push(jobj! { "type": "text", "text": text.as_str(), "providerOptions"?: provider_metadata.clone() });
            }
            ContentPart::Reasoning { text, provider_metadata } => {
                content.push(jobj! { "type": "reasoning", "text": text.as_str(), "providerOptions"?: provider_metadata.clone() });
            }
            ContentPart::File { base64, media_type, provider_metadata } => {
                content.push(jobj! { "type": "file", "data": base64.clone(), "mediaType": media_type.as_str(), "providerOptions"?: provider_metadata.clone() });
            }
            ContentPart::ReasoningFile { base64, media_type, provider_metadata } => {
                content.push(jobj! { "type": "reasoning-file", "data": base64.clone(), "mediaType": media_type.as_str(), "providerOptions"?: provider_metadata.clone() });
            }
            ContentPart::Custom { kind, provider_metadata } => {
                content.push(jobj! { "type": "custom", "kind": kind.as_str(), "providerOptions"?: provider_metadata.clone() });
            }
            ContentPart::ToolApprovalRequest { approval_id, tool_call_id } => {
                content.push(jobj! { "type": "tool-approval-request", "approvalId": approval_id.as_str(), "toolCallId": tool_call_id.as_str() });
            }
            ContentPart::ToolCall(call) => {
                let next = order.len();
                order.entry(call.tool_call_id.as_str()).or_insert(next);
                let input = if call.invalid && !matches!(call.input, Json::Obj(_) | Json::Arr(_) | Json::Null) { Json::obj() } else { call.input.clone() };
                content.push(jobj! {
                    "type": "tool-call",
                    "toolCallId": call.tool_call_id.as_str(),
                    "toolName": call.tool_name.as_str(),
                    "input": input,
                    "providerExecuted"?: call.provider_executed,
                    "providerOptions"?: call.provider_metadata.clone(),
                });
            }
            ContentPart::ToolResult { tool_call_id, tool_name, output, provider_executed, provider_metadata } => {
                if *provider_executed {
                    content.push(jobj! {
                        "type": "tool-result",
                        "toolCallId": tool_call_id.as_str(),
                        "toolName": tool_name.as_str(),
                        "output": tool_output_json(output),
                        "providerOptions"?: provider_metadata.clone(),
                    });
                }
            }
            ContentPart::ToolError { tool_call_id, tool_name, error, provider_executed, provider_metadata } => {
                if *provider_executed {
                    content.push(jobj! {
                        "type": "tool-result",
                        "toolCallId": tool_call_id.as_str(),
                        "toolName": tool_name.as_str(),
                        "output": jobj! { "type": "error-json", "value": error.clone() },
                        "providerOptions"?: provider_metadata.clone(),
                    });
                }
            }
        }
    }
    if !content.is_empty() {
        messages.push(jobj! { "role": "assistant", "content": content });
    }

    // Client results, sorted by their call's order: only invalid calls have one here.
    let mut results: Vec<(Option<usize>, usize, Json)> = Vec::new();
    for part in &step.content {
        match part {
            ContentPart::ToolResult { tool_call_id, tool_name, output, provider_executed: false, provider_metadata } => {
                let at = results.len();
                results.push((order.get(tool_call_id.as_str()).copied(), at, jobj! {
                    "type": "tool-result",
                    "toolCallId": tool_call_id.as_str(),
                    "toolName": tool_name.as_str(),
                    "output": tool_output_json(output),
                    "providerOptions"?: provider_metadata.clone(),
                }));
            }
            ContentPart::ToolError { tool_call_id, tool_name, error, provider_executed: false, provider_metadata } => {
                let at = results.len();
                let text = error.as_str().map_or_else(|| error.to_text(), str::to_owned);
                results.push((order.get(tool_call_id.as_str()).copied(), at, jobj! {
                    "type": "tool-result",
                    "toolCallId": tool_call_id.as_str(),
                    "toolName": tool_name.as_str(),
                    "output": jobj! { "type": "error-text", "value": text },
                    "providerOptions"?: provider_metadata.clone(),
                }));
            }
            _ => {}
        }
    }
    if !results.is_empty() {
        results.sort_by_key(|(order, index, _)| (order.is_none(), order.unwrap_or(0), *index));
        messages.push(jobj! { "role": "tool", "content": results.into_iter().map(|(_, _, part)| part).collect::<Vec<_>>() });
    }
    messages
}

/// A stream chunk the provider did not recognize as its own error shape, normalized the way
/// `normalizeStreamProviderError` reads a payload: `response.error`, then `error`, then the object itself.
pub fn stream_error_from_payload(payload: &Json) -> AiError {
    let details = payload
        .path(&["response", "error"])
        .filter(|v| v.is_obj())
        .or_else(|| payload.get("error").filter(|v| v.is_obj()))
        .unwrap_or(payload);
    let Some(message) = details.str_at("message") else {
        return AiError::other(payload.to_text());
    };
    let status_of = |value: Option<&Json>| -> Option<u16> {
        let value = value?;
        let number = match value {
            Json::Str(text) if text.len() == 3 && text.bytes().all(|b| b.is_ascii_digit()) => text.parse::<u64>().ok(),
            other => other.as_u64(),
        }?;
        (400..=599).contains(&number).then_some(number as u16)
    };
    let explicit = ["statusCode", "status_code", "status", "code"].iter().find_map(|key| {
        status_of(details.get(key)).or_else(|| status_of(payload.get(key)))
    });
    let inferred = match message.trim().to_ascii_lowercase().as_str() {
        "overloaded" | "overloaded error" | "model overloaded" | "service unavailable" => Some((503, true)),
        "internal server error" => Some((500, true)),
        _ => None,
    };
    let status = explicit.or(inferred.map(|(s, _)| s));
    let explicit_retry = ["isRetryable", "is_retryable"].iter().find_map(|key| {
        details.get(key).and_then(Json::as_bool).or_else(|| payload.get(key).and_then(Json::as_bool))
    });
    let retryable = explicit_retry
        .or(inferred.map(|(_, r)| r))
        .unwrap_or_else(|| matches!(status, Some(code) if code == 408 || code == 409 || code == 429 || code >= 500));
    AiError::StreamProvider(Box::new(crate::error::StreamProviderError {
        message: message.to_owned(),
        type_: details.str_at("type").or_else(|| payload.str_at("type")).map(str::to_owned),
        code: details.get("code").or_else(|| payload.get("code")).filter(|c| c.is_str() || c.is_num()).cloned(),
        status,
        is_retryable: retryable,
        data: Some(payload.clone()),
    }))
}
