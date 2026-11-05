//! Upstream's `GitReviewPanel.test.tsx`, on the state machine: every effect that goes out is an answer the test can hold
//! back, deliver late or deliver out of order.

use super::*;
use crate::git::git_conversation_target;
use crate::testing::{file, summary};

fn target() -> GitTarget {
    git_conversation_target("conversation-1", None)
}

fn snapshot() -> GitWorkspaceSnapshot {
    // Upstream's fixture: a complete snapshot with one file carrying both a staged and an unstaged change.
    let mut f = file("src/App.tsx");
    f.staged = true;
    f.additions = Some(7);
    f.deletions = Some(2);
    GitWorkspaceSnapshot { files: vec![f], files_complete: true, summary_revision: String::new(), staged: 1, unstaged: 1, ..summary_to_snapshot(summary(""), Vec::new()) }
}

fn inputs(snapshot: GitWorkspaceSnapshot) -> ReviewInputs {
    ReviewInputs { target: target(), snapshot, active: true, checkout: GitReviewCheckout::default(), reveal_request: None, mutation_disabled_reason: None }
}

fn core(i: &ReviewInputs) -> ReviewCore {
    ReviewCore::new(i, true, DiffStyle::Unified, true, true, false)
}

fn patch(path: &str) -> String {
    format!("diff --git a/{path} b/{path}\nindex 1111111..2222222 100644\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old {path}\n+new {path}\n")
}

fn response(text: &str) -> GitDiffResponse {
    GitDiffResponse { path: None, patch: text.to_owned(), truncated: false, additions: 1, deletions: 1, binary: false, files: Vec::new() }
}

fn scope_read(effects: &[Effect]) -> Option<(u64, GitDiffRequest)> {
    effects.iter().find_map(|e| if let Effect::LoadScopeDiff { request_id, request } = e { Some((*request_id, request.clone())) } else { None })
}

fn page_read(effects: &[Effect]) -> Option<(u64, String, Option<String>, GitChangePageRequest)> {
    effects.iter().find_map(|e| if let Effect::LoadPage { request_id, scope_key, cursor, request } = e { Some((*request_id, scope_key.clone(), cursor.clone(), request.clone())) } else { None })
}

fn paged(paths: &[String], revision: &str) -> (GitWorkspaceSnapshot, Vec<GitFileChange>) {
    let files: Vec<GitFileChange> = paths.iter().map(|p| { let mut f = file(p); f.staged = true; f.unstaged = false; f }).collect();
    let s = GitWorkspaceSnapshot { content_revision: format!("large-changes-{}", paths.len()), summary_revision: revision.to_owned(), additions: paths.len() as u64, deletions: 0, staged: paths.len() as u32, unstaged: 0, changed_files: paths.len() as u32, stageable: 0, unstageable: paths.len() as u32, files_complete: false, files: Vec::new(), ..snapshot() };
    (s, files)
}

fn page(revision: &str, files: Vec<GitFileChange>, matched: u32, next: Option<&str>, selection: Option<GitChangeSelection>) -> GitChangePageResult {
    GitChangePageResult::Page { revision: revision.to_owned(), files, matched_count: matched, next_cursor: next.map(str::to_owned), selection }
}

fn labels(c: &ReviewCore, path: &str) -> Vec<String> {
    c.file_actions(path).into_iter().map(|a| a.label).collect()
}

/// Runs what a publish would have the parent do: feeds the snapshot back in as the prop.
fn published(effects: &[Effect]) -> Vec<Option<GitWorkspaceSnapshot>> {
    effects.iter().filter_map(|e| if let Effect::Publish(s) = e { Some(s.clone()) } else { None }).collect()
}

fn discard_request(effects: &[Effect]) -> (u64, String, String, Option<PendingDiscard>) {
    effects.iter().find_map(|e| if let Effect::PrepareDiscard { request_id, scope_key, snapshot_key, confirmation, .. } = e { Some((*request_id, scope_key.clone(), snapshot_key.clone(), confirmation.clone())) } else { None }).expect("a preparation")
}

fn prepared(s: &GitWorkspaceSnapshot, target_revision: &str) -> Result<GitDiscardPreparation, String> {
    Ok(GitDiscardPreparation { snapshot: s.clone(), target_revision: target_revision.to_owned() })
}

const PATH: &str = "src/App.tsx";

#[test]
fn keeps_mounted_inactive_pages_idle_and_loads_the_selected_diff_when_activated() {
    let mut i = inputs(snapshot());
    i.active = false;
    let mut c = core(&i);
    assert!(scope_read(&c.sync(&i)).is_none());
    i.active = true;
    let effects = c.sync(&i);
    // The scope is read as one patch, not one file at a time.
    assert_eq!(scope_read(&effects).unwrap().1, GitDiffRequest::Working { path: None, context: None });
}

#[test]
fn rereads_the_scope_when_it_changes() {
    let i = inputs(snapshot());
    let mut c = core(&i);
    c.sync(&i);
    c.diff_scope = DiffScope::Staged;
    assert_eq!(scope_read(&c.sync(&i)).unwrap().1, GitDiffRequest::Staged { path: None, context: None });
    c.diff_scope = DiffScope::Unstaged;
    assert_eq!(scope_read(&c.sync(&i)).unwrap().1, GitDiffRequest::Unstaged { path: None, context: None });
    assert!(scope_read(&c.sync(&i)).is_none(), "nothing changed: nothing is read again");
}

