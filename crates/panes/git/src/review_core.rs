//! What the review pane knows and decides, with nothing in it that touches the host or the screen: upstream's
//! `GitReviewPanel`, its state, refs and effects written as a state machine. A React effect here is a step of `sync` that
//! runs when what it depended on has changed; anything asynchronous is an `Effect` going out and a method its answer
//! comes back through, with the same fences upstream puts around each. That is what lets every race it guards against be
//! scripted in a test.

use std::collections::HashMap;
use std::sync::Arc;

use mewrk_client::{t, tf};
use serde_json::json;

use crate::diff_view::{DiffEntry, DiffEntryStatus, DiffStyle, DIFF_LARGE_LINE_COUNT};
use crate::git::*;
use crate::unified_diff::{parse_unified_diff, DiffFile};

pub const CHANGE_FILE_BATCH_SIZE: u16 = 200;
/// Changed files past which the scope is read one file at a time. A scope-wide `git diff` is one command holding the
/// repository lock for as long as it takes to write every hunk of every file; past this many files the wait is long
/// enough to stall the change list, and every file opens folded anyway.
pub const DIFF_LAZY_FILE_COUNT: u64 = 60;
/// Context lines asked for when a hunk boundary is expanded. Each step re-reads that one file at a wider `-U`; the last
/// is large enough to be the whole of any file a reviewer will read in a pane.
pub const DIFF_CONTEXT_STEPS: [u32; 3] = [25, 100, 100_000];

pub const DIFF_TREE_STORAGE_KEY: &str = "mewrk.review.showFiles";
pub const DIFF_STYLE_STORAGE_KEY: &str = "mewrk.review.diffStyle";
pub const DIFF_WORD_WRAP_STORAGE_KEY: &str = "mewrk.review.wordWrap";
pub const DIFF_WORD_DIFF_STORAGE_KEY: &str = "mewrk.review.wordDiff";
pub const DIFF_HIDE_WHITESPACE_STORAGE_KEY: &str = "mewrk.review.hideWhitespace";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestState {
    Idle,
    Loading,
    Ready,
    Error,
}

/// What the pane lists: every uncommitted change, one side of the index, or, for an isolated worktree, everything its
/// branch has done since it was forked, committed or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffScope {
    Working,
    Staged,
    Unstaged,
    Branch,
}

/// A changed file the pane has been asked to show from somewhere outside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitReviewRevealRequest {
    /// Repository-relative path, `/` separated.
    pub path: String,
    /// Bumped per request, so asking twice for the same file asks twice.
    pub nonce: u64,
    /// The pane was not open when the file was asked for: it opens on the diff alone, with the file column folded away.
    pub collapse_tree: bool,
}

/// What the checkout under review is, beyond what its snapshot says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitReviewCheckout {
    /// The name of the conversation's isolated worktree when the checkout is one, or `None` at a workspace's own directory.
    pub worktree_name: Option<String>,
    /// The branch the worktree was forked from, which the review is read against.
    pub base_branch: Option<String>,
    /// The commit the worktree was forked at. With it the pane offers the branch's whole change since then and opens on it.
    pub base_oid: Option<String>,
}

/// What a frame gives the panel: upstream's props.
#[derive(Clone, Debug)]
pub struct ReviewInputs {
    pub target: GitTarget,
    pub snapshot: GitWorkspaceSnapshot,
    pub active: bool,
    pub checkout: GitReviewCheckout,
    pub reveal_request: Option<GitReviewRevealRequest>,
    pub mutation_disabled_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDiscard {
    pub key: String,
    pub scope_key: String,
    pub snapshot_key: String,
    pub path: String,
    pub include_untracked: bool,
    pub content_revision: String,
    pub target_revision: String,
}

#[derive(Clone, Debug)]
pub enum Effect {
    LoadPage { request_id: u64, scope_key: String, cursor: Option<String>, request: GitChangePageRequest },
    LoadScopeDiff { request_id: u64, request: GitDiffRequest },
    LoadFilePatch { path: String, request_id: u64, step: Option<usize>, request: GitDiffRequest },
    /// `get_git_workspace_summary`, for the refresh in the pane's menu.
    Refresh { identity: String, known_revision: String },
    RunAction { request_id: u64, identity: String, key: String, action: GitAction },
    /// After an action failed: read the summary, since a failed write may still have changed the checkout.
    Reconcile { request_id: u64, identity: String, key: String, message: String },
    PrepareDiscard { request_id: u64, scope_key: String, snapshot_key: String, path: String, include_untracked: bool, confirmation: Option<PendingDiscard> },
    /// `onSnapshotChange`.
    Publish(Option<GitWorkspaceSnapshot>),
    /// `onMutationEnd`.
    MutationEnd,
    /// `onRevealRequestHandled`.
    RevealHandled(u64),
}

/// The review panel lists tracked changes only. `git status` enumerates untracked files and the workspace snapshot
/// carries them for the `git_status` tool and discard; the host filters them out of `get_git_change_page`, and this is
/// the same rule for the inline path that reads `snapshot.files` without asking the host for a page.
pub fn is_reviewable_change(file: &GitFileChange) -> bool {
    !file.untracked && file.status != GitFileStatus::Untracked
}

pub fn repository_operation_label(operation: GitRepositoryOperation) -> &'static str {
    match operation {
        GitRepositoryOperation::Merge => t("合并", "merge"),
        GitRepositoryOperation::Rebase => t("变基", "rebase"),
        GitRepositoryOperation::CherryPick => t("拣选提交", "cherry-pick"),
        GitRepositoryOperation::Revert => t("还原提交", "revert"),
        GitRepositoryOperation::Bisect => t("二分查找", "bisect"),
    }
}

pub fn operation_supports_continue(operation: GitRepositoryOperation) -> bool {
    operation != GitRepositoryOperation::Bisect
}

pub fn operation_supports_skip(operation: GitRepositoryOperation) -> bool {
    matches!(operation, GitRepositoryOperation::Rebase | GitRepositoryOperation::CherryPick | GitRepositoryOperation::Revert)
}

pub fn diff_snapshot_fingerprint(s: &GitWorkspaceSnapshot) -> String {
    json!([s.repository_id, s.worktree_id, s.summary_revision, s.content_revision, s.branch, s.head, s.staged, s.unstaged, s.untracked, s.conflicted, s.operation, s.operation_revision]).to_string()
}

fn git_remote_proof_key(remote: Option<&GitRemote>) -> Option<String> {
    remote.map(|r| json!([r.name, r.fetch_revision, r.push_revision, r.url]).to_string())
}

fn git_upstream_proof_key(upstream: Option<&GitUpstream>) -> Option<String> {
    upstream.map(|u| json!([u.remote_name, u.remote_branch, u.merge_ref, u.tracking_ref, u.tracking_oid, u.is_local, git_remote_proof_key(Some(&u.remote))]).to_string())
}

