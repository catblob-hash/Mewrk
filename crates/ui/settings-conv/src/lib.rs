//! Conversation settings: upstream's `ConversationSettings.tsx`, its pages and its editors. The complete external configuration and runtime settings of a conversation.
//!
//! Public views, for the shell to wire in. Each runs every frame and returns an element.
//!
//! ```text
//! // The whole pane for the active conversation or draft, with upstream's `App` wiring: the client's document, drafts, presets and templates, the tools narrowed to the
//! // shells the conversation's machines have, and `App`'s dialog before a preset's queue replaces a timeline. `None` while nothing is active.
//! pub fn conversation_settings_pane(app: &mut App) -> Option<E>
//!
//! // The same with every prop upstream's `ConversationSettings` carried, for a surface that wires its own handlers.
//! pub fn conversation_settings(app: &mut App, props: ConversationSettingsProps) -> E
//!
//! // That dialog alone, for wherever else `apply_preset_to_active_conversation` can be reached from. The pane above already draws it.
//! pub fn template_switch_prompt(app: &mut App) -> Option<E>
//!
//! // The pages on their own, each taking upstream's props as a struct.
//! impl ToolGroups { pub fn view(self) -> E }                  // ToolSelectionGroups
//! impl AdvancedPage { pub fn view(self) -> E }                // AdvancedToolsPage
//! impl WebSection { pub fn view(self) -> E }                  // WebSearchBehaviorSettings
//! impl CapabilityPage { pub fn view(self, app: &mut App) -> E } // CapabilitySelectionPage
//! impl CatalogRow { pub fn view(self) -> E }                  // CatalogRow, CatalogToggleRow
//! impl RolesPage { pub fn view(self, app: &mut App) -> E }    // AgentRolesPage
//! impl PresetsPage { pub fn view(self, app: &mut App) -> E }  // ConversationPresetsPage
//! impl RoleEditor { pub fn view(self, app: &mut App) -> E }   // AgentRoleEditor
//! impl CapabilityEditor { pub fn view(self, app: &mut App) -> E } // creating and editing a skill, an MCP server or a hook
//! impl TemplateEditor { pub fn view(self, app: &mut App) -> E } // ConversationTemplateEditor
//! ```
//!
//! `ops` holds `Handlers::live()` and the capability operations. The shell must have called `mewrk_client::doc::init`, and sets `ops::live(app).on_remote_reveal` where it has a Files pane to open a remote folder in.

pub mod advanced;
pub mod capability_editor;
pub mod catalog;
pub mod lock;
pub mod logic;
pub mod ops;
pub mod pane;
pub mod roles;
pub mod template_editor;
pub mod tools;
pub mod web;
pub mod widgets;

pub use advanced::{AdvancedPage, BackendLock, WebSection, WebSubject};
pub use capability_editor::{editable_capability_page, CapabilityEditor};
pub use catalog::{CapabilityPage, CatalogKind, CatalogRow};
pub use logic::{capability_workspaces, CapabilityWorkspace};
pub use ops::Handlers;
pub use pane::{conversation_settings, conversation_settings_pane, template_switch_prompt, ConversationSettingsProps, Mode, Page};
pub use roles::{PresetsPage, RoleEditor, RolesPage};
pub use template_editor::TemplateEditor;
pub use tools::ToolGroups;