#[test]
fn names_a_worktree_and_opens_on_everything_its_branch_did_since_it_forked() {
    let mut s = snapshot();
    s.summary_revision = "summary-1".into();
    s.files_complete = false;
    s.files.clear();
    s.changed_files = 1;
    let mut i = inputs(s);
    i.checkout = GitReviewCheckout { worktree_name: Some("task-1".into()), base_branch: Some("main".into()), base_oid: Some("forkpoint".into()) };
    let mut c = core(&i);
    assert_eq!(c.branch_label(), "main");
    assert_eq!(c.scope_note(), None, "the scope the page opens on needs no word");
    let effects = c.sync(&i);
    let (_, request) = scope_read(&effects).unwrap();
    assert_eq!(request, GitDiffRequest::Branch { base: "forkpoint".into(), path: None, context: None });
    let (id, scope, _, page_request) = page_read(&effects).unwrap();
    assert_eq!(page_request.base.as_deref(), Some("forkpoint"), "a branch's listing is always the host's");

    // A branch's size is learned from its listing.
    assert_eq!(c.total_changed_files(), 1);
    c.page_answer(id, &scope, None, Ok(page("summary-1", vec![file("a.ts"), file("b.ts")], 2, None, None)));
    assert_eq!(c.total_changed_files(), 2);

    c.diff_scope = DiffScope::Working;
    assert_eq!(c.scope_note(), Some("未提交"));
    let effects = c.sync(&i);
    assert_eq!(scope_read(&effects).unwrap().1, GitDiffRequest::Working { path: None, context: None });
}

#[test]
fn lists_every_tracked_change_once_leaves_untracked_files_out_and_offers_staging_actions() {
    let mut s = snapshot();
    let mut staged = file("src/staged.ts");
    staged.staged = true;
    staged.unstaged = false;
    let unstaged = file("src/unstaged.ts");
    let mut new = file("src/new.ts");
    new.untracked = true;
    new.status = GitFileStatus::Untracked;
    s.files = vec![staged, unstaged, new];
    s.untracked = 1;
    let i = inputs(s);
    let mut c = core(&i);
    c.sync(&i);
    assert_eq!(c.change_files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["src/staged.ts", "src/unstaged.ts"]);
    assert_eq!(c.change_matched_count, 2);
    // A staged-only file can be unstaged but not staged again, and an unstaged one the other way round.
    assert!(labels(&c, "src/staged.ts").contains(&"取消暂存".to_owned()));
    assert!(!labels(&c, "src/staged.ts").contains(&"暂存".to_owned()));
    assert!(labels(&c, "src/unstaged.ts").contains(&"暂存".to_owned()));
}

#[test]
fn says_who_changed_how_in_the_columns_rows() {
    let mut s = snapshot();
    let mut renamed = file("b.ts");
    renamed.status = GitFileStatus::Renamed;
    renamed.original_path = Some("a.ts".into());
    let mut conflicted = file("c.ts");
    conflicted.conflicted = true;
    conflicted.status = GitFileStatus::Unmerged;
    let mut sub = file("vendor/m");
    sub.submodule = true;
    s.files = vec![s.files[0].clone(), renamed, conflicted, sub];
    let i = inputs(s);
    let mut c = core(&i);
    c.sync(&i);
    let entries = c.diff_entries();
    assert_eq!(entries[0].note.as_deref(), Some("已暂存 + 未暂存"));
    assert_eq!((entries[1].status, entries[1].old_path.as_deref()), (DiffEntryStatus::Renamed, Some("a.ts")));
    assert_eq!(entries[2].status, DiffEntryStatus::Conflicted);
    assert_eq!(entries[3].unavailable.as_deref(), Some("子模块变更没有行级差异"));
}

#[test]
fn a_file_with_no_patch_is_asked_for_once_and_its_answer_is_final() {
    let i = inputs(snapshot());
    let mut c = core(&i);
    let (id, _) = scope_read(&c.sync(&i)).unwrap();
    assert!(c.request_file_patch(PATH, None).is_none(), "until the scope-wide read answers, every file looks missing");
    c.scope_diff_answer(id, Ok(response("")));
    let Some(Effect::LoadFilePatch { request_id, step, request, .. }) = c.request_file_patch(PATH, None) else { panic!() };
    assert_eq!(request, GitDiffRequest::Working { path: Some(PATH.into()), context: None });
    assert_eq!(c.diff_entries()[0].loading, true);
    assert!(c.request_file_patch(PATH, None).is_none(), "asking twice for one path is free");
    // An answer carrying no patch is still an answer: the viewer does not ask again.
    c.file_patch_answer(PATH, request_id, step, Ok(response("")));
    assert_eq!(c.diff_entries()[0].unavailable.as_deref(), Some("这个范围没有行级差异"));
    assert!(c.request_file_patch(PATH, None).is_none());
}

#[test]
fn several_patches_can_be_in_flight_and_only_a_newer_read_of_the_same_file_fences() {
    let i = inputs(snapshot());
    let mut c = core(&i);
    let (id, _) = scope_read(&c.sync(&i)).unwrap();
    c.scope_diff_answer(id, Ok(response("")));
    let Some(Effect::LoadFilePatch { request_id: first, .. }) = c.request_file_patch("a.ts", None) else { panic!() };
    let Some(Effect::LoadFilePatch { request_id: other, .. }) = c.request_file_patch("b.ts", None) else { panic!() };
    let Some(Effect::LoadFilePatch { request_id: second, step, request, .. }) = c.request_file_patch("a.ts", Some(0)) else { panic!() };
    assert_eq!(request, GitDiffRequest::Working { path: Some("a.ts".into()), context: Some(25) });
    c.file_patch_answer("b.ts", other, None, Ok(response(&patch("b.ts"))));
    assert!(c.file_patches.contains_key("b.ts"), "the slow one's answer is not stale just for being late");
    c.file_patch_answer("a.ts", first, None, Ok(response(&patch("a.ts"))));
    assert!(!c.file_patches.contains_key("a.ts"), "the older read of the same file is");
    c.file_patch_answer("a.ts", second, step, Ok(response(&patch("a.ts"))));
    assert_eq!(c.context_steps.get("a.ts"), Some(&0));
}

#[test]
fn context_widens_a_step_at_a_time_and_stops_at_the_whole_file() {
    let i = inputs(snapshot());
    let mut c = core(&i);
    let (id, _) = scope_read(&c.sync(&i)).unwrap();
    c.scope_diff_answer(id, Ok(response(&patch(PATH))));
    for (step, context) in DIFF_CONTEXT_STEPS.iter().enumerate() {
        let Some(Effect::LoadFilePatch { request_id, step: s, request, .. }) = c.expand_file_context(PATH) else { panic!("step {step}") };
        assert_eq!(request, GitDiffRequest::Working { path: Some(PATH.into()), context: Some(*context) });
        assert!(c.expanding_paths().contains(PATH));
        c.file_patch_answer(PATH, request_id, s, Ok(response(&patch(PATH))));
        assert!(!c.expanding_paths().contains(PATH));
    }
    assert!(c.fully_expanded_paths().contains(PATH));
    assert!(c.expand_file_context(PATH).is_none());
    // A file fetched on its own wins over the scope-wide read.
    let merged = c.diff_patches();
    assert!(Arc::ptr_eq(&merged[PATH], &c.file_patches[PATH]));
}

