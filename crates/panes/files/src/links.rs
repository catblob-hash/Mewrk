//! A click on a file path the app detected (upstream's `lib/pathLinks.ts`), and the path rules surfaces reach for here. The rules
//! are `mewrk_ui_common::path_text`'s.

use crate::browser::BrowseMachine;
pub use mewrk_ui_common::path_text::{elide_path, external_http_url, is_absolute_path, local_link_target, LocalLinkTarget};

/// A click on a detected path, before anything has decided what to do with it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PathOpenRequest {
    /// Exactly what the link carries: absolute, or relative to `base_dir`.
    pub path: String,
    /// The directory relative paths resolve against, or `None` when the surface knew none.
    pub base_dir: Option<String>,
    /// The line the reference named, or `None` when it named only a file.
    pub line: Option<u32>,
    /// The workspace number a tool call named, when written against a workspace other than the surface's own.
    pub workspace: Option<u32>,
    /// Set by a turn's list of changed files: a file whose change Git tracks opens in the review pane.
    pub review: bool,
    /// Set by a tool row's file name: the file opens in a page of its own.
    pub new_page: bool,
    /// The machine the path is on, when the surface that showed it says so (outer `None` means it did not).
    pub machine: Option<BrowseMachine>,
    /// The link's box, for a menu that opens where the link is.
    pub anchor: Option<[f32; 4]>,
}
