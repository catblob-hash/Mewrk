//! The snapshots the rest of the app reads, and the lease that fences them against Git writes: upstream's
//! `lib/gitController.ts`.

use std::collections::{HashMap, HashSet};

use mewrk_client::prelude::*;

use crate::git::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSnapshotEntry {
    pub workspace_id: String,
    pub snapshot: Option<GitWorkspaceSnapshot>,
}

pub type GitSnapshots = HashMap<String, GitSnapshotEntry>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitSnapshotRefreshResult {
    Resolved(Option<GitWorkspaceSnapshot>),
    Unchanged(String),
    Failed,
}

/// `None` when the entry is another workspace's, `Some(None)` when the directory is not a repository.
pub fn git_snapshot_for_workspace<'a>(entry: Option<&'a GitSnapshotEntry>, workspace_id: Option<&str>) -> Option<Option<&'a GitWorkspaceSnapshot>> {
    let entry = entry?;
    (Some(entry.workspace_id.as_str()) == workspace_id).then_some(entry.snapshot.as_ref())
}

/// Answers whether anything changed.
pub fn git_snapshots_after_refresh(current: &mut GitSnapshots, conversation_id: &str, workspace_id: &str, result: GitSnapshotRefreshResult) -> bool {
    let GitSnapshotRefreshResult::Resolved(snapshot) = result else { return false };
    let next = GitSnapshotEntry { workspace_id: workspace_id.to_owned(), snapshot };
    if current.get(conversation_id) == Some(&next) {
        return false;
    }
    current.insert(conversation_id.to_owned(), next);
    true
}

pub fn git_snapshot_refresh_result_from_summary(result: GitWorkspaceSummaryResult) -> GitSnapshotRefreshResult {
    match result {
        GitWorkspaceSummaryResult::NotRepository => GitSnapshotRefreshResult::Resolved(None),
        GitWorkspaceSummaryResult::Unchanged { revision } => GitSnapshotRefreshResult::Unchanged(revision),
        GitWorkspaceSummaryResult::Snapshot { summary } => GitSnapshotRefreshResult::Resolved(Some(summary_to_snapshot(summary, Vec::new()))),
    }
}

pub fn git_snapshots_after_workspace_mutation(current: &mut GitSnapshots, conversation_ids: &[String], workspace_id: &str, snapshot: Option<&GitWorkspaceSnapshot>) {
    for id in conversation_ids {
        current.insert(id.clone(), GitSnapshotEntry { workspace_id: workspace_id.to_owned(), snapshot: snapshot.cloned() });
    }
}

/// Moves a draft's snapshots, one per project workspace, to the real conversation's keys on redemption. Both keys refer
/// to the same checkout, and moving the entries keeps the status card from disappearing until the next poll.
pub fn git_snapshots_after_draft_redemption(current: &mut GitSnapshots, draft_conversation_id: &str, conversation_id: &str) -> bool {
    let moved: Vec<String> = current.keys().filter(|key| git_snapshot_key_conversation(key) == draft_conversation_id).cloned().collect();
    for key in &moved {
        if let Some(entry) = current.remove(key) {
            current.insert(format!("{conversation_id}{}", &key[draft_conversation_id.len()..]), entry);
        }
    }
    !moved.is_empty()
}

/// Selects recipients for a snapshot returned by a write operation. Share only conversations running against the same
/// checkout, not merely the same workspace: an isolated worktree has different content and its own Git actions. A
/// recorded worktree is a conservative criterion.
pub fn git_snapshot_broadcast_ids(conversations: &[(String, bool)], acting_conversation_id: &str, acting_runs_in_own_worktree: bool) -> Vec<String> {
    if acting_runs_in_own_worktree {
        return vec![acting_conversation_id.to_owned()];
    }
    let mut shared: Vec<String> = conversations.iter().filter(|(_, has_worktree)| !has_worktree).map(|(id, _)| id.clone()).collect();
    // Drafts are absent from workspace conversation lists but read and write the workspace root.
    if !shared.iter().any(|id| id == acting_conversation_id) {
        shared.push(acting_conversation_id.to_owned());
    }
    shared
}