#[test]
fn reloads_an_open_diff_when_content_changes_even_if_all_line_counts_stay_equal() {
    let mut i = inputs(snapshot());
    let mut c = core(&i);
    let (id, _) = scope_read(&c.sync(&i)).unwrap();
    c.scope_diff_answer(id, Ok(response(&patch(PATH))));
    assert!(scope_read(&c.sync(&i)).is_none());
    i.snapshot.content_revision = "revision-2".into();
    let (id, _) = scope_read(&c.sync(&i)).expect("the content moved");
    // What was fetched on its own belonged to the previous revision.
    c.file_patches.insert("old.ts".into(), Arc::new(parse_unified_diff(&patch("old.ts"), None).remove(0)));
    c.scope_diff_answer(id, Ok(response(&patch(PATH))));
    assert!(c.file_patches.is_empty());
}

#[test]
fn loads_changed_files_from_the_backend_in_200_row_pages() {
    let paths: Vec<String> = (0..450).map(|i| format!("src/bulk/file-{i:03}.ts")).collect();
    let (s, files) = paged(&paths, "summary-large");
    let i = inputs(s);
    let mut c = core(&i);
    let effects = c.sync(&i);
    let (id, scope, _, request) = page_read(&effects).unwrap();
    assert_eq!((request.expected_revision.as_str(), request.limit, request.cursor.is_none(), request.selected_path.is_none()), ("summary-large", 200, true, true));
    assert!(scope_read(&effects).is_none(), "450 files are read one at a time");
    c.page_answer(id, &scope, None, Ok(page("summary-large", files[..200].to_vec(), 450, Some("page-2"), None)));
    assert_eq!((c.change_files.len(), c.change_matched_count, c.next_change_page_count()), (200, 450, 200));
    assert_eq!(c.selected_path.as_deref(), Some(paths[0].as_str()));

    let mut effects = Vec::new();
    c.load_change_page(c.change_next_cursor.clone(), &mut effects);
    assert!(c.change_loading_more);
    let (id, scope, cursor, request) = page_read(&effects).unwrap();
    assert_eq!((request.cursor.as_deref(), request.limit, request.selected_path.as_deref()), (Some("page-2"), 200, Some(paths[0].as_str())));
    c.page_answer(id, &scope, cursor.as_deref(), Ok(page("summary-large", files[200..400].to_vec(), 450, Some("page-3"), Some(GitChangeSelection::Present { file: files[0].clone() }))));
    assert_eq!(c.change_files.len(), 400);
    assert_eq!(c.next_change_page_count(), 50);
    assert!(!c.change_loading_more);
}

#[test]
fn opens_a_file_the_timeline_asked_for_with_the_column_folded_for_that_visit() {
    let paths: Vec<String> = (0..300).map(|i| format!("f{i:03}.ts")).collect();
    let (s, files) = paged(&paths, "r1");
    let wanted = paths[250].clone();
    let mut i = inputs(s);
    i.reveal_request = Some(GitReviewRevealRequest { path: wanted.clone(), nonce: 1, collapse_tree: true });
    let mut c = core(&i);
    let effects = c.sync(&i);
    assert!(effects.iter().any(|e| matches!(e, Effect::RevealHandled(1))));
    assert!(!c.show_diff_files, "folded for this visit");
    let (id, scope, _, request) = page_read(&effects).unwrap();
    assert_eq!(request.selected_path.as_deref(), Some(wanted.as_str()), "a pane mounted to show one file asks for it with its first page");
    // Listed even though it sorts past the page: opening it is the whole point of the click.
    c.page_answer(id, &scope, None, Ok(page("r1", files[..200].to_vec(), 300, Some("next"), Some(GitChangeSelection::Present { file: files[250].clone() }))));
    assert_eq!(c.change_files.len(), 201);
    let effects = c.sync(&i);
    assert_eq!(c.diff_reveal.as_ref().map(|r| r.0.as_str()), Some(wanted.as_str()));
    assert!(page_read(&effects).is_none());
    // The later page holding it does not list it twice.
    let mut more = Vec::new();
    c.load_change_page(c.change_next_cursor.clone(), &mut more);
    let (id, scope, cursor, _) = page_read(&more).unwrap();
    c.page_answer(id, &scope, cursor.as_deref(), Ok(page("r1", files[200..300].to_vec(), 300, None, None)));
    assert_eq!(c.change_files.iter().filter(|f| f.path == wanted).count(), 1);
}

#[test]
fn a_reveal_rereads_when_the_file_is_on_no_page_read_so_far_and_gives_way_to_all_changes() {
    let paths: Vec<String> = (0..3).map(|i| format!("f{i}.ts")).collect();
    let (s, files) = paged(&paths, "r1");
    let mut i = inputs(s);
    let mut c = core(&i);
    let (id, scope, _, _) = page_read(&c.sync(&i)).unwrap();
    c.page_answer(id, &scope, None, Ok(page("r1", files[..2].to_vec(), 3, None, None)));
    c.sync(&i);

    // Asked while the pane is open: its column stays as it was.
    i.reveal_request = Some(GitReviewRevealRequest { path: "f2.ts".into(), nonce: 7, collapse_tree: false });
    c.diff_scope = DiffScope::Unstaged;
    let effects = c.sync(&i);
    assert!(c.show_diff_files);
    let (id, scope, _, request) = page_read(&effects).expect("the first page once more, with it as the selection");
    assert_eq!(request.selected_path.as_deref(), Some("f2.ts"));
    let mut wanted = files[2].clone();
    wanted.unstaged = false;
    c.page_answer(id, &scope, None, Ok(page("r1", files[..2].to_vec(), 3, None, Some(GitChangeSelection::Present { file: wanted }))));
    c.sync(&i);
    assert_eq!(c.diff_scope, DiffScope::Working, "a scope that does not hold it gives way");
    assert_eq!(c.diff_reveal.as_ref().map(|r| r.1), Some(7));
    // Asking twice for the same request asks once.
    c.diff_reveal = None;
    c.sync(&i);
    assert_eq!(c.diff_reveal, None);
}

