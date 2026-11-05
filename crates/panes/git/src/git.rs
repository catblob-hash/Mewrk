//! The host's Git commands and what they carry: upstream's `lib/git.ts`. The wire types are `git_core`'s own, the ones
//! the host serialises.

use mewrk_client::prelude::*;
use mewrk_client::t;
use serde::Serialize;

pub use git_core::{
    GitAction, GitActionResult, GitBisectOutcome, GitBranch, GitBranchKind, GitBranchesResult, GitChangePageRequest, GitChangePageResult, GitChangeSelection, GitDiffRequest, GitDiffResponse, GitDiscardPreparation, GitFileChange, GitFileStatus, GitRemote, GitRepositoryOperation, GitUpstream, GitWorkspaceSnapshot, GitWorkspaceSummary, GitWorkspaceSummaryResult,
};

/// Checkout targeted by a Git request. Git status belongs to a working directory, not a conversation. Both forms send
/// only ids to the host, which resolves paths from persisted documents; the client must never supply a repository path.
/// A workspace target resolves to the registered directory; a conversation target to its worktree when it has one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum GitTarget {
    Conversation {
        conversation_id: String,
        /// Which of the project's workspaces, 1-based. Absent or 1 is workspace 1.
        #[serde(skip_serializing_if = "Option::is_none")]
        member: Option<u32>,
    },
    Workspace {
        workspace_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        member: Option<u32>,
    },
}

pub fn git_conversation_target(conversation_id: &str, member: Option<u32>) -> GitTarget {
    GitTarget::Conversation { conversation_id: conversation_id.to_owned(), member: member.filter(|m| *m > 1) }
}

pub fn git_workspace_target(workspace_id: &str, member: Option<u32>) -> GitTarget {
    GitTarget::Workspace { workspace_id: workspace_id.to_owned(), member: member.filter(|m| *m > 1) }
}

/// Stable string suitable for state keys and cache keys.
pub fn git_target_key(target: &GitTarget) -> String {
    match target {
        GitTarget::Conversation { conversation_id, member: Some(m) } if *m > 1 => format!("conversation:{conversation_id}#{m}"),
        GitTarget::Conversation { conversation_id, .. } => format!("conversation:{conversation_id}"),
        GitTarget::Workspace { workspace_id, member: Some(m) } if *m > 1 => format!("workspace:{workspace_id}#{m}"),
        GitTarget::Workspace { workspace_id, .. } => format!("workspace:{workspace_id}"),
    }
}

/// The key a conversation's Git snapshot is stored under: the project id for the project's first workspace, and
/// `<project id>#<member>` for another. Two directories of one project are two checkouts.
pub fn git_surface_key(workspace_id: &str, member: u32) -> String {
    if member > 1 { format!("{workspace_id}#{member}") } else { workspace_id.to_owned() }
}

/// The project a surface key belongs to.
pub fn git_surface_project_id(surface_key: &str) -> &str {
    surface_key.split_once('#').map_or(surface_key, |(p, _)| p)
}

/// The key a conversation's snapshot of its project workspace `member` is cached under.
pub fn git_snapshot_key(conversation_id: &str, member: u32) -> String {
    if member > 1 { format!("{conversation_id}#{member}") } else { conversation_id.to_owned() }
}

/// The conversation a snapshot key belongs to.
pub fn git_snapshot_key_conversation(key: &str) -> &str {
    key.split_once('#').map_or(key, |(c, _)| c)
}

pub fn git_file_has_staged_change(file: &GitFileChange) -> bool {
    file.staged
}

pub fn git_file_has_unstaged_change(file: &GitFileChange) -> bool {
    file.unstaged
}

/// A summary and no files yet: what `summaryToGitWorkspaceSnapshot` makes.
pub fn summary_to_snapshot(summary: GitWorkspaceSummary, files: Vec<GitFileChange>) -> GitWorkspaceSnapshot {
    let GitWorkspaceSummary { repository_id, worktree_id, branch, head, content_revision, summary_revision, upstream, upstream_target, ahead, behind, additions, deletions, staged, unstaged, untracked, conflicted, stash, changed_files, stageable, unstageable, remote, remotes, git_version, repository_root, worktree_root, detached, unborn, operation, operation_revision, is_clean, binary_files, warnings } = summary;
    GitWorkspaceSnapshot { repository_id, worktree_id, branch, head, content_revision, upstream, upstream_target, ahead, behind, additions, deletions, staged, unstaged, untracked, conflicted, stash, files, remote, remotes, git_version, repository_root, worktree_root, detached, unborn, operation, operation_revision, is_clean, binary_files, warnings, summary_revision, changed_files, stageable, unstageable, files_complete: false }
}