pub fn discard_preparation_snapshot_key(s: &GitWorkspaceSnapshot) -> String {
    json!([
        diff_snapshot_fingerprint(s),
        s.repository_id,
        s.worktree_id,
        s.repository_root,
        s.worktree_root,
        s.upstream,
        git_upstream_proof_key(s.upstream_target.as_ref()),
        s.ahead,
        s.behind,
        s.additions,
        s.deletions,
        s.stash,
        git_remote_proof_key(s.remote.as_ref()),
        s.remotes.iter().map(|r| git_remote_proof_key(Some(r))).collect::<Vec<_>>(),
        s.git_version,
        s.detached,
        s.unborn,
        s.is_clean,
        s.binary_files,
        s.changed_files,
        s.stageable,
        s.unstageable,
        s.warnings,
    ])
    .to_string()
}

pub fn snapshot_has_inline_changes(s: &GitWorkspaceSnapshot) -> bool {
    s.files_complete || s.summary_revision.is_empty()
}

/// Files this panel will list. A snapshot that carries no `files` still knows how many paths changed and how many of
/// those are untracked, so the tracked count is exact either way.
pub fn reviewable_file_count(s: &GitWorkspaceSnapshot) -> u64 {
    if snapshot_has_inline_changes(s) {
        return s.files.iter().filter(|f| is_reviewable_change(f)).count() as u64;
    }
    u64::from(s.changed_files).saturating_sub(u64::from(s.untracked))
}

fn failure_message(reason: String, fallback: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.is_empty() { fallback.to_owned() } else { trimmed.to_owned() }
}

/// A file's row in the `⋮` menu, as data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileAction {
    pub id: &'static str,
    pub label: String,
    pub icon: &'static str,
    pub disabled: bool,
    pub description: Option<String>,
}

pub struct ReviewCore {
    pub target: GitTarget,
    /// The prop as last given.
    prop_snapshot: GitWorkspaceSnapshot,
    pub current_snapshot: GitWorkspaceSnapshot,
    pub active: bool,
    pub checkout: GitReviewCheckout,
    pub mutation_disabled_reason: Option<String>,
    pub busy_action: Option<String>,
    pub operation_error: Option<String>,
    pub operation_message: Option<String>,
    pub selected_path: Option<String>,
    pub change_filter: String,
    pub change_files: Vec<GitFileChange>,
    pub change_matched_count: u64,
    pub change_next_cursor: Option<String>,
    pub change_page_state: RequestState,
    pub change_page_error: Option<String>,
    pub change_loading_more: bool,
    pub diff_scope: DiffScope,
    pub branch_file_count: Option<u64>,
    pub diff: Option<GitDiffResponse>,
    pub diff_state: RequestState,
    pub diff_error: Option<String>,
    pub diff_revision: u64,
    pub show_diff_files: bool,
    pub diff_style: DiffStyle,
    pub diff_word_wrap: bool,
    pub diff_word_diff: bool,
    pub diff_hide_whitespace: bool,
    pub diff_fold_all: Option<(bool, u64)>,
    pub diff_can_fit_files: bool,
    pub diff_can_fit_split: bool,
    pub diff_is_large: bool,
    /// Patches parsed out of the scope-wide diff, keyed by repository-relative path.
    pub scope_patches: HashMap<String, Arc<DiffFile>>,
    /// Paths whose patch had to be fetched on its own, and how that went.
    pub file_patches: HashMap<String, Arc<DiffFile>>,
    pub file_patch_states: HashMap<String, Option<String>>,
    /// How much context each file has been expanded to, as an index into the steps.
    pub context_steps: HashMap<String, usize>,
    pub pending_discard: Option<PendingDiscard>,
    pub pending_git_action: Option<String>,
    pub pending_reveal: Option<(String, u64, bool)>,
    pub diff_reveal: Option<(String, u64)>,
    reveal_nonce: Option<u64>,
    reveal_path: Option<String>,
    change_diff_request: u64,
    change_file_patch_requests: HashMap<String, u64>,
    change_page_request: u64,
    discard_preparation_request: u64,
    git_action_request: u64,
    diff_fingerprint: String,
    accepted_discard_snapshot_key: Option<String>,
    discard_preparation_snapshot_key: String,
    committed_discard_snapshot_key: String,
    discard_scope_key: String,
    // What each effect last ran for.
    dep_discard_scope: Option<String>,
    dep_identity: Option<String>,
    dep_active: Option<bool>,
    dep_reason: Option<Option<String>>,
    dep_operation: Option<String>,
    dep_change_page: Option<String>,
    dep_scope_diff: Option<String>,
}

impl ReviewCore {
    pub fn new(inputs: &ReviewInputs, show_diff_files: bool, diff_style: DiffStyle, word_wrap: bool, word_diff: bool, hide_whitespace: bool) -> Self {
        let s = &inputs.snapshot;
        let inline = snapshot_has_inline_changes(s);
        let discard_key = discard_preparation_snapshot_key(s);
        Self {
            target: inputs.target.clone(),
            prop_snapshot: s.clone(),
            current_snapshot: s.clone(),
            active: inputs.active,
            checkout: inputs.checkout.clone(),
            mutation_disabled_reason: inputs.mutation_disabled_reason.clone(),
            busy_action: None,
            operation_error: None,
            operation_message: None,
            // A pane mounted to show one file asks for that file with its first page.
            selected_path: inputs.reveal_request.as_ref().map(|r| r.path.clone()).or_else(|| s.files.iter().find(|f| is_reviewable_change(f)).map(|f| f.path.clone())),
            change_filter: String::new(),
            change_files: if inline { s.files.iter().filter(|f| is_reviewable_change(f)).cloned().collect() } else { Vec::new() },
            change_matched_count: reviewable_file_count(s),
            change_next_cursor: None,
            change_page_state: if inline { RequestState::Ready } else { RequestState::Idle },
            change_page_error: None,
            change_loading_more: false,
            diff_scope: if inputs.checkout.base_oid.is_some() { DiffScope::Branch } else { DiffScope::Working },
            branch_file_count: None,
            diff: None,
            diff_state: RequestState::Idle,
            diff_error: None,
            diff_revision: 0,
            show_diff_files,
            diff_style,
            diff_word_wrap: word_wrap,
            diff_word_diff: word_diff,
            diff_hide_whitespace: hide_whitespace,
            diff_fold_all: None,
            diff_can_fit_files: true,
            diff_can_fit_split: true,
            diff_is_large: false,
            scope_patches: HashMap::new(),
            file_patches: HashMap::new(),
            file_patch_states: HashMap::new(),
            context_steps: HashMap::new(),
            pending_discard: None,
            pending_git_action: None,
            pending_reveal: None,
            diff_reveal: None,
            reveal_nonce: None,
            reveal_path: inputs.reveal_request.as_ref().map(|r| r.path.clone()),
            change_diff_request: 0,
            change_file_patch_requests: HashMap::new(),
            change_page_request: 0,
            discard_preparation_request: 0,
            git_action_request: 0,
            diff_fingerprint: diff_snapshot_fingerprint(s),
            accepted_discard_snapshot_key: None,
            discard_preparation_snapshot_key: discard_key.clone(),
            committed_discard_snapshot_key: discard_key,
            discard_scope_key: String::new(),
            dep_discard_scope: None,
            dep_identity: None,
            dep_active: None,
            dep_reason: None,
            dep_operation: None,
            dep_change_page: None,
            dep_scope_diff: None,
        }
    }