#[test]
fn a_filter_resets_pagination_and_clearing_it_for_a_reveal() {
    let paths: Vec<String> = (0..3).map(|i| format!("f{i}.ts")).collect();
    let (s, files) = paged(&paths, "r1");
    let i = inputs(s);
    let mut c = core(&i);
    let (id, scope, _, _) = page_read(&c.sync(&i)).unwrap();
    c.page_answer(id, &scope, None, Ok(page("r1", files.clone(), 3, Some("next"), None)));
    c.change_filter = " F1 ".into();
    let (id, scope, cursor, request) = page_read(&c.sync(&i)).expect("a new first page");
    assert_eq!((request.query.as_deref(), cursor), (Some("f1"), None), "trimmed and lowered");
    assert!(c.change_files.is_empty());
    assert_eq!(c.change_next_cursor, None);
    c.page_answer(id, &scope, None, Ok(page("r1", vec![files[1].clone()], 1, None, Some(GitChangeSelection::Missing))));
    assert_eq!(c.change_files.len(), 1);
}

#[test]
fn ignores_a_late_change_page_from_an_older_summary_revision() {
    let paths: Vec<String> = (0..2).map(|i| format!("f{i}.ts")).collect();
    let (s, files) = paged(&paths, "r1");
    let mut i = inputs(s);
    let mut c = core(&i);
    let (old_id, old_scope, _, _) = page_read(&c.sync(&i)).unwrap();
    i.snapshot.summary_revision = "r2".into();
    i.snapshot.content_revision = "c2".into();
    let (id, scope, _, request) = page_read(&c.sync(&i)).expect("the new revision is read");
    assert_eq!(request.expected_revision, "r2");
    c.page_answer(old_id, &old_scope, None, Ok(page("r1", files.clone(), 2, None, None)));
    assert!(c.change_files.is_empty(), "the old answer is dropped");
    c.page_answer(id, &scope, None, Ok(page("r2", files, 2, None, None)));
    assert_eq!(c.change_files.len(), 2);
}

#[test]
fn a_page_from_another_revision_is_an_error_not_a_listing() {
    let paths = vec!["a.ts".to_owned()];
    let (s, files) = paged(&paths, "r1");
    let i = inputs(s);
    let mut c = core(&i);
    let (id, scope, _, _) = page_read(&c.sync(&i)).unwrap();
    c.page_answer(id, &scope, None, Ok(page("rX", files, 1, None, None)));
    assert_eq!(c.change_page_state, RequestState::Error);
    assert_eq!(c.change_page_error.as_deref(), Some("Git 变更页来自其他仓库修订；请刷新后重试"));
}

#[test]
fn uses_present_filtered_out_and_missing_selection_states_distinctly() {
    let paths: Vec<String> = ["src/match/a.ts", "src/match/b.ts", "src/other/c.ts"].iter().map(|s| (*s).to_owned()).collect();
    let (s, files) = paged(&paths, "v1");
    let mut i = inputs(s);
    let mut c = core(&i);
    let (id, scope, _, _) = page_read(&c.sync(&i)).unwrap();
    c.page_answer(id, &scope, None, Ok(page("v1", files[..2].to_vec(), 3, None, None)));
    assert_eq!(c.selected_path.as_deref(), Some("src/match/a.ts"));

    // Present: the backend answers with the file the selection still points at.
    c.change_filter = "match".into();
    let (id, scope, _, _) = page_read(&c.sync(&i)).unwrap();
    c.page_answer(id, &scope, None, Ok(page("v1", vec![files[1].clone()], 2, None, Some(GitChangeSelection::Present { file: files[0].clone() }))));
    assert_eq!(c.selected_path.as_deref(), Some("src/match/a.ts"));

    // Filtered out: the latent selection is kept, so no visible row takes it.
    c.change_filter = "other".into();
    let (id, scope, _, _) = page_read(&c.sync(&i)).unwrap();
    c.page_answer(id, &scope, None, Ok(page("v1", vec![files[2].clone()], 1, None, Some(GitChangeSelection::FilteredOut))));
    assert_eq!(c.selected_path.as_deref(), Some("src/match/a.ts"));

    // Missing: the selection is gone for good, so the first row takes it.
    i.snapshot.summary_revision = "v2".into();
    i.snapshot.content_revision = "c2".into();
    i.snapshot.changed_files = 1;
    let (id, scope, _, _) = page_read(&c.sync(&i)).unwrap();
    c.page_answer(id, &scope, None, Ok(page("v2", vec![files[2].clone()], 1, None, Some(GitChangeSelection::Missing))));
    assert_eq!(c.selected_path.as_deref(), Some("src/other/c.ts"));
}

#[test]
fn publishes_a_stale_page_summary_and_reloads_against_its_new_revision() {
    let paths = vec!["a.ts".to_owned()];
    let (s, _) = paged(&paths, "r1");
    let i = inputs(s);
    let mut c = core(&i);
    let (id, scope, _, _) = page_read(&c.sync(&i)).unwrap();
    let mut newer = summary("r2");
    newer.changed_files = 1;
    let effects = c.page_answer(id, &scope, None, Ok(GitChangePageResult::Stale { summary: newer }));
    assert_eq!(published(&effects)[0].as_ref().unwrap().summary_revision, "r2");
    assert_eq!(c.current_snapshot.summary_revision, "r2");
    let (_, _, _, request) = page_read(&c.sync(&i)).expect("reads against the new revision");
    assert_eq!(request.expected_revision, "r2");
}