/// One side of a Git write conflict test. `snapshot` is what the host last answered for that conversation's checkout in
/// the workspace being written: `None` never seen, `Some(None)` not a repository. `isolated` says the conversation still
/// records its own worktree; a draft is always false.
#[derive(Clone, Copy)]
pub struct GitCheckoutRef<'a> {
    pub snapshot: Option<Option<&'a GitWorkspaceSnapshot>>,
    pub isolated: bool,
}

fn checkout_worktree_id<'a>(snapshot: Option<Option<&'a GitWorkspaceSnapshot>>) -> Option<&'a str> {
    let snapshot = snapshot.flatten()?;
    if snapshot.repository_id.is_empty() || snapshot.worktree_id.is_empty() {
        return None;
    }
    Some(&snapshot.worktree_id)
}

/// Whether two conversations are known to work in different checkouts. The judgement belongs to the host and is read
/// back from the identities in its snapshots. A recorded worktree on its own proves nothing, because the host falls back
/// to the workspace root when that directory is gone. Anything not observed reads as the same checkout, so an unknown
/// peer keeps blocking.
pub fn git_checkouts_are_distinct(acting: GitCheckoutRef<'_>, peer: GitCheckoutRef<'_>) -> bool {
    if !acting.isolated && !peer.isolated {
        return false;
    }
    match (checkout_worktree_id(acting.snapshot), checkout_worktree_id(peer.snapshot)) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    }
}

/// Whether one workspace peer blocks a Git write by the acting conversation. A peer holding a Git mutation lease always
/// blocks: linked worktrees share the repository, so two writes still interleave. A peer merely running a model blocks
/// only while it may be working in the same checkout.
pub fn git_peer_blocks_mutation(acting: GitCheckoutRef<'_>, peer: GitCheckoutRef<'_>, peer_model_run_active: bool, peer_git_mutation_active: bool) -> bool {
    if peer_git_mutation_active {
        return true;
    }
    if !peer_model_run_active {
        return false;
    }
    !git_checkouts_are_distinct(acting, peer)
}

/// A worktree that deleting its task left on disk: one holding uncommitted changes or new commits (`error` is `None`),
/// or one that could not be released, with the reason.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct KeptWorktree {
    pub path: String,
    pub error: Option<String>,
}

/// What to tell the user about the worktrees a deletion left on disk, or `None` for none.
pub fn kept_worktrees_notice(kept: &[KeptWorktree]) -> Option<String> {
    let holding: Vec<&str> = kept.iter().filter(|w| w.error.is_none()).map(|w| w.path.as_str()).collect();
    let mut sentences = Vec::new();
    if holding.len() == 1 {
        sentences.push(tf("工作树 {path} 里还有未提交的改动或新提交，目录与分支已保留", "The worktree {path} still has uncommitted changes or new commits, so its directory and branch were kept", &[("path", &holding[0])]));
    } else if holding.len() > 1 {
        let paths = holding.join(t("、", ", "));
        sentences.push(tf("{count} 个工作树里还有未提交的改动或新提交，目录与分支已保留：{paths}", "{count} worktrees still have uncommitted changes or new commits, so their directories and branches were kept: {paths}", &[("count", &holding.len()), ("paths", &paths)]));
    }
    for worktree in kept.iter().filter(|w| w.error.is_some()) {
        let error = worktree.error.as_deref().unwrap_or_default();
        sentences.push(tf("无法释放工作树 {path}，它仍在磁盘上：{error}", "Could not release the worktree {path}, so it is still on disk: {error}", &[("path", &worktree.path), ("error", &error)]));
    }
    (!sentences.is_empty()).then(|| sentences.join(t("。", ". ")))
}

// ---- commands