    pub fn branch_base(&self) -> Option<&str> {
        self.checkout.base_oid.as_deref()
    }

    pub fn branch_scope(&self) -> bool {
        self.diff_scope == DiffScope::Branch && self.branch_base().is_some()
    }

    fn workspace_identity_key(&self) -> String {
        json!([self.prop_snapshot.repository_id, self.prop_snapshot.worktree_id]).to_string()
    }

    /// A branch not listed yet counts as one file, so the listing is asked for and the viewer is up to show it is loading.
    pub fn total_changed_files(&self) -> u64 {
        if self.branch_scope() { self.branch_file_count.unwrap_or(1) } else { reviewable_file_count(&self.current_snapshot) }
    }

    /// Whether the scope is past the size where one patch is worth asking for. The file count is a second gate because
    /// line counts alone understate the cost: three hundred small files is still one `git diff` holding the repository
    /// lock while every other Git read in the pane queues behind it.
    fn scope_is_too_large_to_read_at_once(&self) -> bool {
        (!self.branch_scope() && self.current_snapshot.additions + self.current_snapshot.deletions > DIFF_LARGE_LINE_COUNT) || self.total_changed_files() > DIFF_LAZY_FILE_COUNT
    }

    fn normalized_filter(&self) -> String {
        self.change_filter.trim().to_lowercase()
    }

    /// A branch's listing is always the host's: a snapshot carries only the uncommitted files.
    pub fn changes_are_inline(&self) -> bool {
        !self.branch_scope() && snapshot_has_inline_changes(&self.current_snapshot)
    }

    fn change_page_revision(&self) -> &str {
        if self.current_snapshot.summary_revision.is_empty() { &self.current_snapshot.content_revision } else { &self.current_snapshot.summary_revision }
    }

    fn change_page_scope_key(&self) -> String {
        let s = &self.current_snapshot;
        json!([self.target, s.repository_id, s.worktree_id, s.repository_root, s.worktree_root, self.change_page_revision(), self.normalized_filter(), self.changes_are_inline(), if self.branch_scope() { self.branch_base() } else { None }]).to_string()
    }

    fn compute_discard_scope_key(&self) -> String {
        let s = &self.prop_snapshot;
        json!([self.active, self.target, s.repository_id, s.worktree_id, s.repository_root, s.worktree_root, self.mutation_disabled_reason]).to_string()
    }

    fn clear_prepare_discard_busy(&mut self) {
        if self.busy_action.as_deref().is_some_and(|b| b.starts_with("prepare-discard:")) {
            self.busy_action = None;
        }
    }

    fn publish(&mut self, next: Option<GitWorkspaceSnapshot>, out: &mut Vec<Effect>) {
        if let Some(snapshot) = &next {
            self.diff_fingerprint = diff_snapshot_fingerprint(snapshot);
            self.current_snapshot = snapshot.clone();
        }
        out.push(Effect::Publish(next));
    }

    /// Drops the cached scope diff, so the next read goes back to the repository.
    fn invalidate_repository_reads(&mut self) {
        self.change_diff_request += 1;
        self.diff = None;
        self.diff_state = RequestState::Idle;
        self.diff_error = None;
    }

    /// The frame's props, and everything that has to follow from them: every effect whose dependencies changed. Runs
    /// until nothing moves, the way React re-renders until its effects stop setting state.
    pub fn sync(&mut self, inputs: &ReviewInputs) -> Vec<Effect> {
        let mut out = Vec::new();
        self.target = inputs.target.clone();
        self.active = inputs.active;
        self.checkout = inputs.checkout.clone();
        self.mutation_disabled_reason = inputs.mutation_disabled_reason.clone();
        let snapshot_changed = self.prop_snapshot != inputs.snapshot;
        if snapshot_changed {
            self.prop_snapshot = inputs.snapshot.clone();
            // Done while rendering upstream: a preparation still out for an older snapshot must not arm anything, unless
            // this snapshot is the one that preparation itself published.
            let key = discard_preparation_snapshot_key(&inputs.snapshot);
            if self.discard_preparation_snapshot_key != key {
                if self.accepted_discard_snapshot_key.as_deref() != Some(key.as_str()) {
                    self.discard_preparation_request += 1;
                }
                self.discard_preparation_snapshot_key = key;
            }
        }
        self.discard_scope_key = self.compute_discard_scope_key();

        // [discardScopeKey]
        if self.dep_discard_scope.as_deref() != Some(self.discard_scope_key.as_str()) {
            self.dep_discard_scope = Some(self.discard_scope_key.clone());
            self.discard_preparation_request += 1;
            self.accepted_discard_snapshot_key = None;
            self.pending_discard = None;
            self.clear_prepare_discard_busy();
        }
        // [discardSnapshotKey]
        let discard_key = discard_preparation_snapshot_key(&self.prop_snapshot);
        if self.committed_discard_snapshot_key != discard_key {
            self.committed_discard_snapshot_key = discard_key.clone();
            if self.accepted_discard_snapshot_key.as_deref() == Some(discard_key.as_str()) {
                self.accepted_discard_snapshot_key = None;
            } else {
                self.accepted_discard_snapshot_key = None;
                self.pending_discard = None;
                self.clear_prepare_discard_busy();
            }
        }
        // [snapshot]
        if snapshot_changed {
            let next = diff_snapshot_fingerprint(&inputs.snapshot);
            let changed = self.diff_fingerprint != next;
            self.diff_fingerprint = next;
            self.current_snapshot = inputs.snapshot.clone();
            if changed {
                self.diff_revision += 1;
            }
        }
        // [workspaceIdentityKey]: the same filesystem path can be replaced with another repository, and a linked worktree
        // shares repository metadata while having different HEAD and index state. Never let either reuse an old page,
        // patch, or armed write confirmation.
        let identity = self.workspace_identity_key();
        if self.dep_identity.as_deref() != Some(identity.as_str()) {
            self.dep_identity = Some(identity);
            self.change_diff_request += 1;
            self.change_page_request += 1;
            self.change_filter.clear();
            self.change_files.clear();
            self.change_next_cursor = None;
            self.change_loading_more = false;
            self.change_page_state = RequestState::Idle;
            self.change_page_error = None;
            self.selected_path = None;
            self.diff = None;
            self.diff_state = RequestState::Idle;
            self.diff_error = None;
            self.pending_git_action = None;
        }
        // [active]
        if self.dep_active != Some(self.active) {
            self.dep_active = Some(self.active);
            if !self.active {
                self.git_action_request += 1;
                self.change_diff_request += 1;
                self.change_page_request += 1;
                if self.diff_state == RequestState::Loading {
                    self.diff_state = RequestState::Idle;
                }
                if self.change_page_state == RequestState::Loading {
                    self.change_page_state = RequestState::Idle;
                }
                self.change_loading_more = false;
                self.pending_discard = None;
                self.pending_git_action = None;
            }
        }
        // [mutationDisabledReason]
        if self.dep_reason.as_ref() != Some(&self.mutation_disabled_reason) {
            self.dep_reason = Some(self.mutation_disabled_reason.clone());
            if self.mutation_disabled_reason.is_some() {
                self.pending_discard = None;
                self.pending_git_action = None;
            }
        }
        // [operation, operationRevision]
        let operation = json!([self.current_snapshot.operation, self.current_snapshot.operation_revision]).to_string();
        if self.dep_operation.as_deref() != Some(operation.as_str()) {
            self.dep_operation = Some(operation);
            self.pending_git_action = None;
        }
        // The rest can set state that the others read: run them until nothing moves.
        for _ in 0..8 {
            let before = self.progress_key();
            // Upstream's first page reads the selection from the render before its effects ran, which a mount aimed at one
            // file already holds. Here the identity reset has just cleared it, so the reveal has to come first.
            self.reveal_step(inputs, &mut out);
            self.change_page_step(&mut out);
            self.load_page_step(&mut out);
            self.settle_reveal_step();
            self.scope_diff_step(&mut out);
            if self.progress_key() == before {
                break;
            }
        }
        out
    }

