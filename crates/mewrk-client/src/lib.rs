//! The client side of Mewrk: the state the upstream renderer kept in React and `src/lib`, and its link to the host.
//!
//! Views are written with `fluent::el`: a function from `&mut App` to an `El<App>`. See `PORTING.md` in this crate.

mod app;
pub mod i18n;
pub mod doc;
pub mod memory_pool;
pub mod conversation_bodies;
pub mod conversation_sync;
pub mod conversation_forks;
pub mod conversation_branches;
pub mod conversation_history;
pub mod conversation_tree;
pub mod selection;
pub mod drafts;
pub mod workspaces;
pub mod machine_shells;
pub mod ui_store;
pub mod timers;
pub mod presets;
pub mod tool_defaults;
pub mod tool_lock;
pub mod task_tools;
pub mod model_capabilities;
pub mod agent_roles;
pub mod settings_flags;
pub mod auto_compact;
pub mod appearance;
pub mod side_panes;
pub mod shortcuts;
pub mod file_mention;
pub mod runs;

pub use app::{App, Channel, StartOptions, Task, WindowRequests};
pub use i18n::{t, tf};
pub use mewrk_lib::model;
pub use mewrk_lib::{askpass_main, chromium_preload_framework, chromium_set_accessibility, chromium_subprocess_main};
pub use mewrk_lib::native::{prepare_process, Host, HostConfig, StartError};

/// A remote screen, for a viewer drawn in this process.
pub mod screen {
    pub use mewrk_lib::native::{LiveScreen, PointerPhase, RemoteControlSession, ScreenPlatform, ScreenUpdate, SessionPhase, ViewerCredentials, ViewerEvent, ViewerState, Zeroizing};
}

/// What a view file starts with.
pub mod prelude {
    pub use crate::{model, t, tf, App, Channel, Task};
    pub use fluent::el::*;
    pub use fluent::theme::*;
    pub use fluent::ui::{rect, CursorKind, Icon, Key, KeyPress, Rect, Ui};
    pub use fluent::TextEdit;
    pub use serde_json::{json, Value};
    pub use std::rc::Rc;
    pub use std::sync::Arc;

    pub type E = fluent::el::El<App>;
    pub type MenuItem = fluent::el::MenuRow<App>;
}
