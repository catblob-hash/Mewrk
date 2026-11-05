//! What the crate's tests share: a snapshot to change a field of.
#![cfg(test)]

use crate::git::*;

pub(crate) fn snapshot(worktree_id: &str) -> GitWorkspaceSnapshot {
    summary_to_snapshot(GitWorkspaceSummary { worktree_id: worktree_id.to_owned(), ..summary("summary-1") }, Vec::new())
}

pub(crate) fn summary(revision: &str) -> GitWorkspaceSummary {
    GitWorkspaceSummary {
        repository_id: "repository-id-1".into(),
        worktree_id: "worktree-id-1".into(),
        branch: Some("feature/review".into()),
        head: Some("12ab34cd".into()),
        content_revision: "revision-1".into(),
        summary_revision: revision.into(),
        upstream: None,
        upstream_target: None,
        ahead: 0,
        behind: 0,
        additions: 7,
        deletions: 2,
        staged: 1,
        unstaged: 1,
        untracked: 0,
        conflicted: 0,
        stash: 0,
        changed_files: 1,
        stageable: 1,
        unstageable: 1,
        remote: None,
        remotes: Vec::new(),
        git_version: "git version 2.50.0".into(),
        repository_root: "/repo".into(),
        worktree_root: "/repo".into(),
        detached: false,
        unborn: false,
        operation: None,
        operation_revision: None,
        is_clean: false,
        binary_files: 0,
        warnings: Vec::new(),
    }
}

pub(crate) fn file(path: &str) -> GitFileChange {
    GitFileChange { path: path.into(), original_path: None, status: GitFileStatus::Modified, index_status: String::new(), worktree_status: String::new(), staged: false, unstaged: true, untracked: false, conflicted: false, additions: Some(1), deletions: Some(0), binary: false, submodule: false, submodule_commit_changed: false, submodule_modified: false, submodule_untracked: false }
}