    /// Enough of the state to tell whether a step did anything.
    fn progress_key(&self) -> String {
        json!([self.selected_path, self.change_files.len(), self.change_matched_count, format!("{:?}", self.change_page_state), format!("{:?}", self.diff_state), format!("{:?}", self.diff_scope), self.diff_revision, self.change_page_request, self.change_diff_request, self.pending_reveal.as_ref().map(|p| (p.0.clone(), p.2)), self.dep_change_page, self.dep_scope_diff, self.reveal_nonce, self.change_filter, self.show_diff_files]).to_string()
    }

    /// `[changePageScopeKey, changesAreInline, currentSnapshot.files, normalizedChangeFilter, resetFileCount]`
    fn change_page_step(&mut self, _out: &mut Vec<Effect>) {
        let reset_file_count = if self.branch_scope() { None } else { Some(self.total_changed_files()) };
        let files_key = if self.changes_are_inline() { json!(self.current_snapshot.files.iter().map(|f| (&f.path, f.staged, f.unstaged, f.additions, f.deletions)).collect::<Vec<_>>()).to_string() } else { String::new() };
        let key = format!("{}|{}|{}|{:?}", self.change_page_scope_key(), self.changes_are_inline(), files_key, reset_file_count);
        if self.dep_change_page.as_deref() == Some(key.as_str()) {
            return;
        }
        self.dep_change_page = Some(key);
        self.change_page_request += 1;
        self.change_next_cursor = None;
        self.change_loading_more = false;
        self.change_page_error = None;
        let filter = self.normalized_filter();
        if self.changes_are_inline() {
            let next: Vec<GitFileChange> = self.current_snapshot.files.iter().filter(|f| is_reviewable_change(f)).filter(|f| filter.is_empty() || f.path.to_lowercase().contains(&filter) || f.original_path.as_deref().is_some_and(|o| o.to_lowercase().contains(&filter))).cloned().collect();
            self.change_matched_count = next.len() as u64;
            let selected = self.selected_path.as_ref().and_then(|p| next.iter().find(|f| f.path == *p)).map(|f| f.path.clone());
            self.selected_path = selected.or_else(|| next.first().map(|f| f.path.clone()));
            self.change_files = next;
            self.change_page_state = RequestState::Ready;
            return;
        }
        self.change_files.clear();
        self.change_matched_count = if !filter.is_empty() { 0 } else { reset_file_count.or(self.branch_file_count).unwrap_or(1) };
        self.change_page_state = RequestState::Idle;
    }

    /// Asks for the first page when nothing is loaded and there is something to list.
    fn load_page_step(&mut self, out: &mut Vec<Effect>) {
        if !self.active || self.busy_action.is_some() || self.changes_are_inline() || self.change_page_state != RequestState::Idle || self.total_changed_files() == 0 {
            return;
        }
        self.load_change_page(None, out);
    }

    pub fn load_change_page(&mut self, cursor: Option<String>, out: &mut Vec<Effect>) {
        if !self.active || self.busy_action.is_some() || self.changes_are_inline() || self.current_snapshot.summary_revision.is_empty() {
            return;
        }
        self.change_page_request += 1;
        if cursor.is_some() {
            self.change_loading_more = true;
        } else {
            self.change_page_state = RequestState::Loading;
            self.change_page_error = None;
        }
        let filter = self.normalized_filter();
        let request = GitChangePageRequest { expected_revision: self.current_snapshot.summary_revision.clone(), cursor: cursor.clone(), query: (!filter.is_empty()).then_some(filter), limit: CHANGE_FILE_BATCH_SIZE, selected_path: self.selected_path.clone(), base: if self.branch_scope() { self.branch_base().map(str::to_owned) } else { None } };
        out.push(Effect::LoadPage { request_id: self.change_page_request, scope_key: self.change_page_scope_key(), cursor, request });
    }

