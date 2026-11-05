//! The shapes other areas own that a task row is made from: the browser pane's and the terminal pane's own.

pub use mewrk_pane_browser::browser::BrowserStatus;
pub use mewrk_pane_browser::preview::{preview_page_is_served_by, preview_server_address, remote_control_detail, PreviewRemoteControl, PreviewServerSnapshot, PreviewServerStatus, RemoteControlPlatform, RemoteControlProtocol, RemoteControlState};
pub use mewrk_pane_terminal::{TerminalPhase, TerminalSessionState};