#[test]
fn stages_a_file_through_a_tagged_action_and_publishes_its_snapshot() {
    let s = snapshot();
    let i = inputs(s.clone());
    let mut c = core(&i);
    c.sync(&i);
    let effects = c.begin_action(GitAction::Stage { paths: vec![PATH.into()] }, "stage:src/App.tsx", true);
    let Some(Effect::RunAction { request_id, identity, key, action }) = effects.into_iter().next() else { panic!() };
    assert!(matches!(&action, GitAction::Stage { paths } if paths == &[PATH.to_owned()]));
    assert_eq!(c.busy_action.as_deref(), Some("stage:src/App.tsx"));
    assert!(c.mutations_disabled());
    let effects = c.action_answer(request_id, &identity, &key, action_is_quiet(&action), Ok(GitActionResult { message: Some("done".into()), snapshot: Some(s.clone()) }));
    assert_eq!(published(&effects), [Some(s)]);
    assert!(effects.iter().any(|e| matches!(e, Effect::MutationEnd)));
    assert_eq!(c.busy_action, None);
    assert_eq!(c.operation_message, None, "staging is its own feedback");
}

#[test]
fn a_write_that_is_not_quiet_says_what_git_said_or_that_it_finished() {
    let s = snapshot();
    let i = inputs(s.clone());
    let mut c = core(&i);
    c.sync(&i);
    let action = GitAction::Checkout { branch: "x".into() };
    let Some(Effect::RunAction { request_id, identity, key, .. }) = c.begin_action(action.clone(), "k", true).into_iter().next() else { panic!() };
    c.action_answer(request_id, &identity, &key, false, Ok(GitActionResult { message: Some(" ok ".into()), snapshot: Some(s.clone()) }));
    assert_eq!(c.operation_message.as_deref(), Some("ok"));
    let Some(Effect::RunAction { request_id, identity, key, .. }) = c.begin_action(action, "k", true).into_iter().next() else { panic!() };
    assert_eq!(c.operation_message, None, "a new write clears the last word");
    c.action_answer(request_id, &identity, &key, false, Ok(GitActionResult { message: None, snapshot: Some(s) }));
    assert_eq!(c.operation_message.as_deref(), Some("Git 操作已完成"));
}

#[test]
fn a_write_that_lost_the_lease_race_says_so_and_goes_nowhere() {
    let i = inputs(snapshot());
    let mut c = core(&i);
    c.sync(&i);
    assert!(c.begin_action(GitAction::Stage { paths: vec![PATH.into()] }, "k", false).is_empty());
    assert_eq!(c.operation_error.as_deref(), Some("另一个任务或工作区操作已经开始，请稍后重试"));
    assert_eq!(c.busy_action, None);
}

#[test]
fn does_not_let_a_diff_read_from_before_a_mutation_overwrite_the_refreshed_diff() {
    let s = snapshot();
    let i = inputs(s.clone());
    let mut c = core(&i);
    let (stale_id, _) = scope_read(&c.sync(&i)).unwrap();
    let Some(Effect::RunAction { request_id, identity, key, .. }) = c.begin_action(GitAction::Stage { paths: vec![PATH.into()] }, "k", true).into_iter().next() else { panic!() };
    assert!(scope_read(&c.sync(&i)).is_none(), "nothing is read while a write is out");
    c.action_answer(request_id, &identity, &key, true, Ok(GitActionResult { message: None, snapshot: Some(s) }));
    let (fresh_id, _) = scope_read(&c.sync(&i)).expect("the write's result is read");
    c.scope_diff_answer(fresh_id, Ok(response(&patch(PATH).replace("new", "fresh-after-mutation"))));
    c.scope_diff_answer(stale_id, Ok(response(&patch(PATH).replace("new", "stale-before-mutation"))));
    let text = &c.scope_patches[PATH].hunks[0].lines[1].text;
    assert!(text.contains("fresh-after-mutation"), "{text}");
}

#[test]
fn says_an_untracked_only_working_tree_has_nothing_to_review_and_reads_no_diff() {
    let mut s = snapshot();
    let mut draft = file("draft.txt");
    draft.untracked = true;
    draft.status = GitFileStatus::Untracked;
    s.files = vec![draft];
    s.untracked = 1;
    let i = inputs(s);
    let mut c = core(&i);
    let effects = c.sync(&i);
    assert!(scope_read(&effects).is_none());
    assert_eq!(c.total_changed_files(), 0);
    assert_eq!(c.empty_label(), "没有已跟踪的变更；1 个未跟踪文件不在审阅范围内。");
    assert_eq!(c.diff_state, RequestState::Ready);
}

#[test]
fn a_scope_too_large_to_read_at_once_is_read_file_by_file() {
    let mut s = snapshot();
    s.additions = DIFF_LARGE_LINE_COUNT + 1;
    let i = inputs(s);
    let mut c = core(&i);
    assert!(scope_read(&c.sync(&i)).is_none());
    assert_eq!(c.diff_state, RequestState::Ready);
    assert!(c.request_file_patch(PATH, None).is_some());
    let many: Vec<String> = (0..(DIFF_LAZY_FILE_COUNT + 1)).map(|i| format!("f{i}")).collect();
    let (s, _) = paged(&many, "r1");
    let i = inputs(s);
    let mut c = core(&i);
    assert!(scope_read(&c.sync(&i)).is_none(), "three hundred small files are still one `git diff` holding the lock");
}

/// Arms the discard on a file with only an unstaged change, the way the first click does.
fn armed() -> (ReviewCore, ReviewInputs, GitWorkspaceSnapshot) {
    let mut s = snapshot();
    s.files[0].staged = false;
    s.staged = 0;
    let i = inputs(s.clone());
    let mut c = core(&i);
    c.sync(&i);
    assert_eq!(labels(&c, PATH).last().unwrap(), "丢弃未暂存更改");
    let effects = c.request_discard(PATH, false, None);
    let (id, scope, key, confirmation) = discard_request(&effects);
    assert_eq!(c.busy_action.as_deref(), Some("prepare-discard:src/App.tsx"));
    let (effects, run) = c.discard_prepared(id, &scope, &key, PATH, false, confirmation, prepared(&s, "target-revision-1"));
    assert!(run.is_none(), "the first click only arms");
    assert_eq!(published(&effects), [Some(s.clone())]);
    assert_eq!(c.busy_action, None);
    assert_eq!(labels(&c, PATH).last().unwrap(), "确认丢弃未暂存更改");
    (c, i, s)
}