    /// What the host answered for a page.
    pub fn page_answer(&mut self, request_id: u64, scope_key: &str, cursor: Option<&str>, result: Result<GitChangePageResult, String>) -> Vec<Effect> {
        let mut out = Vec::new();
        let current = request_id == self.change_page_request && scope_key == self.change_page_scope_key();
        if !current {
            return out;
        }
        self.change_loading_more = false;
        match result {
            Ok(GitChangePageResult::Stale { summary }) => {
                self.change_files.clear();
                self.change_matched_count = 0;
                self.change_next_cursor = None;
                self.change_page_state = RequestState::Idle;
                self.publish(Some(summary_to_snapshot(summary, Vec::new())), &mut out);
                self.diff_revision += 1;
            }
            Ok(GitChangePageResult::Page { revision, files, matched_count, next_cursor, selection }) => {
                if revision != self.current_snapshot.summary_revision {
                    self.change_page_error = Some(t("Git 变更页来自其他仓库修订；请刷新后重试", "The Git change page belongs to another repository revision. Refresh and try again.").to_owned());
                    self.change_page_state = RequestState::Error;
                    return out;
                }
                // A file the timeline asked for is listed even when it sorts past the pages read so far, since opening it
                // is the whole point of the click; the page that holds it later does not list it twice. Any other
                // selection off the page stays latent.
                let mut next: Vec<GitFileChange> = if cursor.is_some() {
                    let listed: Vec<String> = self.change_files.iter().map(|f| f.path.clone()).collect();
                    let mut all = self.change_files.clone();
                    all.extend(files.into_iter().filter(|f| !listed.contains(&f.path)));
                    all
                } else {
                    files
                };
                if let Some(GitChangeSelection::Present { file }) = &selection
                    && Some(&file.path) == self.reveal_path.as_ref()
                    && !next.iter().any(|f| f.path == file.path)
                {
                    next.push(file.clone());
                }
                self.change_matched_count = u64::from(matched_count);
                if self.branch_scope() && self.normalized_filter().is_empty() {
                    self.branch_file_count = Some(u64::from(matched_count));
                }
                self.change_next_cursor = next_cursor;
                match selection {
                    Some(GitChangeSelection::Present { file }) => self.selected_path = Some(file.path),
                    // Deliberately nothing: the latent path is kept so clearing the query can restore the user's
                    // selection, and a file the filter is hiding must not become the selected one in the meantime.
                    Some(GitChangeSelection::FilteredOut) => {}
                    Some(GitChangeSelection::Missing) => self.selected_path = next.first().map(|f| f.path.clone()),
                    None => {
                        if self.selected_path.is_none() {
                            self.selected_path = next.first().map(|f| f.path.clone());
                        }
                    }
                }
                self.change_files = next;
                self.change_page_state = RequestState::Ready;
                self.change_page_error = None;
            }
            Err(reason) => {
                self.change_page_error = Some(failure_message(reason, t("无法读取 Git 变更文件", "Unable to load Git changes")));
                self.change_page_state = RequestState::Error;
            }
        }
        out
    }

    /// Opens what the timeline asked for. The nonce is kept so a request that arrived while the pane was closed is still
    /// honoured on the mount that follows; the owner is told once it has been.
    fn reveal_step(&mut self, inputs: &ReviewInputs, out: &mut Vec<Effect>) {
        let Some(request) = &inputs.reveal_request else { return };
        if self.reveal_nonce == Some(request.nonce) {
            return;
        }
        self.reveal_nonce = Some(request.nonce);
        // A pane opened just to show this diff shows the diff: the file column folds away for this visit without changing
        // what the reader chose for the pane.
        if request.collapse_tree {
            self.show_diff_files = false;
        }
        // A filter hiding the file would hide the answer to the click.
        self.change_filter.clear();
        self.selected_path = Some(request.path.clone());
        self.reveal_path = Some(request.path.clone());
        self.pending_reveal = Some((request.path.clone(), request.nonce, false));
        out.push(Effect::RevealHandled(request.nonce));
    }

    /// Hands the asked-for file to the viewer once it is a row. It may sort past the pages read so far, and those were
    /// read without asking for it by name, so the first page is read once more with it as the selection, which lists it.
    /// A scope that does not hold it (staged-only while the change is unstaged) gives way to all changes.
    fn settle_reveal_step(&mut self) {
        let Some((path, seq, reread)) = self.pending_reveal.clone() else { return };
        if let Some(file) = self.change_files.iter().find(|f| f.path == path) {
            if (self.diff_scope == DiffScope::Staged && !git_file_has_staged_change(file)) || (self.diff_scope == DiffScope::Unstaged && !git_file_has_unstaged_change(file)) {
                self.diff_scope = DiffScope::Working;
            }
            self.diff_reveal = Some((path, seq));
            self.reveal_path = None;
            self.pending_reveal = None;
            return;
        }
        if self.total_changed_files() > 0 && matches!(self.change_page_state, RequestState::Idle | RequestState::Loading) {
            return;
        }
        if self.change_page_state != RequestState::Ready || self.changes_are_inline() || reread {
            self.reveal_path = None;
            self.pending_reveal = None;
            return;
        }
        self.selected_path = Some(path.clone());
        self.pending_reveal = Some((path, seq, true));
        self.change_page_state = RequestState::Idle;
    }

    fn scope_diff_request(&self, path: Option<String>, context: Option<u32>) -> GitDiffRequest {
        if self.branch_scope()
            && let Some(base) = self.branch_base()
        {
            return GitDiffRequest::Branch { base: base.to_owned(), path, context };
        }
        match self.diff_scope {
            DiffScope::Staged => GitDiffRequest::Staged { path, context },
            DiffScope::Unstaged => GitDiffRequest::Unstaged { path, context },
            DiffScope::Working | DiffScope::Branch => GitDiffRequest::Working { path, context },
        }
    }

    /// Reads the whole scope as one patch. The reference shell shows every changed file in one scroller, so the request
    /// is scope-wide rather than per file. It is skipped once the change set passes the size at which every file opens
    /// folded anyway, and skipped outright when nothing in the scope is reviewable.
    fn scope_diff_step(&mut self, out: &mut Vec<Effect>) {
        let too_large = self.scope_is_too_large_to_read_at_once();
        let has_files = self.total_changed_files() > 0;
        let key = json!([self.active, self.branch_base(), self.branch_scope(), self.busy_action, self.target, self.diff_revision, format!("{:?}", self.diff_scope), too_large, has_files]).to_string();
        if self.dep_scope_diff.as_deref() == Some(key.as_str()) {
            return;
        }
        self.dep_scope_diff = Some(key);
        // The effect's cleanup: a read it had out is over.
        self.change_diff_request += 1;
        if !self.active || self.busy_action.is_some() {
            return;
        }
        if too_large || !has_files {
            self.change_diff_request += 1;
            self.scope_patches.clear();
            self.diff_state = RequestState::Ready;
            self.diff_error = None;
            return;
        }
        self.change_diff_request += 1;
        self.diff_state = RequestState::Loading;
        self.diff_error = None;
        out.push(Effect::LoadScopeDiff { request_id: self.change_diff_request, request: self.scope_diff_request(None, None) });
    }

    pub fn scope_diff_answer(&mut self, request_id: u64, result: Result<GitDiffResponse, String>) {
        if request_id != self.change_diff_request {
            return;
        }
        match result {
            Ok(response) => {
                self.scope_patches = parse_unified_diff(&response.patch, None).into_iter().map(|f| (f.path.clone(), Arc::new(f))).collect();
                self.diff = Some(response);
                // Anything fetched on its own belonged to the previous revision of this scope; keeping it would show one
                // file from before the change beside the rest from after it.
                self.file_patches.clear();
                self.file_patch_states.clear();
                self.context_steps.clear();
                self.diff_state = RequestState::Ready;
            }
            Err(reason) => {
                self.scope_patches.clear();
                self.diff_error = Some(failure_message(reason, t("无法读取文件差异", "Unable to load file diff")));
                self.diff_state = RequestState::Error;
            }
        }
    }

