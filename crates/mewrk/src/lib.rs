//! The shell: what `App.tsx` renders at the top level, the components no other crate owns, and the wiring between
//! surfaces. `main.rs` is the window driver; [`root::root`] is what it draws.

pub mod actions;
pub mod chat;
pub mod chrome;
pub mod composer;
pub mod dialogs;
pub mod effects;
pub mod glass;
pub mod menu;
#[cfg(target_os = "macos")]
pub mod native;
pub mod pane_tiles;
pub mod panes;
pub mod root;
pub mod scenes;
pub mod shortcuts;
pub mod side_pane;
pub mod sidebar;
pub mod state;
pub mod topbar;
pub mod widgets;