#[test]
fn the_second_click_prepares_again_and_runs_only_if_nothing_moved() {
    let (mut c, _, s) = armed();
    let armed_discard = c.armed_discard(PATH);
    let (id, scope, key, confirmation) = discard_request(&c.request_discard(PATH, false, armed_discard));
    let (_, run) = c.discard_prepared(id, &scope, &key, PATH, false, confirmation, prepared(&s, "target-revision-1"));
    let (action, action_key) = run.expect("confirmed");
    assert!(matches!(&action, GitAction::Discard { paths, include_untracked: false, expected_content_revision, expected_target_revision } if paths == &[PATH.to_owned()] && expected_content_revision == "revision-1" && expected_target_revision == "target-revision-1"));
    assert_eq!(action_key, "discard:revision-1:src/App.tsx");
}

#[test]
fn requires_a_new_confirmation_after_the_repository_content_revision_changes() {
    let (mut c, mut i, _) = armed();
    i.snapshot.content_revision = "revision-2".into();
    c.sync(&i);
    // The armed confirmation belonged to the old content revision, so it is gone.
    assert_eq!(labels(&c, PATH).last().unwrap(), "丢弃未暂存更改");
    assert_eq!(c.armed_discard(PATH), None);
    let (id, scope, key, confirmation) = discard_request(&c.request_discard(PATH, false, None));
    let (_, run) = c.discard_prepared(id, &scope, &key, PATH, false, confirmation, prepared(&i.snapshot, "target-revision-2"));
    assert!(run.is_none());
    let armed_discard = c.armed_discard(PATH);
    assert!(armed_discard.is_some());
    let (id, scope, key, confirmation) = discard_request(&c.request_discard(PATH, false, armed_discard));
    let (_, run) = c.discard_prepared(id, &scope, &key, PATH, false, confirmation, prepared(&i.snapshot, "target-revision-2"));
    assert!(matches!(run.unwrap().0, GitAction::Discard { expected_content_revision, expected_target_revision, .. } if expected_content_revision == "revision-2" && expected_target_revision == "target-revision-2"));
}

#[test]
fn requires_another_confirmation_when_only_the_discard_target_revision_changes() {
    let (mut c, _, s) = armed();
    let armed_discard = c.armed_discard(PATH);
    let (id, scope, key, confirmation) = discard_request(&c.request_discard(PATH, false, armed_discard));
    let (_, run) = c.discard_prepared(id, &scope, &key, PATH, false, confirmation, prepared(&s, "target-revision-2"));
    assert!(run.is_none(), "the file on disk moved under the confirmation");
    assert_eq!(labels(&c, PATH).last().unwrap(), "确认丢弃未暂存更改", "and this is the one to confirm now");
    assert_eq!(c.pending_discard.as_ref().unwrap().target_revision, "target-revision-2");
}

#[test]
fn ignores_out_of_order_preparations_after_the_repository_changes() {
    let mut one = snapshot();
    one.repository_root = "C:/repo-one".into();
    one.worktree_root = "C:/repo-one".into();
    let mut two = one.clone();
    two.content_revision = "revision-repo-two".into();
    two.repository_root = "C:/repo-two".into();
    two.worktree_root = "C:/repo-two".into();
    let mut i = inputs(one.clone());
    let mut c = core(&i);
    c.sync(&i);
    let (old_id, old_scope, old_key, old_conf) = discard_request(&c.request_discard(PATH, false, None));
    i.snapshot = two.clone();
    c.sync(&i);
    assert_eq!(c.busy_action, None, "the old preparation stops holding the panel");
    let (id, scope, key, conf) = discard_request(&c.request_discard(PATH, false, None));
    let (effects, _) = c.discard_prepared(id, &scope, &key, PATH, false, conf, prepared(&two, "target-repo-two"));
    assert_eq!(published(&effects), [Some(two.clone())]);
    assert_eq!(labels(&c, PATH).last().unwrap(), "确认丢弃未暂存更改");
    let (effects, run) = c.discard_prepared(old_id, &old_scope, &old_key, PATH, false, old_conf, prepared(&one, "target-repo-one"));
    assert!(effects.is_empty() && run.is_none());
    assert_eq!(c.pending_discard.as_ref().unwrap().target_revision, "target-repo-two");
}

#[test]
fn does_not_publish_or_arm_a_preparation_from_an_old_conversation_or_an_inactive_panel() {
    let s = snapshot();
    let mut i = inputs(s.clone());
    let mut c = core(&i);
    c.sync(&i);
    let (id, scope, key, conf) = discard_request(&c.request_discard(PATH, false, None));
    i.target = git_conversation_target("conversation-2", None);
    c.sync(&i);
    let (effects, _) = c.discard_prepared(id, &scope, &key, PATH, false, conf, prepared(&s, "t"));
    assert!(effects.is_empty());
    assert_eq!(c.pending_discard, None);

    let (id, scope, key, conf) = discard_request(&c.request_discard(PATH, false, None));
    i.active = false;
    c.sync(&i);
    let (effects, _) = c.discard_prepared(id, &scope, &key, PATH, false, conf, prepared(&s, "t"));
    assert!(effects.is_empty());
    assert_eq!(c.pending_discard, None);
}

#[test]
fn invalidates_a_deferred_preparation_when_the_same_repository_snapshot_changes() {
    let s = snapshot();
    let mut i = inputs(s.clone());
    let mut c = core(&i);
    c.sync(&i);
    let (id, scope, key, conf) = discard_request(&c.request_discard(PATH, false, None));
    i.snapshot.additions += 1;
    c.sync(&i);
    assert_eq!(c.busy_action, None);
    let (effects, _) = c.discard_prepared(id, &scope, &key, PATH, false, conf, prepared(&s, "t"));
    assert!(effects.is_empty(), "it was prepared against something else");
    assert_eq!(c.pending_discard, None);
}