    /// Fetches one file's patch on its own. Two reasons a file is not in the scope-wide patch: an untracked file has to be
    /// diffed against nothing, one path at a time, and a file whose context has been expanded needs re-reading at a wider
    /// `-U`. The call is idempotent for the plain case, and the fence is per path, because several files can be in flight
    /// at once and the slow one's answer is not stale just for being late.
    pub fn request_file_patch(&mut self, path: &str, step: Option<usize>) -> Option<Effect> {
        // An inactive panel still renders its viewer; reading a patch for a pane nobody is looking at is exactly the work
        // `active` exists to stop. Waiting for the scope-wide read matters for the same reason: until it answers, every
        // file looks missing.
        if !self.active || self.diff_state != RequestState::Ready {
            return None;
        }
        if step.is_none() && (self.file_patch_states.contains_key(path) || self.file_patches.contains_key(path)) {
            return None;
        }
        let request_id = self.change_file_patch_requests.get(path).copied().unwrap_or(0) + 1;
        self.change_file_patch_requests.insert(path.to_owned(), request_id);
        let context = step.map(|s| DIFF_CONTEXT_STEPS[s]);
        self.file_patch_states.insert(path.to_owned(), None);
        Some(Effect::LoadFilePatch { path: path.to_owned(), request_id, step, request: self.scope_diff_request(Some(path.to_owned()), context) })
    }

    pub fn file_patch_answer(&mut self, path: &str, request_id: u64, step: Option<usize>, result: Result<GitDiffResponse, String>) {
        if self.change_file_patch_requests.get(path) != Some(&request_id) {
            return;
        }
        match result {
            Ok(response) => {
                let Some(parsed) = parse_unified_diff(&response.patch, Some(path)).into_iter().next() else {
                    // An answer carrying no patch is still an answer. Recording it is what stops the viewer asking again
                    // for a file that has no line-level diff to give.
                    self.file_patch_states.insert(path.to_owned(), Some(t("这个范围没有行级差异", "No line-level diff in this scope").to_owned()));
                    return;
                };
                self.file_patch_states.remove(path);
                self.file_patches.insert(path.to_owned(), Arc::new(parsed));
                if let Some(step) = step {
                    self.context_steps.insert(path.to_owned(), step);
                }
            }
            Err(reason) => {
                self.file_patch_states.insert(path.to_owned(), Some(failure_message(reason, t("无法读取文件差异", "Unable to load file diff"))));
            }
        }
    }

    /// Asks for the next wider `-U` for one file, if there is one left.
    pub fn expand_file_context(&mut self, path: &str) -> Option<Effect> {
        let next = self.context_steps.get(path).map_or(0, |c| c + 1);
        if next >= DIFF_CONTEXT_STEPS.len() {
            return None;
        }
        self.request_file_patch(path, Some(next))
    }

    pub fn refresh_snapshot(&mut self) -> Vec<Effect> {
        if !self.active || self.busy_action.is_some() {
            return Vec::new();
        }
        self.busy_action = Some("refresh".to_owned());
        self.operation_error = None;
        self.operation_message = None;
        vec![Effect::Refresh { identity: self.workspace_identity_key(), known_revision: self.current_snapshot.summary_revision.clone() }]
    }

    fn apply_summary(&mut self, identity: &str, result: GitWorkspaceSummaryResult, out: &mut Vec<Effect>) -> bool {
        if identity != self.workspace_identity_key() {
            return false;
        }
        match result {
            GitWorkspaceSummaryResult::NotRepository => self.publish(None, out),
            GitWorkspaceSummaryResult::Snapshot { summary } => self.publish(Some(summary_to_snapshot(summary, Vec::new())), out),
            GitWorkspaceSummaryResult::Unchanged { .. } => {}
        }
        true
    }

    pub fn refresh_answer(&mut self, identity: &str, result: Result<GitWorkspaceSummaryResult, String>) -> Vec<Effect> {
        let mut out = Vec::new();
        match result {
            Ok(summary) => {
                self.apply_summary(identity, summary, &mut out);
                self.diff_revision += 1;
            }
            Err(reason) => self.operation_error = Some(failure_message(reason, t("无法刷新 Git 状态", "Unable to refresh Git status"))),
        }
        self.busy_action = None;
        out
    }

    /// Whether a write may start: the caller then asks `onMutationStart` and calls `begin_action`.
    pub fn can_run_action(&self) -> bool {
        self.active && self.busy_action.is_none() && self.mutation_disabled_reason.is_none()
    }

    /// A write. `started` is what `onMutationStart` said: another task or workspace operation may have got there first.
    pub fn begin_action(&mut self, action: GitAction, key: &str, started: bool) -> Vec<Effect> {
        if !self.can_run_action() {
            return Vec::new();
        }
        if !started {
            self.operation_error = Some(t("另一个任务或工作区操作已经开始，请稍后重试", "Another task or workspace operation has started. Try again shortly.").to_owned());
            return Vec::new();
        }
        self.git_action_request += 1;
        self.invalidate_repository_reads();
        self.busy_action = Some(key.to_owned());
        self.operation_error = None;
        self.operation_message = None;
        vec![Effect::RunAction { request_id: self.git_action_request, identity: self.workspace_identity_key(), key: key.to_owned(), action }]
    }

    fn finish_action(&mut self, request_id: u64, key: &str, out: &mut Vec<Effect>) {
        if request_id == self.git_action_request && self.busy_action.as_deref() == Some(key) {
            self.busy_action = None;
        }
        out.push(Effect::MutationEnd);
    }

    /// What the host answered to a write. `was` says what it was, since the message rule depends on it. Answers whether
    /// it succeeded in `Effect`s; `succeeded` is for `confirm_local_git_action`.
    pub fn action_answer(&mut self, request_id: u64, identity: &str, key: &str, action_quiet: bool, result: Result<GitActionResult, String>) -> Vec<Effect> {
        let mut out = Vec::new();
        let current = request_id == self.git_action_request && identity == self.workspace_identity_key();
        match result {
            Ok(result) => {
                if current {
                    self.publish(result.snapshot, &mut out);
                    self.diff_revision += 1;
                    if !action_quiet {
                        let message = result.message.as_deref().map(str::trim).filter(|m| !m.is_empty());
                        self.operation_message = Some(message.map_or_else(|| t("Git 操作已完成", "Git operation completed").to_owned(), str::to_owned));
                    }
                }
                self.finish_action(request_id, key, &mut out);
            }
            Err(reason) => {
                if !current {
                    self.finish_action(request_id, key, &mut out);
                    return out;
                }
                let message = failure_message(reason, t("Git 操作失败", "Git operation failed"));
                // A failed write may have changed the checkout part of the way, so read it back before saying so.
                out.push(Effect::Reconcile { request_id, identity: identity.to_owned(), key: key.to_owned(), message });
            }
        }
        out
    }

