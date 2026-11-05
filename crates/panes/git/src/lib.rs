//! The review pane, upstream's `GitReviewPanel`, `DiffViewer`, `DiffOutput`, `GitStatusCard` and `lib/git*.ts`,
//! `lib/lineDiff.ts`, `lib/wordDiff.ts`, `lib/unifiedDiff.ts`.
//!
//! Public views:
//!
//! ```text
//! git_review_panel(app: &mut App, props: GitReviewProps) -> GitReviewView { header: E, menu_sections: Vec<MenuSection>, body: E }
//!     The pane. `header` goes in `SidePane`'s title bar, `menu_sections` behind its `⋮`, `body` in its body. Call it every
//!     frame the pane is on screen; stopping unmounts it, as upstream's panel. Key it by member and
//!     `git_review_snapshot_cache_key(snapshot)`, as upstream's `key` does.
//! diff_view(app: &mut App, props: DiffViewProps) -> E
//!     The continuous diff with its file column. No pane assumptions: chat and the files pane can use it.
//! diff_output(key, value: &str, path: Option<&str>) -> E                      The read-only diff card.
//! git_status_card(key, git: &GitWorkspaceSnapshot, git_open: bool,
//!                 on_open_review: Rc<dyn Fn(&mut App)>) -> E                  The composer row's chip and its panel.
//! branch_chips(app: &mut App, props: BranchChipProps) -> E                    The branch menu and the worktree box.
//! git_review_page_label(workspace: &str, branch: &str, worktree: Option<&str>) -> E
//! popover_menu(key, label, trigger: E, sections: impl FnOnce() -> Vec<MenuSection> + 'static, align: Align) -> E
//! ```
//!
//! And what the rest of the app calls in place of `gitController`, `useGitSurfacePolling` and friends:
//!
//! ```text
//! git_controller(app) -> &mut GitController       snapshots, acquire/release_mutation_lease, mutation_is_active
//! git_refresh(app, snapshot_key, workspace_id, target, done)
//! git_surface_polling(app, surfaces: &[GitPollSurface], refresh: GitPollRefresh, enabled)    Call it every frame.
//! git_polling_wake(app) / git_polling_set_visible(app, visible)        The window came back, or went.
//! git_snapshots_after_* / git_snapshot_broadcast_ids / git_review_lists_path / git_peer_blocks_mutation ...
//! set_file_icon(f)                                Who draws a file's icon: the files pane's `file_icon`.
//! ```

pub mod branch;
mod clipboard;
pub mod code_table;
pub mod controller;
pub mod diff_highlight;
pub mod diff_output;
pub mod diff_view;
pub mod git;
pub mod line_diff;
pub mod menu;
mod path;
pub mod polling;
pub mod review;
pub mod review_core;
mod spinner;
pub mod status_card;
#[cfg(test)]
mod testing;
pub mod unified_diff;
pub mod word_diff;

pub use branch::{branch_chip_error, branch_chips, checkout_branch, load_branch_picker, toggle_conversation_worktree, BranchChipHooks, BranchChipProps};
pub use controller::*;
pub use diff_output::{diff_output, parse_unified_diff_lines};
pub use diff_view::{diff_view, set_file_icon, DiffEntry, DiffEntryStatus, DiffStyle, DiffViewProps, DIFF_LARGE_LINE_COUNT};
pub use git::*;
pub use line_diff::{line_diff, line_diff_with, LineDiffOptions, LineDiffResult};
pub use menu::{popover_menu, MenuItem, MenuSection};
pub use polling::{git_polling_set_visible, git_polling_wake, git_surface_polling, next_git_poll_delay, GitPollRefresh, GitPollSurface};
pub use review::{git_review_page_label, git_review_panel, review_core, run_git_action, GitReviewHooks, GitReviewProps, GitReviewView};
pub use review_core::{DiffScope, GitReviewCheckout, GitReviewRevealRequest, ReviewCore, ReviewInputs};
pub use status_card::git_status_card;
pub use unified_diff::{parse_unified_diff, unquote_git_path, DiffFile, DiffHunk, DiffLine, DiffLineKind};
pub use word_diff::{tokenize_line, word_diff, word_diff_with, WordDiffOptions, WordDiffResult, WordDiffSegment};