#[test]
fn preserves_an_armed_discard_when_the_parent_echoes_its_accepted_preparation_snapshot() {
    let mut s = snapshot();
    s.files[0].staged = false;
    s.staged = 0;
    let mut i = inputs(s.clone());
    let mut c = core(&i);
    c.sync(&i);
    let (id, scope, key, conf) = discard_request(&c.request_discard(PATH, false, None));
    let mut accepted = s.clone();
    accepted.additions += 3;
    c.discard_prepared(id, &scope, &key, PATH, false, conf, prepared(&accepted, "t"));
    assert!(c.armed_discard(PATH).is_some());
    // The parent stores what was published and hands it back.
    i.snapshot = accepted;
    c.sync(&i);
    assert!(c.armed_discard(PATH).is_some(), "the echo of its own preparation does not disarm it");
}

#[test]
fn an_untracked_file_warns_that_deleting_it_cannot_be_undone() {
    let mut s = snapshot();
    let mut f = file("draft.txt");
    f.untracked = true;
    s.files.push(f.clone());
    // A branch or a page can list one; the discard rows are what matter here.
    let i = inputs(s);
    let mut c = core(&i);
    c.sync(&i);
    c.change_files.push(f);
    let actions = c.file_actions("draft.txt");
    let discard = actions.iter().find(|a| a.id == "discard").unwrap();
    assert_eq!(discard.label, "永久删除未跟踪文件");
    assert_eq!(discard.description.as_deref(), Some("这个文件不在 Git 里，删除无法撤销。"));
}

#[test]
fn keeps_nested_submodule_changes_visible_without_offering_no_op_parent_actions() {
    let mut s = snapshot();
    let mut f = file("vendor/module");
    f.submodule = true;
    f.submodule_modified = true;
    s.files = vec![f];
    s.staged = 0;
    s.warnings = vec!["1 个子模块包含内部未提交变更；请将子模块目录作为独立工作区处理".into()];
    let i = inputs(s);
    let mut c = core(&i);
    c.sync(&i);
    assert_eq!(c.change_files.len(), 1);
    assert!(c.file_actions("vendor/module").is_empty());
}

#[test]
fn a_submodule_whose_commit_moved_can_be_staged_but_not_discarded() {
    let mut s = snapshot();
    let mut f = file("vendor/module");
    f.submodule = true;
    f.submodule_commit_changed = true;
    s.files = vec![f];
    let i = inputs(s);
    let mut c = core(&i);
    c.sync(&i);
    assert_eq!(labels(&c, "vendor/module"), ["暂存"]);
}

fn merging() -> GitWorkspaceSnapshot {
    let mut s = snapshot();
    s.operation = Some(GitRepositoryOperation::Merge);
    s.operation_revision = Some("operation-revision-1".into());
    s.conflicted = 1;
    s.files[0].status = GitFileStatus::Unmerged;
    s.files[0].conflicted = true;
    s
}

#[test]
fn says_how_a_repository_operation_stands_and_a_restarted_one_disarms_its_buttons() {
    let mut i = inputs(merging());
    let mut c = core(&i);
    c.sync(&i);
    assert_eq!(c.operation_note().as_deref(), Some("仍有 1 个冲突；解决并暂存后再继续"));
    assert!(labels(&c, PATH).contains(&"暂存".to_owned()), "resolving a conflict is still staging");
    let scope = c.operation_scope().unwrap();
    let abort = format!("abort-operation:{scope}");
    assert!(!c.confirm_local_git_action(&abort), "the first click arms");
    assert_eq!(c.pending_git_action.as_deref(), Some(abort.as_str()));
    i.snapshot.operation_revision = Some("operation-revision-2".into());
    c.sync(&i);
    assert_eq!(c.pending_git_action, None, "that was another operation");
    let scope = c.operation_scope().unwrap();
    let abort = format!("abort-operation:{scope}");
    assert!(!c.confirm_local_git_action(&abort));
    assert!(c.confirm_local_git_action(&abort), "the second click runs");
    assert_eq!(c.pending_git_action, None);
}

#[test]
fn what_each_operation_offers() {
    use GitRepositoryOperation::*;
    assert!(operation_supports_continue(Merge) && !operation_supports_continue(Bisect));
    assert!(operation_supports_skip(Rebase) && operation_supports_skip(CherryPick) && operation_supports_skip(Revert));
    assert!(!operation_supports_skip(Merge) && !operation_supports_skip(Bisect));
    assert_eq!(repository_operation_label(CherryPick), "拣选提交");
}

#[test]
fn a_bisect_says_to_commit_first_when_the_tree_is_not_clean() {
    let mut s = snapshot();
    s.operation = Some(GitRepositoryOperation::Bisect);
    s.operation_revision = Some("b".into());
    let i = inputs(GitWorkspaceSnapshot { is_clean: false, ..s.clone() });
    let mut c = core(&i);
    c.sync(&i);
    assert_eq!(c.operation_note().as_deref(), Some("先提交或储藏当前变更，才能继续二分查找"));
    let i = inputs(GitWorkspaceSnapshot { is_clean: true, ..s });
    let mut c = core(&i);
    c.sync(&i);
    assert!(c.operation_note().unwrap().starts_with("测试当前提交"));
}

#[test]
fn reconciles_the_repository_snapshot_after_a_failed_action_mutates_git_state() {
    let i = inputs(snapshot());
    let mut c = core(&i);
    c.sync(&i);
    let Some(Effect::RunAction { request_id, identity, key, action }) = c.begin_action(GitAction::Stage { paths: vec![PATH.into()] }, "k", true).into_iter().next() else { panic!() };
    let effects = c.action_answer(request_id, &identity, &key, action_is_quiet(&action), Err("merge conflict".into()));
    let Some(Effect::Reconcile { message, .. }) = effects.iter().find(|e| matches!(e, Effect::Reconcile { .. })) else { panic!("{effects:?}") };
    assert_eq!(message, "merge conflict");
    assert_eq!(c.busy_action.as_deref(), Some("k"), "still working: the write's lease is held until the read is back");
    assert!(!effects.iter().any(|e| matches!(e, Effect::MutationEnd)));

    let mut conflicted = summary("conflicted-summary");
    conflicted.operation = Some(GitRepositoryOperation::Merge);
    conflicted.operation_revision = Some("o".into());
    let effects = c.reconcile_answer(request_id, &identity, &key, "merge conflict".into(), Ok(GitWorkspaceSummaryResult::Snapshot { summary: conflicted }));
    assert_eq!(published(&effects)[0].as_ref().unwrap().operation, Some(GitRepositoryOperation::Merge));
    assert_eq!(c.operation_error.as_deref(), Some("merge conflict"));
    assert_eq!(c.busy_action, None);
    assert!(effects.iter().any(|e| matches!(e, Effect::MutationEnd)));
}