    /// The read after a failed write. Preserves the action failure if this fails too: it is the primary error, while the
    /// regular polling path can retry a snapshot that temporarily could not be read.
    pub fn reconcile_answer(&mut self, request_id: u64, identity: &str, key: &str, message: String, result: Result<GitWorkspaceSummaryResult, String>) -> Vec<Effect> {
        let mut out = Vec::new();
        let accepted = match result {
            Ok(summary) => {
                let accepted = self.apply_summary(identity, summary, &mut out);
                if accepted {
                    self.diff_revision += 1;
                }
                accepted
            }
            Err(_) => true,
        };
        if accepted {
            self.operation_error = Some(message);
        }
        self.finish_action(request_id, key, &mut out);
        out
    }

    /// Discard keeps its two-click proof: the first selection arms it and the row's label becomes the confirmation, so the
    /// menu is a different affordance for the same guarded action rather than a way around it. Either click first prepares
    /// a proof of what the repository looks like right now.
    pub fn request_discard(&mut self, path: &str, include_untracked: bool, confirmation: Option<PendingDiscard>) -> Vec<Effect> {
        self.pending_discard = None;
        if !self.active || self.busy_action.is_some() || self.mutation_disabled_reason.is_some() {
            return Vec::new();
        }
        self.discard_preparation_request += 1;
        self.busy_action = Some(format!("prepare-discard:{path}"));
        self.operation_error = None;
        self.operation_message = None;
        vec![Effect::PrepareDiscard { request_id: self.discard_preparation_request, scope_key: self.discard_scope_key.clone(), snapshot_key: self.discard_preparation_snapshot_key.clone(), path: path.to_owned(), include_untracked, confirmation }]
    }

    /// What the host answered to a preparation. Either arms the confirmation, or, when the click that asked for it was the
    /// confirmation and nothing has moved since, runs the discard: the returned key and action, with `onMutationStart`
    /// still to be asked.
    #[allow(clippy::too_many_arguments)]
    pub fn discard_prepared(&mut self, request_id: u64, scope_key: &str, snapshot_key: &str, path: &str, include_untracked: bool, confirmation: Option<PendingDiscard>, result: Result<GitDiscardPreparation, String>) -> (Vec<Effect>, Option<(GitAction, String)>) {
        let mut out = Vec::new();
        let busy_key = format!("prepare-discard:{path}");
        let current = request_id == self.discard_preparation_request && scope_key == self.discard_scope_key;
        match result {
            Ok(preparation) => {
                if !current || snapshot_key != self.discard_preparation_snapshot_key {
                    return (out, None);
                }
                let accepted = discard_preparation_snapshot_key(&preparation.snapshot);
                self.accepted_discard_snapshot_key = Some(accepted.clone());
                self.discard_preparation_snapshot_key = accepted.clone();
                let content_revision = preparation.snapshot.content_revision.clone();
                self.publish(Some(preparation.snapshot), &mut out);
                if self.busy_action.as_deref() == Some(busy_key.as_str()) {
                    self.busy_action = None;
                }
                let prepared = PendingDiscard {
                    key: json!([scope_key, accepted, path, include_untracked, content_revision, preparation.target_revision]).to_string(),
                    scope_key: scope_key.to_owned(),
                    snapshot_key: accepted,
                    path: path.to_owned(),
                    include_untracked,
                    content_revision,
                    target_revision: preparation.target_revision,
                };
                let confirmed = confirmation.as_ref().is_some_and(|c| c.scope_key == prepared.scope_key && c.snapshot_key == prepared.snapshot_key && c.path == prepared.path && c.include_untracked == prepared.include_untracked && c.content_revision == prepared.content_revision && c.target_revision == prepared.target_revision);
                if !confirmed {
                    self.pending_discard = Some(prepared);
                    return (out, None);
                }
                let action = GitAction::Discard { paths: vec![prepared.path.clone()], include_untracked: prepared.include_untracked, expected_content_revision: prepared.content_revision.clone(), expected_target_revision: prepared.target_revision.clone() };
                (out, Some((action, format!("discard:{}:{}", prepared.content_revision, prepared.path))))
            }
            Err(reason) => {
                if current && snapshot_key == self.discard_preparation_snapshot_key {
                    self.pending_discard = None;
                    self.operation_error = Some(failure_message(reason, t("无法准备安全丢弃", "Unable to prepare a safe discard")));
                    if self.busy_action.as_deref() == Some(busy_key.as_str()) {
                        self.busy_action = None;
                    }
                }
                (out, None)
            }
        }
    }

    /// Two clicks for what cannot be taken back: the first arms it, the second does it. `None` armed it; `Some` is to run.
    pub fn confirm_local_git_action(&mut self, key: &str) -> bool {
        if self.pending_git_action.as_deref() != Some(key) {
            self.pending_git_action = Some(key.to_owned());
            return false;
        }
        self.pending_git_action = None;
        true
    }

    /// Whether the panel is disabled for writes, for any reason.
    pub fn mutations_disabled(&self) -> bool {
        self.busy_action.is_some() || self.mutation_disabled_reason.is_some()
    }

    /// The change list as rows of the viewer's file column. The reference shell's column is a directory tree of everything
    /// in the scope, so staging composition rides along as a note rather than splitting the list into groups.
    pub fn diff_entries(&self) -> Vec<DiffEntry> {
        self.change_files
            .iter()
            .map(|file| {
                let (staged, unstaged) = (git_file_has_staged_change(file), git_file_has_unstaged_change(file));
                let untracked = file.untracked || file.status == GitFileStatus::Untracked;
                let status = if file.conflicted || file.status == GitFileStatus::Unmerged {
                    DiffEntryStatus::Conflicted
                } else if untracked {
                    DiffEntryStatus::Untracked
                } else {
                    match file.status {
                        GitFileStatus::Added => DiffEntryStatus::Added,
                        GitFileStatus::Deleted => DiffEntryStatus::Deleted,
                        GitFileStatus::Renamed => DiffEntryStatus::Renamed,
                        GitFileStatus::Copied => DiffEntryStatus::Copied,
                        GitFileStatus::TypeChanged => DiffEntryStatus::TypeChanged,
                        _ => DiffEntryStatus::Modified,
                    }
                };
                let patch_state = self.file_patch_states.get(&file.path);
                let failure = patch_state.and_then(Clone::clone);
                DiffEntry {
                    path: file.path.clone(),
                    old_path: file.original_path.clone(),
                    status,
                    additions: file.additions.unwrap_or(0),
                    deletions: file.deletions.unwrap_or(0),
                    binary: file.binary,
                    note: (staged && unstaged).then(|| t("已暂存 + 未暂存", "Staged + unstaged").to_owned()),
                    loading: matches!(patch_state, Some(None)),
                    unavailable: failure.or_else(|| file.submodule.then(|| t("子模块变更没有行级差异", "A submodule change has no line-level diff").to_owned())),
                }
            })
            .collect()
    }