/// What a review panel is keyed by: everything whose change makes the old pages and patches meaningless.
pub fn git_review_snapshot_cache_key(snapshot: &GitWorkspaceSnapshot) -> String {
    json!([
        snapshot.repository_id,
        snapshot.worktree_id,
        snapshot.repository_root,
        snapshot.worktree_root,
        snapshot.branch.as_deref().unwrap_or(""),
        snapshot.head.as_deref().unwrap_or(""),
        snapshot.upstream_target.as_ref().map(|u| json!([u.remote_name, u.remote_branch, u.merge_ref, u.tracking_ref, u.tracking_oid, u.is_local, u.remote.fetch_revision, u.remote.push_revision])),
        snapshot.remote.as_ref().map(|r| json!([r.name, r.fetch_revision, r.push_revision])),
        snapshot.remotes.iter().map(|r| json!([r.name, r.fetch_revision, r.push_revision])).collect::<Vec<_>>(),
    ])
    .to_string()
}

/// Whether the review pane lists `path`, a tracked file with a change, in the checkout `snapshot` describes. A summary
/// snapshot carries no file list, so the host is asked for that one path by name; a summary gone stale in the meantime
/// is answered with the current one, which is asked once more. `base` is a worktree's fork point.
pub fn git_review_lists_path(task: &Task, target: &GitTarget, snapshot: &GitWorkspaceSnapshot, path: &str, base: Option<&str>) -> Result<bool, String> {
    if snapshot.summary_revision.is_empty() || (base.is_none() && snapshot.files_complete) {
        return Ok(snapshot.files.iter().any(|file| file.path == path && !file.untracked && file.status != GitFileStatus::Untracked));
    }
    let mut revision = snapshot.summary_revision.clone();
    for _ in 0..2 {
        let request = GitChangePageRequest { expected_revision: revision.clone(), cursor: None, query: None, limit: 1, selected_path: Some(path.to_owned()), base: base.map(str::to_owned) };
        let result: GitChangePageResult = task.invoke("get_git_change_page", json!({ "target": target, "request": request }))?;
        match result {
            GitChangePageResult::Page { selection, .. } => return Ok(matches!(selection, Some(GitChangeSelection::Present { .. }))),
            GitChangePageResult::Stale { summary } => revision = summary.summary_revision,
        }
    }
    Ok(false)
}

#[derive(Default)]
pub struct GitController {
    pub snapshots: GitSnapshots,
    /// Conversations currently holding a Git mutation lease.
    mutation_conversation_ids: HashSet<String>,
    refresh_tokens: HashMap<String, u64>,
    /// Bumped on every change: what a subscriber would hear.
    pub revision: u64,
}

impl GitController {
    pub fn update_snapshots(&mut self, update: impl FnOnce(&mut GitSnapshots) -> bool) {
        if update(&mut self.snapshots) {
            self.revision += 1;
        }
    }

    pub fn mutation_is_active(&self, conversation_id: &str) -> bool {
        self.mutation_conversation_ids.contains(conversation_id)
    }

    /// Grants the mutation lease and invalidates every in-flight poll for the workspace: a poll that started before the
    /// lease was acquired must never overwrite the mutation result with its older repository snapshot.
    pub fn acquire_mutation_lease(&mut self, conversation_id: &str, workspace_conversation_ids: &[String]) {
        // Every workspace of every conversation named: a poll of any of them started before the lease must not land
        // after the write.
        for (key, token) in &mut self.refresh_tokens {
            if workspace_conversation_ids.iter().any(|id| id == git_snapshot_key_conversation(key)) {
                *token += 1;
            }
        }
        for id in workspace_conversation_ids {
            self.refresh_tokens.entry(id.clone()).or_insert(1);
        }
        if self.mutation_conversation_ids.insert(conversation_id.to_owned()) {
            self.revision += 1;
        }
    }

    pub fn release_mutation_lease(&mut self, conversation_id: &str) {
        if self.mutation_conversation_ids.remove(conversation_id) {
            self.revision += 1;
        }
    }

    /// Takes the next refresh token for a key: last-token-wins, and a lease bumps it too.
    fn begin_refresh(&mut self, conversation_id: &str) -> u64 {
        let token = self.refresh_tokens.entry(conversation_id.to_owned()).or_insert(0);
        *token += 1;
        *token
    }

    /// Commits what a refresh found, unless a newer one or a lease got there first. Answers what the caller is told.
    fn finish_refresh(&mut self, conversation_id: &str, workspace_id: &str, token: u64, result: GitWorkspaceSummaryResult) -> Option<Option<GitWorkspaceSnapshot>> {
        let refresh = git_snapshot_refresh_result_from_summary(result);
        if self.refresh_tokens.get(conversation_id) == Some(&token) && git_snapshots_after_refresh(&mut self.snapshots, conversation_id, workspace_id, refresh.clone()) {
            self.revision += 1;
        }
        match refresh {
            GitSnapshotRefreshResult::Resolved(snapshot) => Some(snapshot),
            _ => git_snapshot_for_workspace(self.snapshots.get(conversation_id), Some(workspace_id)).map(|s| s.cloned()),
        }
    }