#[test]
fn the_action_failure_stands_when_the_read_after_it_fails_too() {
    let i = inputs(snapshot());
    let mut c = core(&i);
    c.sync(&i);
    let Some(Effect::RunAction { request_id, identity, key, .. }) = c.begin_action(GitAction::Stage { paths: vec![PATH.into()] }, "k", true).into_iter().next() else { panic!() };
    c.action_answer(request_id, &identity, &key, true, Err("first".into()));
    let effects = c.reconcile_answer(request_id, &identity, &key, "first".into(), Err("second".into()));
    assert_eq!(c.operation_error.as_deref(), Some("first"));
    assert!(effects.iter().any(|e| matches!(e, Effect::MutationEnd)));
}

#[test]
fn keeps_review_readable_but_disables_mutations_while_the_workspace_is_busy() {
    let mut s = snapshot();
    s.files[0].staged = false;
    let mut i = inputs(s);
    i.mutation_disabled_reason = Some("内置终端正在执行命令".into());
    let mut c = core(&i);
    let effects = c.sync(&i);
    assert!(scope_read(&effects).is_some(), "reading goes on");
    assert!(c.file_actions(PATH).iter().all(|a| a.disabled));
    assert!(c.begin_action(GitAction::Stage { paths: vec![PATH.into()] }, "k", true).is_empty());
    assert!(c.request_discard(PATH, false, None).is_empty());
}

#[test]
fn disarms_destructive_confirmations_whenever_workspace_mutations_become_locked() {
    let (mut c, mut i, _) = armed();
    c.pending_git_action = Some("k".into());
    i.mutation_disabled_reason = Some("busy".into());
    c.sync(&i);
    assert_eq!(c.pending_discard, None);
    assert_eq!(c.pending_git_action, None);
    i.mutation_disabled_reason = None;
    c.sync(&i);
    assert_eq!(labels(&c, PATH).last().unwrap(), "丢弃未暂存更改");
}

#[test]
fn going_inactive_stops_what_is_out_and_disarms() {
    let (mut c, mut i, _) = armed();
    let id = c.change_diff_request;
    i.active = false;
    c.sync(&i);
    assert_ne!(c.change_diff_request, id);
    assert_eq!(c.pending_discard, None);
    assert_eq!(c.diff_state, RequestState::Idle);
}

#[test]
fn another_repository_at_the_same_path_reuses_nothing() {
    let paths = vec!["a.ts".to_owned()];
    let (s, files) = paged(&paths, "r1");
    let mut i = inputs(s);
    let mut c = core(&i);
    let (id, scope, _, _) = page_read(&c.sync(&i)).unwrap();
    c.page_answer(id, &scope, None, Ok(page("r1", files, 1, None, None)));
    c.change_filter = "a".into();
    c.sync(&i);
    i.snapshot.repository_id = "another".into();
    i.snapshot.summary_revision = "x1".into();
    c.sync(&i);
    assert!(c.change_files.is_empty());
    assert_eq!(c.change_filter, "");
    assert_eq!(c.selected_path, None);
}

#[test]
fn refresh_reads_the_summary_and_rereads_the_diff() {
    let i = inputs(snapshot());
    let mut c = core(&i);
    let (id, _) = scope_read(&c.sync(&i)).unwrap();
    c.scope_diff_answer(id, Ok(response(&patch(PATH))));
    let effects = c.refresh_snapshot();
    let Some(Effect::Refresh { identity, .. }) = effects.first().cloned() else { panic!() };
    assert_eq!(c.busy_action.as_deref(), Some("refresh"));
    assert!(c.refresh_snapshot().is_empty(), "one at a time");
    let effects = c.refresh_answer(&identity, Ok(GitWorkspaceSummaryResult::NotRepository));
    assert_eq!(published(&effects), [None]);
    assert_eq!(c.busy_action, None);
    assert!(scope_read(&c.sync(&i)).is_some(), "a refresh rereads even when nothing else moved");
    let effects = c.refresh_snapshot();
    let Some(Effect::Refresh { identity, .. }) = effects.first().cloned() else { panic!() };
    c.refresh_answer(&identity, Err(" ".into()));
    assert_eq!(c.operation_error.as_deref(), Some("无法刷新 Git 状态"));
}

#[test]
fn a_failed_scope_read_says_why_and_a_failed_page_too() {
    let i = inputs(snapshot());
    let mut c = core(&i);
    let (id, _) = scope_read(&c.sync(&i)).unwrap();
    c.scope_diff_answer(id, Err("".into()));
    assert_eq!(c.diff_error.as_deref(), Some("无法读取文件差异"));
    assert_eq!(c.diff_state, RequestState::Error);
    let paths = vec!["a.ts".to_owned()];
    let (s, _) = paged(&paths, "r1");
    let i = inputs(s);
    let mut c = core(&i);
    let (id, scope, _, _) = page_read(&c.sync(&i)).unwrap();
    c.page_answer(id, &scope, None, Err("".into()));
    assert_eq!(c.change_page_error.as_deref(), Some("无法读取 Git 变更文件"));
}

#[test]
fn an_inline_filter_narrows_by_either_name_and_selection_falls_to_the_first_match() {
    let mut s = snapshot();
    let mut renamed = file("new/name.ts");
    renamed.original_path = Some("OLD/NAME.ts".into());
    s.files = vec![file("a.ts"), renamed];
    let i = inputs(s);
    let mut c = core(&i);
    c.sync(&i);
    assert_eq!(c.selected_path.as_deref(), Some("a.ts"));
    c.change_filter = "old/".into();
    c.sync(&i);
    assert_eq!(c.change_files.len(), 1);
    assert_eq!(c.selected_path.as_deref(), Some("new/name.ts"));
    c.change_filter = "nothing".into();
    c.sync(&i);
    assert!(c.change_files.is_empty());
    assert_eq!(c.selected_path, None);
}