    /// A file fetched on its own wins: it is either the only place its patch exists (untracked) or a wider read of the
    /// same file (expanded context).
    pub fn diff_patches(&self) -> HashMap<String, Arc<DiffFile>> {
        let mut merged = self.scope_patches.clone();
        for (path, file) in &self.file_patches {
            merged.insert(path.clone(), file.clone());
        }
        merged
    }

    pub fn expanding_paths(&self) -> std::collections::HashSet<String> {
        self.file_patch_states.iter().filter(|(_, s)| s.is_none()).map(|(p, _)| p.clone()).collect()
    }

    pub fn fully_expanded_paths(&self) -> std::collections::HashSet<String> {
        self.context_steps.iter().filter(|(_, s)| **s >= DIFF_CONTEXT_STEPS.len() - 1).map(|(p, _)| p.clone()).collect()
    }

    pub fn remaining_changes(&self) -> u64 {
        self.change_matched_count.saturating_sub(self.change_files.len() as u64)
    }

    pub fn next_change_page_count(&self) -> u64 {
        u64::from(CHANGE_FILE_BATCH_SIZE).min(self.remaining_changes())
    }

    fn discard_armed(&self, path: &str, untracked: bool) -> bool {
        self.pending_discard.as_ref().is_some_and(|p| p.scope_key == self.discard_scope_key && p.snapshot_key == discard_preparation_snapshot_key(&self.current_snapshot) && p.path == path && p.include_untracked == untracked && p.content_revision == self.current_snapshot.content_revision)
    }

    /// The `⋮` on a file row and on its sticky header, as data: the stage, unstage and discard rows it has.
    pub fn file_actions(&self, path: &str) -> Vec<FileAction> {
        let Some(file) = self.change_files.iter().find(|f| f.path == path) else { return Vec::new() };
        let (staged, unstaged) = (git_file_has_staged_change(file), git_file_has_unstaged_change(file));
        let untracked = file.untracked || file.status == GitFileStatus::Untracked;
        let stageable = unstaged && (!file.submodule || file.submodule_commit_changed);
        let discardable = unstaged && !file.submodule;
        let armed = self.discard_armed(path, untracked);
        let disabled = self.mutations_disabled();
        let mut items = Vec::new();
        if stageable {
            items.push(FileAction { id: "stage", label: t("暂存", "Stage").to_owned(), icon: "Plus", disabled, description: None });
        }
        if staged {
            items.push(FileAction { id: "unstage", label: t("取消暂存", "Unstage").to_owned(), icon: "RotateCcw", disabled, description: None });
        }
        if discardable {
            let label = match (armed, untracked) {
                (true, true) => t("确认永久删除", "Confirm permanent deletion"),
                (true, false) => t("确认丢弃未暂存更改", "Confirm discard of unstaged changes"),
                (false, true) => t("永久删除未跟踪文件", "Permanently delete untracked file"),
                (false, false) => t("丢弃未暂存更改", "Discard unstaged changes"),
            };
            items.push(FileAction {
                id: "discard",
                label: label.to_owned(),
                icon: if armed { "Check" } else { "Trash2" },
                disabled,
                description: (!armed && untracked).then(|| t("这个文件不在 Git 里，删除无法撤销。", "This file is not in Git; deleting it cannot be undone.").to_owned()),
            });
        }
        items
    }

    pub fn is_untracked(&self, path: &str) -> bool {
        self.change_files.iter().any(|f| f.path == path && (f.untracked || f.status == GitFileStatus::Untracked))
    }

    /// The pending confirmation for this file, if the row is armed.
    pub fn armed_discard(&self, path: &str) -> Option<PendingDiscard> {
        self.discard_armed(path, self.is_untracked(path)).then(|| self.pending_discard.clone()).flatten()
    }

    /// A narrowed scope, named after the refs; the scope the page opens on needs no word.
    pub fn scope_note(&self) -> Option<&'static str> {
        match self.diff_scope {
            DiffScope::Staged => Some(t("已暂存", "Staged")),
            DiffScope::Unstaged => Some(t("未暂存", "Unstaged")),
            DiffScope::Working if self.branch_base().is_some() => Some(t("未提交", "Uncommitted")),
            _ => None,
        }
    }

    pub fn branch_label(&self) -> String {
        self.checkout.base_branch.clone().or_else(|| self.current_snapshot.branch.clone()).or_else(|| self.current_snapshot.head.as_ref().map(|h| h.chars().take(8).collect())).unwrap_or_else(|| t("尚无提交", "No commits yet").to_owned())
    }

    /// The key an in-progress operation's buttons are confirmed under: it changes when the operation does.
    pub fn operation_scope(&self) -> Option<String> {
        let s = &self.current_snapshot;
        s.operation.map(|operation| format!("{}:{}:{}:{}", self.workspace_identity_key(), json!(operation), s.head.as_deref().unwrap_or("unborn"), s.operation_revision.as_deref().unwrap_or("unavailable")))
    }

    /// What the review's empty page says.
    pub fn empty_label(&self) -> String {
        if self.branch_scope() {
            t("自工作树分出以来没有变更", "No changes since the worktree was forked").to_owned()
        } else if self.current_snapshot.untracked > 0 {
            tf("没有已跟踪的变更；{count} 个未跟踪文件不在审阅范围内。", "No tracked changes. {count} untracked files are out of review scope.", &[("count", &self.current_snapshot.untracked)])
        } else {
            t("工作区没有变更", "Working tree is clean").to_owned()
        }
    }

    /// What the operation banner says under its title.
    pub fn operation_note(&self) -> Option<String> {
        let s = &self.current_snapshot;
        let operation = s.operation?;
        Some(if s.conflicted > 0 {
            tf("仍有 {count} 个冲突；解决并暂存后再继续", "{count} conflicts remain; resolve and stage them before continuing", &[("count", &s.conflicted)])
        } else if operation == GitRepositoryOperation::Bisect {
            if s.is_clean {
                t("测试当前提交，然后标记旧状态、新状态或跳过；自定义术语由 Git 自动映射", "Test this commit, then mark it old, new, or skip it. Git maps custom terms automatically.").to_owned()
            } else {
                t("先提交或储藏当前变更，才能继续二分查找", "Commit or stash the current changes before advancing the bisect.").to_owned()
            }
        } else {
            t("没有检测到未合并文件，可以继续当前操作", "No unmerged files were detected; the operation can continue").to_owned()
        })
    }
}

/// Which actions report a message when they finish.
pub fn action_is_quiet(action: &GitAction) -> bool {
    matches!(action, GitAction::Stage { .. } | GitAction::Unstage { .. } | GitAction::Discard { .. })
}

#[cfg(test)]
#[path = "review_core_tests.rs"]
mod tests;
