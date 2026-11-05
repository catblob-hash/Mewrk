//! Global settings, upstream's GlobalSettings and its pages.
//!
//! # For the shell
//!
//! ```ignore
//! // Mount it: inside `root::root`, as a child anywhere in the tree. It lifts itself to the dialog layer.
//! pub fn global_settings_window(app: &mut App) -> Option<E>;
//! // What upstream's `openGlobalSettings(view)` was. `SettingsView::from_wire` takes upstream's view ids.
//! pub fn open(app: &mut App, view: SettingsView);
//! pub fn close(app: &mut App);
//! pub fn is_open(app: &mut App) -> bool;
//! // Only the body, for a caller with its own window to put it in.
//! pub fn global_settings_body(app: &mut App, view: SettingsView) -> E;
//!
//! // What Appearance changes, for `root::theme` and the frame driver (`effects.rs` says what each one is):
//! pub fn themed(base: Theme, appearance: &AppearancePreferences) -> Theme;
//! pub fn zoom(app: &App) -> f32;
//! pub fn message_type(app: &App) -> TypeStyle;
//! pub fn window_backdrop(app: &mut App) -> Option<E>;
//! pub fn on_scheme_changed(app: &mut App, dark: bool);
//! pub fn window_refocused(app: &mut App);
//!
//! // Once the window is up, and among the root's windows: the sheet of the first launch, which `permissions_page` says more about.
//! pub fn first_launch(app: &mut App);
//! pub fn first_launch_dialog(app: &mut App) -> Option<E>;
//!
//! // The two windows opened from a gear beside a workspace or a machine. See their modules for their props.
//! pub fn workspace_settings_dialog(app: &mut App, props: WorkspaceSettingsProps) -> E;
//! pub fn machine_settings_dialog(app: &mut App, props: MachineSettingsProps) -> E;
//! ```

pub mod appearance;
pub mod dependencies;
pub mod effects;
pub mod logic;
pub mod machine_dialogs;
pub mod permissions_page;
pub mod providers;
mod providers_format {
    pub use crate::providers::format_bytes;
}
pub mod sandbox;
pub mod secret_field;
pub mod shell;
pub mod shortcuts_page;
pub mod updates;
pub mod usage_page;
pub mod web_search_behavior;
pub mod web_search_page;
pub mod widgets;
pub mod workspace_settings;

pub use effects::{backdrop_media, backdrop_source, message_type, on_scheme_changed, set_backdrop_covered, set_window_visible, themed, tick_backdrop, window_backdrop, window_refocused, zoom, BackdropMedia, BackdropSource};
pub use machine_dialogs::{machine_settings_dialog, MachineSettingsProps};
pub use permissions_page::{first_launch, first_launch_dialog};
pub use shell::{close, current_view, global_settings_body, global_settings_window, is_open, open, select_view, SettingsView};
pub use workspace_settings::{workspace_settings_dialog, WorkspaceSettingsProps};

/// The language is one switch for the process, and tests run at once: what asserts on wording holds this while it does.
#[cfg(test)]
pub(crate) fn language_for_test(english: bool) -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    mewrk_client::i18n::set_english(english);
    guard
}
