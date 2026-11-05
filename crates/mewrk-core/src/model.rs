//! What a conversation is made of. The timeline is a flat list of items: what the model sees is built from
//! it on every request, so editing or deleting an item really does change the model's context.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub type ItemId = u64;

pub fn new_id() -> String {
    let mut out = String::with_capacity(16);
    for _ in 0..2 {
        out.push_str(&format!("{:08x}", fastrand::u32(..)));
    }
    out
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// A rough token count for display, before the provider reports the real one.
pub fn estimate_tokens(s: &str) -> u32 {
    let mut ascii = 0u32;
    let mut wide = 0u32;
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    ascii / 4 + wide
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    pub mime: String,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Its arguments are still arriving.
    #[default]
    Streaming,
    /// Waiting for the user to approve it.
    Pending,
    Running,
    Done,
    Error,
    Denied,
    Cancelled,
}

impl ToolStatus {
    pub fn finished(self) -> bool {
        matches!(self, ToolStatus::Done | ToolStatus::Error | ToolStatus::Denied | ToolStatus::Cancelled)
    }
}

/// What a tool can say about itself beyond its output, for the timeline's rows and cards.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolMeta {
    /// The file a write or edit touched, as the tool was given it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub added: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub removed: u32,
    /// Unified diff of the change, shown when the row is opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The subagent or background task this call started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolItem {
    pub id: ItemId,
    pub call_id: String,
    pub name: String,
    pub args: String,
    #[serde(default)]
    pub status: ToolStatus,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub took_ms: u64,
    #[serde(default)]
    pub meta: ToolMeta,
    #[serde(default)]
    pub tokens: u32,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

impl Usage {
    /// What the prompt of this request weighed, whichever way the provider splits it up.
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// One entry in the timeline.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Item {
    User {
        id: ItemId,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Attachment>,
        at: u64,
    },
    /// Sent to the model in the user role, but not said by the user: a task finishing, a mode switching.
    Host {
        id: ItemId,
        text: String,
        at: u64,
    },
    Thinking {
        id: ItemId,
        text: String,
        /// The provider's opaque proof of this reasoning. It has to go back unchanged.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        signature: String,
        #[serde(default)]
        tokens: u32,
    },
    Text {
        id: ItemId,
        text: String,
        #[serde(default)]
        tokens: u32,
    },
    Tool(ToolItem),
    /// The end of one model request.
    Turn {
        id: ItemId,
        model: String,
        usage: Usage,
        #[serde(default)]
        finish: String,
        at: u64,
    },
    /// For the user only. The model never sees these.
    Notice {
        id: ItemId,
        level: NoticeLevel,
        text: String,
    },
    /// Everything before this was summarised into `summary`, and requests start here.
    Compaction {
        id: ItemId,
        summary: String,
        at: u64,
    },
}

impl Item {
    pub fn id(&self) -> ItemId {
        match self {
            Item::User { id, .. } | Item::Host { id, .. } | Item::Thinking { id, .. } | Item::Text { id, .. } | Item::Turn { id, .. } | Item::Notice { id, .. } | Item::Compaction { id, .. } => *id,
            Item::Tool(t) => t.id,
        }
    }
}

// ---------------------------------------------------------------- settings

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 3] = [ProviderKind::Anthropic, ProviderKind::OpenAiResponses, ProviderKind::OpenAiChat];

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::OpenAiChat => "OpenAI Chat 兼容",
            ProviderKind::OpenAiResponses => "OpenAI Responses",
            ProviderKind::Anthropic => "Anthropic",
        }
    }

    pub fn default_url(self) -> &'static str {
        match self {
            ProviderKind::OpenAiChat | ProviderKind::OpenAiResponses => "https://api.openai.com/v1",
            ProviderKind::Anthropic => "https://api.anthropic.com/v1",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelProfile {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub max_output: u32,
    #[serde(default)]
    pub vision: bool,
}

impl ModelProfile {
    pub fn label(&self) -> &str {
        if self.name.is_empty() { &self.id } else { &self.name }
    }

    pub fn context_window(&self) -> u64 {
        if self.context_window > 0 { self.context_window } else { 200_000 }
    }

    pub fn max_output(&self) -> u32 {
        if self.max_output > 0 { self.max_output } else { 16_000 }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default)]
    pub models: Vec<ModelProfile>,
    /// The server can continue from a prefilled assistant message, so a dropped stream resumes instead of restarting.
    #[serde(default)]
    pub resume_prefill: bool,
    /// How long a stream may stay silent, in seconds. 0 means the default.
    #[serde(default)]
    pub stall_secs: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Ask before anything that changes the machine.
    #[default]
    Ask,
    /// Never ask.
    FullAccess,
    /// Refuse anything that changes the machine.
    ReadOnly,
}

impl PermissionMode {
    pub const ALL: [PermissionMode; 3] = [PermissionMode::Ask, PermissionMode::FullAccess, PermissionMode::ReadOnly];

    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::Ask => "需要批准",
            PermissionMode::FullAccess => "完全访问",
            PermissionMode::ReadOnly => "只读",
        }
    }
}

/// Reasoning effort as the composer offers it. Providers map it to whatever they call it.
pub const EFFORTS: [&str; 5] = ["off", "low", "medium", "high", "max"];

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConvSettings {
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub permission: PermissionMode,
    #[serde(default)]
    pub plan_mode: bool,
    /// Tools taken away from this conversation.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    /// Added to the system prompt, for this conversation only.
    #[serde(default)]
    pub instructions: String,
    /// More folders the agent works in, after the project's own.
    #[serde(default)]
    pub extra_workspaces: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// `None` is the temporary project, whose conversations get a scratch folder each.
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub collapsed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConvMeta {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub settings: ConvSettings,
    #[serde(default)]
    pub usage: Usage,
    /// The prompt size of the latest request, for the context meter.
    #[serde(default)]
    pub context_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GlobalSettings {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub default_provider: String,
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub default_effort: String,
    #[serde(default)]
    pub default_permission: PermissionMode,
    #[serde(default)]
    pub theme: ThemeChoice,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    #[serde(default = "yes")]
    pub sidebar_open: bool,
    /// Enter sends and Shift+Enter breaks the line; off, the other way round.
    #[serde(default = "yes")]
    pub enter_sends: bool,
    #[serde(default)]
    pub wide_messages: bool,
    #[serde(default)]
    pub global_instructions: String,
}

fn default_sidebar_width() -> f32 {
    264.0
}

fn yes() -> bool {
    true
}

impl Default for GlobalSettings {
    fn default() -> Self {
        serde_json::from_str("{}").expect("every field has a default")
    }
}

/// What lives in `document.json`: everything but the conversations' bodies.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Document {
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub conversations: Vec<ConvMeta>,
    #[serde(default)]
    pub settings: GlobalSettings,
}

impl Document {
    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.settings.providers.iter().find(|p| p.id == id)
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn conversation(&self, id: &str) -> Option<&ConvMeta> {
        self.conversations.iter().find(|c| c.id == id)
    }

    pub fn conversation_mut(&mut self, id: &str) -> Option<&mut ConvMeta> {
        self.conversations.iter_mut().find(|c| c.id == id)
    }
}