pub fn get_git_workspace_summary(app: &App, target: &GitTarget, known_revision: Option<&str>, done: impl FnOnce(&mut App, Result<GitWorkspaceSummaryResult, String>) + Send + 'static) {
    app.invoke("get_git_workspace_summary", json!({ "target": target, "knownRevision": known_revision }), done);
}

pub fn get_git_change_page(app: &App, target: &GitTarget, request: &GitChangePageRequest, done: impl FnOnce(&mut App, Result<GitChangePageResult, String>) + Send + 'static) {
    app.invoke("get_git_change_page", json!({ "target": target, "request": request }), done);
}

pub fn get_git_diff(app: &App, target: &GitTarget, request: &GitDiffRequest, done: impl FnOnce(&mut App, Result<GitDiffResponse, String>) + Send + 'static) {
    app.invoke("get_git_diff", json!({ "target": target, "request": request }), done);
}

pub fn get_git_branches(app: &App, target: &GitTarget, done: impl FnOnce(&mut App, Result<GitBranchesResult, String>) + Send + 'static) {
    app.invoke("get_git_branches", json!({ "target": target }), done);
}

pub fn execute_git_action(app: &App, target: &GitTarget, action: &GitAction, done: impl FnOnce(&mut App, Result<GitActionResult, String>) + Send + 'static) {
    app.invoke("execute_git_action", json!({ "target": target, "action": action }), done);
}

pub fn prepare_git_discard(app: &App, target: &GitTarget, paths: &[String], include_untracked: bool, done: impl FnOnce(&mut App, Result<GitDiscardPreparation, String>) + Send + 'static) {
    app.invoke("prepare_git_discard", json!({ "target": target, "paths": paths, "includeUntracked": include_untracked }), done);
}

/// Create an isolated worktree of the conversation's project workspace `member` as the conversation's trusted directory
/// for that workspace, on whichever machine it is. `from_branch` is the baseline branch, defaulting to the workspace
/// HEAD. Persist the returned record in `Conversation.worktrees` so future host resolution uses it.
pub fn create_conversation_worktree(app: &App, conversation_id: &str, member: u32, from_branch: Option<&str>, done: impl FnOnce(&mut App, Result<model::ConversationWorktree, String>) + Send + 'static) {
    app.invoke("create_conversation_worktree", json!({ "conversationId": conversation_id, "member": member, "fromBranch": from_branch }), done);
}

/// Release a conversation's isolated worktree of its project workspace `member`. True when the worktree and branch were
/// deleted; false when uncommitted work or additional commits require retaining it on disk. Either removes the
/// conversation's association.
pub fn release_conversation_worktree(app: &App, conversation_id: &str, member: u32, done: impl FnOnce(&mut App, Result<bool, String>) + Send + 'static) {
    app.invoke("release_conversation_worktree", json!({ "conversationId": conversation_id, "member": member }), done);
}