    /// True when the failure belongs to the newest refresh of its key, which is the only one worth reporting.
    fn refresh_is_current(&self, conversation_id: &str, token: u64) -> bool {
        self.refresh_tokens.get(conversation_id) == Some(&token)
    }
}

/// The app's one controller.
pub fn git_controller(app: &mut App) -> &mut GitController {
    app.local::<GitController>()
}

/// Polls the workspace summary. A newer refresh of the same key, or a mutation lease for its conversation, invalidates
/// this poll's commit. `conversation_id` is the cache key, a `git_snapshot_key`, and `target` is the addressing: for a
/// real conversation the two say the same thing, while the draft caches under its own renderer-only key while asking
/// about the workspace it has selected.
///
/// `done` gets `Ok(Some(snapshot))` for an answer, `Ok(None)` when nothing is known of this workspace, and `Err` for a
/// failure of the newest read; an older read's failure is `Ok(None)` too.
pub fn git_refresh(app: &mut App, conversation_id: &str, workspace_id: &str, target: &GitTarget, done: impl FnOnce(&mut App, Result<Option<Option<GitWorkspaceSnapshot>>, String>) + Send + 'static) {
    let controller = git_controller(app);
    let token = controller.begin_refresh(conversation_id);
    let known = git_snapshot_for_workspace(controller.snapshots.get(conversation_id), Some(workspace_id)).flatten().map(|s| s.summary_revision.clone());
    let (key, workspace) = (conversation_id.to_owned(), workspace_id.to_owned());
    get_git_workspace_summary(app, target, known.as_deref(), move |app, result| match result {
        Ok(summary) => {
            let snapshot = git_controller(app).finish_refresh(&key, &workspace, token, summary);
            repaint();
            done(app, Ok(snapshot));
        }
        Err(message) => {
            let current = git_controller(app).refresh_is_current(&key, token);
            done(app, if current { Err(message) } else { Ok(None) });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{snapshot, summary};

    #[test]
    fn snapshot_for_workspace_only_matches_the_entrys_workspace() {
        let entry = GitSnapshotEntry { workspace_id: "w1".into(), snapshot: Some(snapshot("t")) };
        assert_eq!(git_snapshot_for_workspace(Some(&entry), Some("w2")), None);
        assert_eq!(git_snapshot_for_workspace(None, Some("w1")), None);
        assert!(git_snapshot_for_workspace(Some(&entry), Some("w1")).unwrap().is_some());
        let not_repo = GitSnapshotEntry { workspace_id: "w1".into(), snapshot: None };
        assert_eq!(git_snapshot_for_workspace(Some(&not_repo), Some("w1")), Some(None));
    }

    #[test]
    fn refresh_changes_nothing_for_failed_unchanged_and_identical_results() {
        let mut map = GitSnapshots::new();
        let s = snapshot("t");
        assert!(git_snapshots_after_refresh(&mut map, "c", "w", GitSnapshotRefreshResult::Resolved(Some(s.clone()))));
        assert!(!git_snapshots_after_refresh(&mut map, "c", "w", GitSnapshotRefreshResult::Failed));
        assert!(!git_snapshots_after_refresh(&mut map, "c", "w", GitSnapshotRefreshResult::Unchanged("r".into())));
        assert!(!git_snapshots_after_refresh(&mut map, "c", "w", GitSnapshotRefreshResult::Resolved(Some(s))));
        assert!(git_snapshots_after_refresh(&mut map, "c", "w", GitSnapshotRefreshResult::Resolved(None)));
    }

    #[test]
    fn a_workspace_mutation_stamps_every_conversation() {
        let mut map = GitSnapshots::new();
        let s = snapshot("t");
        git_snapshots_after_workspace_mutation(&mut map, &["a".into(), "b".into()], "w", Some(&s));
        assert_eq!(map["a"].snapshot.as_ref(), Some(&s));
        assert_eq!(map["b"].workspace_id, "w");
    }

    #[test]
    fn broadcast_keeps_a_root_mutation_off_worktrees_includes_the_draft_and_a_worktree_mutation_stays_home() {
        let list = [("a".to_owned(), false), ("b".to_owned(), true), ("c".to_owned(), false)];
        assert_eq!(git_snapshot_broadcast_ids(&list, "a", false), ["a", "c"]);
        assert_eq!(git_snapshot_broadcast_ids(&list, "draft", false), ["a", "c", "draft"]);
        assert_eq!(git_snapshot_broadcast_ids(&list, "b", true), ["b"]);
    }

    #[test]
    fn redemption_moves_the_drafts_entries_and_keeps_the_rest() {
        let mut map = GitSnapshots::new();
        let entry = GitSnapshotEntry { workspace_id: "w".into(), snapshot: None };
        map.insert("draft".into(), entry.clone());
        map.insert("draft#2".into(), entry.clone());
        map.insert("other".into(), entry);
        assert!(git_snapshots_after_draft_redemption(&mut map, "draft", "real"));
        assert!(map.contains_key("real") && map.contains_key("real#2") && map.contains_key("other"));
        assert!(!map.contains_key("draft"));
        assert!(!git_snapshots_after_draft_redemption(&mut map, "draft", "real"));
    }

    #[test]
    fn refresh_result_maps_the_three_summary_kinds() {
        assert_eq!(git_snapshot_refresh_result_from_summary(GitWorkspaceSummaryResult::NotRepository), GitSnapshotRefreshResult::Resolved(None));
        assert_eq!(git_snapshot_refresh_result_from_summary(GitWorkspaceSummaryResult::Unchanged { revision: "r".into() }), GitSnapshotRefreshResult::Unchanged("r".into()));
        assert!(matches!(git_snapshot_refresh_result_from_summary(GitWorkspaceSummaryResult::Snapshot { summary: summary("r") }), GitSnapshotRefreshResult::Resolved(Some(_))));
    }

    #[test]
    fn tracks_mutation_leases_and_notifies() {
        let mut c = GitController::default();
        c.acquire_mutation_lease("c", &["c".to_owned()]);
        assert!(c.mutation_is_active("c"));
        assert_eq!(c.revision, 1);
        c.acquire_mutation_lease("c", &["c".to_owned()]);
        assert_eq!(c.revision, 1, "holding it already changes nothing");
        c.release_mutation_lease("c");
        assert!(!c.mutation_is_active("c"));
        assert_eq!(c.revision, 2);
        c.release_mutation_lease("c");
        assert_eq!(c.revision, 2);
    }

    #[test]
    fn a_lease_drops_a_refresh_that_started_before_it_even_for_another_workspace_of_the_conversation() {
        let mut c = GitController::default();
        let key = git_snapshot_key("c", 2);
        let token = c.begin_refresh(&key);
        c.acquire_mutation_lease("c", &["c".to_owned()]);
        c.finish_refresh(&key, "w", token, GitWorkspaceSummaryResult::Snapshot { summary: summary("old") });
        assert!(c.snapshots.is_empty(), "an older poll must not land after the write");
        assert!(!c.refresh_is_current(&key, token));
    }

    #[test]
    fn a_newer_refresh_wins_and_only_its_failure_counts() {
        let mut c = GitController::default();
        let first = c.begin_refresh("c");
        let second = c.begin_refresh("c");
        assert!(!c.refresh_is_current("c", first));
        assert!(c.refresh_is_current("c", second));
        c.finish_refresh("c", "w", first, GitWorkspaceSummaryResult::Snapshot { summary: summary("first") });
        assert!(c.snapshots.is_empty());
        c.finish_refresh("c", "w", second, GitWorkspaceSummaryResult::Snapshot { summary: summary("second") });
        assert_eq!(c.snapshots["c"].snapshot.as_ref().unwrap().summary_revision, "second");
        // Unchanged reports what is known.
        let third = c.begin_refresh("c");
        let told = c.finish_refresh("c", "w", third, GitWorkspaceSummaryResult::Unchanged { revision: "second".into() });
        assert_eq!(told.unwrap().unwrap().summary_revision, "second");
    }

    #[test]
    fn the_review_cache_key_moves_with_the_checkout_and_not_with_its_content() {
        let a = snapshot("t");
        let mut same = a.clone();
        same.additions += 3;
        same.content_revision = "other".into();
        assert_eq!(git_review_snapshot_cache_key(&a), git_review_snapshot_cache_key(&same));
        let mut moved = a.clone();
        moved.head = Some("ffffff".into());
        assert_ne!(git_review_snapshot_cache_key(&a), git_review_snapshot_cache_key(&moved));
    }
}