/// Releases the worktrees that deleting these conversations leaves no conversation using. Call it just before they are
/// deleted: the host finds the worktrees through the records its saved document still holds for them.
pub fn release_worktrees_of_deleted_conversations(app: &App, conversation_ids: &[String], done: impl FnOnce(&mut App, Result<Vec<KeptWorktree>, String>) + Send + 'static) {
    if conversation_ids.is_empty() {
        app.post(move |app| done(app, Ok(Vec::new())));
        return;
    }
    app.invoke("release_worktrees_of_deleted_conversations", json!({ "conversationIds": conversation_ids }), done);
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn snapshot_with(worktree_id: &str) -> GitWorkspaceSnapshot {
        crate::testing::snapshot(worktree_id)
    }

    #[test]
    fn targets_serialize_the_way_the_host_reads_them() {
        assert_eq!(json!(git_conversation_target("c", None)), json!({"kind": "conversation", "conversationId": "c"}));
        assert_eq!(json!(git_conversation_target("c", Some(1))), json!({"kind": "conversation", "conversationId": "c"}));
        assert_eq!(json!(git_conversation_target("c", Some(2))), json!({"kind": "conversation", "conversationId": "c", "member": 2}));
        assert_eq!(json!(git_workspace_target("w", Some(3))), json!({"kind": "workspace", "workspaceId": "w", "member": 3}));
    }

    #[test]
    fn gives_each_target_kind_a_distinct_stable_key() {
        assert_eq!(git_target_key(&git_conversation_target("x", None)), "conversation:x");
        assert_eq!(git_target_key(&git_workspace_target("x", None)), "workspace:x");
        assert_eq!(git_target_key(&git_conversation_target("x", Some(2))), "conversation:x#2");
        assert_eq!(git_target_key(&git_workspace_target("x", Some(2))), "workspace:x#2");
    }

    #[test]
    fn surface_and_snapshot_keys_carry_the_member() {
        assert_eq!(git_surface_key("p", 1), "p");
        assert_eq!(git_surface_key("p", 2), "p#2");
        assert_eq!(git_surface_project_id("p#2"), "p");
        assert_eq!(git_snapshot_key("c", 3), "c#3");
        assert_eq!(git_snapshot_key_conversation("c#3"), "c");
    }

    #[test]
    fn two_checkouts_are_distinct_only_when_the_host_has_identified_both() {
        let (a, b) = (snapshot_with("worktree-a"), snapshot_with("worktree-b"));
        let r = |snapshot, isolated| GitCheckoutRef { snapshot, isolated };
        // Neither side asked for isolation: both address the root whatever an older snapshot says.
        assert!(!git_checkouts_are_distinct(r(Some(Some(&a)), false), r(Some(Some(&b)), false)));
        assert!(git_checkouts_are_distinct(r(Some(Some(&a)), true), r(Some(Some(&b)), false)));
        assert!(!git_checkouts_are_distinct(r(Some(Some(&a)), true), r(Some(Some(&a)), true)), "same identity: the worktree fell back to the root");
        assert!(!git_checkouts_are_distinct(r(None, true), r(Some(Some(&b)), false)), "never polled reads as the same");
        assert!(!git_checkouts_are_distinct(r(Some(None), true), r(Some(Some(&b)), false)), "not a repository reads as the same");
        let mut half = a.clone();
        half.repository_id.clear();
        assert!(!git_checkouts_are_distinct(r(Some(Some(&half)), true), r(Some(Some(&b)), false)), "an identity without both halves");
    }

    #[test]
    fn a_write_is_blocked_by_every_peer_that_may_be_working_in_the_same_checkout() {
        let (a, b) = (snapshot_with("worktree-a"), snapshot_with("worktree-b"));
        let acting = GitCheckoutRef { snapshot: Some(Some(&a)), isolated: true };
        let same = GitCheckoutRef { snapshot: Some(Some(&a)), isolated: false };
        let other = GitCheckoutRef { snapshot: Some(Some(&b)), isolated: true };
        assert!(git_peer_blocks_mutation(acting, other, false, true), "a lease always blocks");
        assert!(!git_peer_blocks_mutation(acting, same, false, false), "an idle peer never does");
        assert!(git_peer_blocks_mutation(acting, same, true, false));
        assert!(!git_peer_blocks_mutation(acting, other, true, false));
    }

    #[test]
    fn names_the_worktrees_kept_for_their_work_and_those_that_could_not_be_released() {
        assert_eq!(kept_worktrees_notice(&[]), None);
        let held = KeptWorktree { path: "/w/a".into(), error: None };
        assert!(kept_worktrees_notice(std::slice::from_ref(&held)).unwrap().contains("/w/a"));
        let two = [held.clone(), KeptWorktree { path: "/w/b".into(), error: None }];
        assert!(kept_worktrees_notice(&two).unwrap().starts_with("2 "));
        let failed = KeptWorktree { path: "/w/c".into(), error: Some("busy".into()) };
        let notice = kept_worktrees_notice(&[held, failed]).unwrap();
        assert!(notice.contains("/w/a") && notice.contains("/w/c") && notice.contains("busy"));
    }

    #[test]
    fn a_summary_becomes_a_snapshot_of_unknown_files() {
        let summary = crate::testing::summary("r1");
        let snapshot = summary_to_snapshot(summary.clone(), Vec::new());
        assert_eq!(snapshot.summary_revision, "r1");
        assert!(!snapshot.files_complete);
        assert_eq!(snapshot.changed_files, summary.changed_files);
    }
}
