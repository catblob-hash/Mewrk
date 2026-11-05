//! The pane against a real repository through the real host: `App::start` over a temp data dir, and `git` on disk telling
//! the truth about what happened. Nothing here is mocked, so these are the tests that say the wire shapes are right.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fluent::el::{hash_key, rect_of, render, set_a11y_active, set_theme, set_window_size, take_a11y_update, theme};
use fluent::ui::{Input, NoClipboard, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::*;
use mewrk_client::{i18n, App, StartOptions};
use mewrk_pane_git::review_core::RequestState;
use mewrk_pane_git::*;
use mewrk_ui_common::context_menu::{menu_panel_key, menu_row_key};

const WIN: (f32, f32) = (1200.0, 900.0);
const SCHEMA_VERSION: u32 = 6;
const KEY: &str = "review-1";
const SNAPSHOT_KEY: &str = "conversation-1";

static SERIAL: Mutex<()> = Mutex::new(());

fn fresh_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("mewrk-git-{tag}-{}-{n:x}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn git(repo: &Path, args: &[&str]) -> String {
    // The host's git never sees this variable, so this one mustn't either: with a system config that sets `core.autocrlf`, as Git for
    // Windows' does, the host would check files out with CRLF and this one would commit that as a change.
    let out = Command::new("git").args(args).current_dir(repo).env_remove("GIT_CONFIG_NOSYSTEM").env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(repo: &Path, path: &str, text: &str) {
    let full = repo.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

fn read(repo: &Path, path: &str) -> String {
    std::fs::read_to_string(repo.join(path)).unwrap()
}

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: NoClipboard,
    repo: PathBuf,
    target: GitTarget,
    mutations: Arc<Mutex<Vec<&'static str>>>,
    allow_mutation: Rc<std::cell::Cell<bool>>,
    disabled_reason: Rc<std::cell::RefCell<Option<String>>>,
    checkout: Rc<std::cell::RefCell<GitReviewCheckout>>,
    _serial: MutexGuard<'static, ()>,
}

fn start(seed: impl FnOnce(&Path)) -> (Rig, App) {
    let serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let (dir, home, repo) = (fresh_dir("data"), fresh_dir("home"), fresh_dir("repo"));
    git(&repo, &["init", "-b", "main"]);
    write(&repo, "src/app.ts", "const keep = 1;\nconst old = 2;\nconst tail = 3;\n");
    write(&repo, "README.md", "# readme\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "first"]);
    seed(&repo);
    let document = json!({
        "schemaVersion": SCHEMA_VERSION,
        "globalSettings": {},
        "workspaces": [{ "id": "ws1", "name": "project", "kind": "directory", "path": repo.to_str().unwrap(), "createdAt": "2026-01-01T00:00:00.000Z", "defaultConversationPresetId": "", "conversations": [] }],
        "tools": [{ "name": "read", "label": "Read", "description": "", "category": "filesystem", "dangerous": false, "parameters": [] }],
        "capabilities": { "skills": [], "mcps": [] },
    });
    std::fs::write(dir.join("document.v1.json"), serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    unsafe {
        std::env::set_var("HOME", &home);
        // Where `std::env::home_dir` looks on Windows.
        std::env::set_var("USERPROFILE", &home);
    }
    set_theme(Theme::dark());
    set_window_size(WIN.0, WIN.1);
    let app = App::start(StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: dir, resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
    i18n::set_english(false);
    let rig = Rig {
        scene: Scene::default(),
        text: TextCtx::new(1.0),
        mem: UiMemory::default(),
        input: Input::default(),
        clipboard: NoClipboard,
        repo,
        target: git_workspace_target("ws1", None),
        mutations: Arc::default(),
        allow_mutation: Rc::new(std::cell::Cell::new(true)),
        disabled_reason: Rc::default(),
        checkout: Rc::default(),
        _serial: serial,
    };
    (rig, app)
}

/// `App.tsx` around the panel: the snapshot comes from the controller, and what the panel publishes goes back there.
fn view_of(rig: &Rig) -> impl FnMut(&mut App) -> E + use<> {
    let (target, mutations, allow, reason, checkout) = (rig.target.clone(), rig.mutations.clone(), rig.allow_mutation.clone(), rig.disabled_reason.clone(), rig.checkout.clone());
    move |app: &mut App| -> E {
        let Some(snapshot) = git_controller(app).snapshots.get(SNAPSHOT_KEY).and_then(|e| e.snapshot.clone()) else { return div() };
        let inputs = ReviewInputs { target: target.clone(), snapshot, active: true, checkout: checkout.borrow().clone(), reveal_request: None, mutation_disabled_reason: reason.borrow().clone() };
        let hooks = GitReviewHooks {
            on_snapshot_change: Rc::new(|app, next| {
                git_controller(app).update_snapshots(|map| {
                    git_snapshots_after_workspace_mutation(map, &[SNAPSHOT_KEY.to_owned()], "ws1", next.as_ref());
                    true
                });
            }),
            on_mutation_start: {
                let (mutations, allow) = (mutations.clone(), allow.clone());
                Rc::new(move |app| {
                    if !allow.get() {
                        return false;
                    }
                    mutations.lock().unwrap().push("start");
                    git_controller(app).acquire_mutation_lease(SNAPSHOT_KEY, &[SNAPSHOT_KEY.to_owned()]);
                    true
                })
            },
            on_mutation_end: {
                let mutations = mutations.clone();
                Rc::new(move |app| {
                    mutations.lock().unwrap().push("end");
                    git_controller(app).release_mutation_lease(SNAPSHOT_KEY);
                })
            },
            on_reveal_handled: Rc::new(|_, _| {}),
            on_mention_line: None,
        };
        let pane = git_review_panel(app, GitReviewProps { key: KEY.to_owned(), inputs, hooks, page_tabs: None });
        col().child(row().h(32.0).child(pane.header)).child(pane.body)
    }
}

fn frame(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) {
    app.tick();
    rig.scene.clear();
    let root = view(app);
    let mut ui = Ui::new(&mut rig.scene, &mut rig.text, theme(), &rig.input, &mut rig.mem, &mut rig.clipboard, 1.0, WIN, 0.016, 0.0);
    render(root, app, &mut ui);
    rig.input.end_frame();
}

fn until(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, what: &str, done: &mut dyn FnMut(&mut App) -> bool) {
    let start = Instant::now();
    loop {
        frame(rig, app, view);
        if done(app) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "timed out waiting for {what}: {:?}", review_core(app, KEY).map(|c| (c.busy_action.clone(), c.operation_error.clone(), c.change_files.len(), c.diff_state, c.change_page_state)));
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn click(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, key: impl std::hash::Hash) {
    let r = rect_of(key).expect("on screen");
    rig.input.pointer = Some((r.x + r.w * 0.5, r.y + r.h * 0.5));
    frame(rig, app, view);
    rig.input.pressed = true;
    rig.input.down = true;
    frame(rig, app, view);
    rig.input.down = false;
    rig.input.released = true;
    frame(rig, app, view);
    frame(rig, app, view);
}

fn refresh_summary(rig: &Rig, app: &mut App) {
    git_refresh(app, SNAPSHOT_KEY, "ws1", &rig.target, |_, result| {
        result.unwrap();
    });
}

fn known(app: &mut App) -> bool {
    git_controller(app).snapshots.contains_key(SNAPSHOT_KEY)
}

fn settled(app: &mut App) -> bool {
    review_core(app, KEY).is_some_and(|c| c.busy_action.is_none() && c.change_page_state == RequestState::Ready && c.diff_state == RequestState::Ready)
}

fn listed(app: &mut App) -> Vec<String> {
    review_core(app, KEY).map(|c| c.change_files.iter().map(|f| f.path.clone()).collect()).unwrap_or_default()
}

fn porcelain(rig: &Rig) -> String {
    git(&rig.repo, &["status", "--porcelain"])
}

/// Opens a review on what is in the repository, with everything read.
fn open(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) {
    refresh_summary(rig, app);
    until(rig, app, view, "the summary", &mut |app| known(app));
    until(rig, app, view, "the listing", &mut settled);
}

/// A write, through the same function the `⋮` calls, and back to settled.
fn act(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, path: &str, id: &str) {
    mewrk_pane_git::review::file_action(app, KEY, path, id);
    until(rig, app, view, "the action to finish", &mut settled);
}

#[test]
fn stage_unstage_and_discard_change_the_repository_and_the_listing_follows() {
    let (mut rig, mut app) = start(|repo| {
        write(repo, "src/app.ts", "const keep = 1;\nconst next = 2;\nconst tail = 3;\n");
        write(repo, "README.md", "# readme\nmore\n");
        write(repo, "draft.txt", "untracked\n");
    });
    let mut view = view_of(&rig);
    open(&mut rig, &mut app, &mut view);
    // Tracked changes, once each; the untracked file is out of review scope.
    assert_eq!(listed(&mut app), ["README.md", "src/app.ts"]);
    assert_eq!(review_core(&mut app, KEY).unwrap().current_snapshot.untracked, 1);

    // The scope-wide patch came through with its hunks.
    let c = review_core(&mut app, KEY).unwrap();
    let patch = c.scope_patches.get("src/app.ts").expect("a patch for the file").clone();
    assert_eq!((patch.additions, patch.deletions), (1, 1));
    assert_eq!(c.diff_entries().iter().map(|e| (e.path.as_str(), e.additions, e.deletions)).collect::<Vec<_>>(), [("README.md", 1, 0), ("src/app.ts", 1, 1)]);

    // Stage: the file moves, on disk and in the panel.
    act(&mut rig, &mut app, &mut view, "src/app.ts", "stage");
    assert!(porcelain(&rig).lines().any(|l| l == "M  src/app.ts"), "{}", porcelain(&rig));
    assert_eq!(rig.mutations.lock().unwrap().as_slice(), ["start", "end"]);
    let c = review_core(&mut app, KEY).unwrap();
    let file = c.change_files.iter().find(|f| f.path == "src/app.ts").unwrap();
    assert!(file.staged && !file.unstaged);
    assert!(c.file_actions("src/app.ts").iter().any(|a| a.id == "unstage"));
    assert!(!c.file_actions("src/app.ts").iter().any(|a| a.id == "stage"));

    // Narrow the scope to what is staged: the host answers for that side alone.
    review_core(&mut app, KEY).unwrap().diff_scope = DiffScope::Staged;
    until(&mut rig, &mut app, &mut view, "the staged scope", &mut |app| review_core(app, KEY).is_some_and(|c| c.diff_state == RequestState::Ready && c.scope_patches.len() == 1 && c.scope_patches.contains_key("src/app.ts")));
    review_core(&mut app, KEY).unwrap().diff_scope = DiffScope::Working;
    until(&mut rig, &mut app, &mut view, "all changes again", &mut |app| review_core(app, KEY).is_some_and(|c| c.diff_state == RequestState::Ready && c.scope_patches.len() == 2));

    // Unstage: back again.
    act(&mut rig, &mut app, &mut view, "src/app.ts", "unstage");
    assert!(porcelain(&rig).lines().any(|l| l == " M src/app.ts"), "{}", porcelain(&rig));

    // Discard takes two clicks, and the first changes nothing on disk.
    act(&mut rig, &mut app, &mut view, "src/app.ts", "discard");
    assert!(read(&rig.repo, "src/app.ts").contains("next"), "the first click only arms it");
    let c = review_core(&mut app, KEY).unwrap();
    assert_eq!(c.file_actions("src/app.ts").iter().find(|a| a.id == "discard").unwrap().label, "确认丢弃未暂存更改");
    act(&mut rig, &mut app, &mut view, "src/app.ts", "discard");
    until(&mut rig, &mut app, &mut view, "the file to go from the list", &mut |app| listed(app) == ["README.md"]);
    // Under `core.autocrlf`, git writes it back with CRLF.
    assert_eq!(read(&rig.repo, "src/app.ts").replace("\r\n", "\n"), "const keep = 1;\nconst old = 2;\nconst tail = 3;\n");
    assert!(read(&rig.repo, "draft.txt").contains("untracked"), "and it touched nothing else");
}

#[test]
fn a_repository_moving_under_an_armed_discard_takes_the_confirmation_back() {
    let (mut rig, mut app) = start(|repo| write(repo, "src/app.ts", "const keep = 1;\nconst next = 2;\nconst tail = 3;\n"));
    let mut view = view_of(&rig);
    open(&mut rig, &mut app, &mut view);
    act(&mut rig, &mut app, &mut view, "src/app.ts", "discard");
    assert_eq!(review_core(&mut app, KEY).unwrap().file_actions("src/app.ts").iter().find(|a| a.id == "discard").unwrap().label, "确认丢弃未暂存更改");

    // Someone edits the file between the two clicks.
    write(&rig.repo, "src/app.ts", "const keep = 1;\nconst later = 2;\nconst tail = 3;\n");
    let armed = review_core(&mut app, KEY).unwrap().armed_discard("src/app.ts");
    assert!(armed.is_some());
    act(&mut rig, &mut app, &mut view, "src/app.ts", "discard");
    assert!(read(&rig.repo, "src/app.ts").contains("later"), "the second click found another file, and discarded nothing");
    assert_eq!(review_core(&mut app, KEY).unwrap().file_actions("src/app.ts").iter().find(|a| a.id == "discard").unwrap().label, "确认丢弃未暂存更改", "it asks about this version instead");
}

#[test]
fn hundreds_of_changes_come_a_page_at_a_time_and_the_filter_goes_to_the_host() {
    let (mut rig, mut app) = start(|repo| {
        for i in 0..450 {
            write(repo, &format!("bulk/file-{i:03}.txt"), "one\n");
        }
        git(repo, &["add", "."]);
        git(repo, &["commit", "-m", "many"]);
        for i in 0..450 {
            write(repo, &format!("bulk/file-{i:03}.txt"), "two\n");
        }
    });
    let mut view = view_of(&rig);
    refresh_summary(&rig, &mut app);
    until(&mut rig, &mut app, &mut view, "the first page", &mut |app| listed(app).len() == 200);
    let c = review_core(&mut app, KEY).unwrap();
    assert_eq!(c.change_matched_count, 450);
    assert_eq!(c.next_change_page_count(), 200);
    assert!(c.scope_patches.is_empty(), "this many files are never one read");
    assert_eq!(c.selected_path.as_deref(), Some("bulk/file-000.txt"));

    // The button in the column's footer, by its key.
    until(&mut rig, &mut app, &mut view, "the button", &mut |_| rect_of(("review-more", KEY)).is_some());
    click(&mut rig, &mut app, &mut view, ("review-more", KEY));
    until(&mut rig, &mut app, &mut view, "the second page", &mut |app| listed(app).len() == 400);
    click(&mut rig, &mut app, &mut view, ("review-more", KEY));
    until(&mut rig, &mut app, &mut view, "the last page", &mut |app| listed(app).len() == 450);
    assert_eq!(review_core(&mut app, KEY).unwrap().change_next_cursor, None);
    let names = listed(&mut app);
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 450, "nothing listed twice");

    // A filter is the host's to answer, and starts the pages over.
    review_core(&mut app, KEY).unwrap().change_filter = "FILE-03".into();
    until(&mut rig, &mut app, &mut view, "the filtered page", &mut |app| review_core(app, KEY).is_some_and(|c| c.change_page_state == RequestState::Ready && c.change_matched_count == 10));
    assert_eq!(listed(&mut app).len(), 10);
    assert!(listed(&mut app).iter().all(|p| p.contains("file-03")));

    // A single file's patch is asked for when it is opened.
    let Some(effect) = review_core(&mut app, KEY).unwrap().request_file_patch("bulk/file-000.txt", None) else { panic!("not ready") };
    assert!(matches!(effect, mewrk_pane_git::review_core::Effect::LoadFilePatch { .. }));
}

#[test]
fn opening_a_file_draws_its_card_and_a_wider_read_brings_context_in() {
    let (mut rig, mut app) = start(|repo| {
        let long: String = (0..60).map(|i| format!("line {i}\n")).collect();
        write(repo, "long.txt", &long);
        git(repo, &["add", "."]);
        git(repo, &["commit", "-m", "long"]);
        write(repo, "long.txt", &long.replace("line 30\n", "line thirty\n"));
    });
    let mut view = view_of(&rig);
    open(&mut rig, &mut app, &mut view);
    let c = review_core(&mut app, KEY).unwrap();
    assert_eq!(c.scope_patches["long.txt"].hunks[0].lines.len(), 8, "three lines of context either side of the change");

    // Files start folded: click the row in the column and the card opens, with its code painted.
    until(&mut rig, &mut app, &mut view, "the row", &mut |_| rect_of(("diff-tree-file", format!("review-{KEY}").as_str(), "long.txt")).is_some());
    click(&mut rig, &mut app, &mut view, ("diff-tree-file", format!("review-{KEY}").as_str(), "long.txt"));
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(review_core(&mut app, KEY).unwrap().selected_path.as_deref(), Some("long.txt"));
    assert!(rect_of(("diff-file", format!("review-{KEY}").as_str(), "long.txt")).unwrap().h > 64.0, "the card is open");
    assert!(!rig.scene.layers[0].glyphs.is_empty());

    // Expanding asks the host for the next wider read, and the hunk grows.
    let Some(effect) = review_core(&mut app, KEY).unwrap().expand_file_context("long.txt") else { panic!("no read") };
    let mewrk_pane_git::review_core::Effect::LoadFilePatch { path, request_id, step, request } = effect else { panic!() };
    let answer: Value = app.host.invoke_blocking("get_git_diff", json!({ "target": rig.target, "request": request })).unwrap();
    let answer: GitDiffResponse = serde_json::from_value(answer).unwrap();
    review_core(&mut app, KEY).unwrap().file_patch_answer(&path, request_id, step, Ok(answer));
    let c = review_core(&mut app, KEY).unwrap();
    assert_eq!(c.diff_patches()["long.txt"].hunks[0].lines.len(), 52, "25 lines of context either side, and the change");
}

#[test]
fn a_conflicted_merge_shows_its_banner_and_abort_takes_two_clicks() {
    let (mut rig, mut app) = start(|repo| {
        git(repo, &["checkout", "-b", "side"]);
        write(repo, "src/app.ts", "const keep = 1;\nconst side = 2;\nconst tail = 3;\n");
        git(repo, &["commit", "-am", "side"]);
        git(repo, &["checkout", "main"]);
        write(repo, "src/app.ts", "const keep = 1;\nconst mine = 2;\nconst tail = 3;\n");
        git(repo, &["commit", "-am", "mine"]);
        let _ = Command::new("git").args(["merge", "side"]).current_dir(repo).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
    });
    let mut view = view_of(&rig);
    open(&mut rig, &mut app, &mut view);
    let c = review_core(&mut app, KEY).unwrap();
    assert_eq!(c.current_snapshot.operation, Some(GitRepositoryOperation::Merge));
    assert_eq!(c.current_snapshot.conflicted, 1);
    assert_eq!(c.operation_note().as_deref(), Some("仍有 1 个冲突；解决并暂存后再继续"));
    assert_eq!(c.diff_entries()[0].status, DiffEntryStatus::Conflicted);

    until(&mut rig, &mut app, &mut view, "the banner", &mut |_| rect_of(("review-banner", KEY, "abort")).is_some());
    click(&mut rig, &mut app, &mut view, ("review-banner", KEY, "abort"));
    assert!(review_core(&mut app, KEY).unwrap().pending_git_action.as_deref().unwrap().starts_with("abort-operation:"));
    assert!(rig.repo.join(".git/MERGE_HEAD").exists(), "the first click only arms it");
    click(&mut rig, &mut app, &mut view, ("review-banner", KEY, "abort"));
    until(&mut rig, &mut app, &mut view, "the abort", &mut |app| review_core(app, KEY).is_some_and(|c| c.current_snapshot.operation.is_none() && c.busy_action.is_none()));
    assert!(!rig.repo.join(".git/MERGE_HEAD").exists());
    assert!(read(&rig.repo, "src/app.ts").contains("mine"));
}

#[test]
fn a_narrow_pane_puts_the_operation_buttons_on_a_line_of_their_own() {
    let (mut rig, mut app) = start(|repo| {
        git(repo, &["checkout", "-b", "side"]);
        write(repo, "src/app.ts", "const keep = 1;\nconst side = 2;\nconst tail = 3;\n");
        git(repo, &["commit", "-am", "side"]);
        git(repo, &["checkout", "main"]);
        write(repo, "src/app.ts", "const keep = 1;\nconst mine = 2;\nconst tail = 3;\n");
        git(repo, &["commit", "-am", "mine"]);
        let _ = Command::new("git").args(["merge", "side"]).current_dir(repo).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output();
    });
    let mut wide = view_of(&rig);
    open(&mut rig, &mut app, &mut wide);
    until(&mut rig, &mut app, &mut wide, "the banner", &mut |_| rect_of(("review-banner", KEY, "abort")).is_some());
    frame(&mut rig, &mut app, &mut wide);
    let (bar, abort) = (rect_of(("review-banner-row", KEY)).unwrap(), rect_of(("review-banner", KEY, "abort")).unwrap());
    assert!(abort.y < bar.y + 12.0, "beside the words while there is room: {abort:?} in {bar:?}");

    let mut inner = view_of(&rig);
    let mut narrow = move |app: &mut App| -> E { col().child(col().w(400.0).h(600.0).child(inner(app))) };
    for _ in 0..3 {
        frame(&mut rig, &mut app, &mut narrow);
    }
    let (bar, abort) = (rect_of(("review-banner-row", KEY)).unwrap(), rect_of(("review-banner", KEY, "abort")).unwrap());
    assert!(bar.w <= 400.0, "the banner keeps to the pane: {bar:?}");
    assert!(abort.y > bar.y + 20.0, "under them: {abort:?} in {bar:?}");
    assert!((abort.right() - (bar.right() - 8.0)).abs() < 1.0, "against the right edge: {abort:?} in {bar:?}");
}

#[test]
fn a_click_elsewhere_disarms_an_armed_operation_button() {
    let (mut rig, mut app) = start(|repo| {
        git(repo, &["checkout", "-b", "side"]);
        write(repo, "src/app.ts", "side\n");
        git(repo, &["commit", "-am", "side"]);
        git(repo, &["checkout", "main"]);
        write(repo, "src/app.ts", "mine\n");
        git(repo, &["commit", "-am", "mine"]);
        let _ = Command::new("git").args(["merge", "side"]).current_dir(repo).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
    });
    let mut view = view_of(&rig);
    open(&mut rig, &mut app, &mut view);
    until(&mut rig, &mut app, &mut view, "the banner", &mut |_| rect_of(("review-banner", KEY, "abort")).is_some());
    click(&mut rig, &mut app, &mut view, ("review-banner", KEY, "abort"));
    assert!(review_core(&mut app, KEY).unwrap().pending_git_action.is_some());
    rig.input.pointer = Some((5.0, WIN.1 - 5.0));
    frame(&mut rig, &mut app, &mut view);
    rig.input.pressed = true;
    rig.input.down = true;
    frame(&mut rig, &mut app, &mut view);
    rig.input.down = false;
    rig.input.released = true;
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(review_core(&mut app, KEY).unwrap().pending_git_action, None);
}

#[test]
fn a_write_the_workspace_cannot_take_is_refused_in_place() {
    let (mut rig, mut app) = start(|repo| write(repo, "README.md", "# changed\n"));
    let mut view = view_of(&rig);
    open(&mut rig, &mut app, &mut view);
    rig.allow_mutation.set(false);
    mewrk_pane_git::review::file_action(&mut app, KEY, "README.md", "stage");
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(review_core(&mut app, KEY).unwrap().operation_error.as_deref(), Some("另一个任务或工作区操作已经开始，请稍后重试"));
    assert!(porcelain(&rig).lines().any(|l| l == " M README.md"));

    // And while the workspace is busy, the rows say so before anyone clicks.
    rig.allow_mutation.set(true);
    *rig.disabled_reason.borrow_mut() = Some("内置终端正在执行命令，结束后才能执行 Git 写操作".into());
    frame(&mut rig, &mut app, &mut view);
    assert!(review_core(&mut app, KEY).unwrap().file_actions("README.md").iter().all(|a| a.disabled));
    mewrk_pane_git::review::file_action(&mut app, KEY, "README.md", "stage");
    frame(&mut rig, &mut app, &mut view);
    assert!(porcelain(&rig).lines().any(|l| l == " M README.md"));
}

#[test]
fn branches_are_listed_and_a_checkout_moves_head_through_the_same_write() {
    let (mut rig, mut app) = start(|repo| {
        git(repo, &["branch", "feature/one"]);
        git(repo, &["branch", "feature/two"]);
    });
    let mut view = view_of(&rig);
    refresh_summary(&rig, &mut app);
    until(&mut rig, &mut app, &mut view, "the summary", &mut known);

    let answer = app.host.invoke_blocking("get_git_branches", json!({ "target": rig.target })).unwrap();
    let branches: GitBranchesResult = serde_json::from_value(answer).unwrap();
    let local = mewrk_pane_git::branch::switchable_branches(branches);
    let mut names: Vec<&str> = local.iter().map(|b| b.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["feature/one", "feature/two", "main"]);
    assert!(local.iter().find(|b| b.name == "main").unwrap().current);

    let refreshed = Rc::new(std::cell::Cell::new(0));
    let hooks = {
        let a = refreshed.clone();
        BranchChipHooks {
            begin_mutation: Rc::new(|_| true),
            end_mutation: Rc::new(|_| {}),
            refresh: Rc::new(move |_| a.set(a.get() + 1)),
            on_worktree_changed: Rc::new(|_, _| {}),
            on_draft_worktree_toggled: None,
        }
    };
    checkout_branch(&mut app, "chip", &rig.target, "feature/one", &hooks);
    let start = Instant::now();
    while git(&rig.repo, &["branch", "--show-current"]).trim() != "feature/one" {
        assert!(start.elapsed() < Duration::from_secs(20));
        frame(&mut rig, &mut app, &mut view);
        std::thread::sleep(Duration::from_millis(5));
    }
    // HEAD moves before the host has read the snapshot and answered, which takes a while where starting a process is slow.
    let start = Instant::now();
    while refreshed.get() == 0 {
        assert!(start.elapsed() < Duration::from_secs(20), "the checkout's snapshot is read again either way");
        frame(&mut rig, &mut app, &mut view);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(branch_chip_error(&mut app, "chip"), None);
    assert_eq!(refreshed.get(), 1, "the checkout's snapshot is read again either way");

    // Git's own refusal comes back as what the chip says.
    checkout_branch(&mut app, "chip", &rig.target, "no-such-branch", &hooks);
    let start = Instant::now();
    while branch_chip_error(&mut app, "chip").is_none() {
        assert!(start.elapsed() < Duration::from_secs(20));
        frame(&mut rig, &mut app, &mut view);
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_conversation_worktree_is_created_reviewed_against_its_fork_point_and_released() {
    let (mut rig, mut app) = start(|_| {});
    mewrk_client::doc::init(&mut app);
    let conversation: model::Conversation = serde_json::from_value(json!({ "id": SNAPSHOT_KEY, "title": "t", "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z", "settings": { "enabledTools": [] }, "contexts": [] })).unwrap();
    let created = app.host.invoke_blocking("create_conversation", json!({ "workspaceId": "ws1", "conversation": conversation })).unwrap();
    let created: model::Conversation = serde_json::from_value(created).unwrap();
    app.doc.workspaces[0].conversations.insert(0, created);
    let registered = model::AttachedWorkspace { machine: None, path: rig.repo.to_str().unwrap().to_owned() };

    // The record the host resolves the checkout through, as the chip's hook keeps it.
    let saved = Rc::new(std::cell::Cell::new(0));
    let hooks = {
        let (registered, saved) = (registered.clone(), saved.clone());
        BranchChipHooks {
            begin_mutation: Rc::new(|_| true),
            end_mutation: Rc::new(|_| {}),
            refresh: Rc::new(|_| {}),
            on_worktree_changed: Rc::new(move |app, record| {
                let registered = registered.clone();
                app.update_conversation("ws1", SNAPSHOT_KEY, |c| c.worktrees = mewrk_client::workspaces::with_conversation_worktree(&c.worktrees, 1, &registered, record));
                let task = app.task();
                let flushed = saved.clone();
                flushed.set(flushed.get() + 1);
                app.flush_latest_document(true, move |_, result| {
                    result.unwrap();
                    task.post(|_| {});
                });
            }),
            on_draft_worktree_toggled: None,
        }
    };
    let mut nothing = |_: &mut App| -> E { div() };
    toggle_conversation_worktree(&mut app, "chip", SNAPSHOT_KEY, 1, true, &hooks);
    until(&mut rig, &mut app, &mut nothing, "the worktree", &mut |app| !app.doc.workspaces[0].conversations[0].worktrees.is_empty());
    assert_eq!(branch_chip_error(&mut app, "chip"), None);
    let worktree = app.doc.workspaces[0].conversations[0].worktrees[0].clone();
    let tree = PathBuf::from(&worktree.path);
    assert!(tree.is_dir());
    assert!(!worktree.base_oid.is_empty());
    let settle_save = Instant::now();
    while settle_save.elapsed() < Duration::from_millis(300) {
        frame(&mut rig, &mut app, &mut nothing);
        std::thread::sleep(Duration::from_millis(5));
    }

    // What the conversation committed there, and what it has not.
    write(&tree, "committed.txt", "c\n");
    git(&tree, &["add", "."]);
    git(&tree, &["commit", "-m", "work"]);
    write(&tree, "README.md", "# changed\n");

    rig.target = git_conversation_target(SNAPSHOT_KEY, None);
    *rig.checkout.borrow_mut() = GitReviewCheckout { worktree_name: Some(mewrk_client::workspaces::worktree_name(&worktree)), base_branch: worktree.base_branch.clone(), base_oid: Some(worktree.base_oid.clone()) };
    let mut view = view_of(&rig);
    refresh_summary(&rig, &mut app);
    until(&mut rig, &mut app, &mut view, "the branch's whole change", &mut |app| review_core(app, KEY).is_some_and(|c| c.change_files.len() == 2 && c.diff_state == RequestState::Ready));
    let c = review_core(&mut app, KEY).unwrap();
    assert_eq!(c.diff_scope, DiffScope::Branch, "it opens on the branch's whole change");
    assert_eq!(c.scope_note(), None);
    assert_eq!(c.branch_label(), "main", "read against the branch it was forked from");
    assert_eq!(c.total_changed_files(), 2);
    assert_eq!(Path::new(&c.current_snapshot.worktree_root), tree.canonicalize().unwrap());
    assert!(c.scope_patches.contains_key("committed.txt") && c.scope_patches.contains_key("README.md"), "{:?}", c.scope_patches.keys());

    // Narrowed to what is not committed, the commit leaves.
    review_core(&mut app, KEY).unwrap().diff_scope = DiffScope::Working;
    until(&mut rig, &mut app, &mut view, "the uncommitted scope", &mut |app| review_core(app, KEY).is_some_and(|c| c.diff_state == RequestState::Ready && c.scope_patches.len() == 1 && c.scope_patches.contains_key("README.md")));
    assert_eq!(review_core(&mut app, KEY).unwrap().scope_note(), Some("未提交"));

    // Releasing keeps a tree that holds work, says so, and clears the record all the same.
    until(&mut rig, &mut app, &mut view, "the reads to land", &mut settled);
    let mut attempts = 0;
    loop {
        toggle_conversation_worktree(&mut app, "chip", SNAPSHOT_KEY, 1, false, &hooks);
        until(&mut rig, &mut app, &mut view, "the release", &mut |app| app.doc.workspaces[0].conversations[0].worktrees.is_empty() || branch_chip_error(app, "chip").is_some());
        if app.doc.workspaces[0].conversations[0].worktrees.is_empty() {
            break;
        }
        // A read that was out when it asked holds the checkout for a moment; the chip says so, and asking again works.
        let error = branch_chip_error(&mut app, "chip").unwrap();
        assert!(error.contains("active lease"), "{error}");
        attempts += 1;
        assert!(attempts < 50, "{error}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(branch_chip_error(&mut app, "chip").as_deref(), Some("工作树里还有未提交的改动，目录与分支已保留"));
    assert!(tree.is_dir());
    assert_eq!(saved.get(), 2);
}

/// The right-click on a line of a diff, through to the hook, on a real patch.
#[test]
fn a_right_clicked_line_can_be_mentioned_in_the_conversation() {
    let (mut rig, mut app) = start(|repo| write(repo, "src/app.ts", "const keep = 1;\nconst next = 2;\nconst tail = 3;\n"));
    refresh_summary(&rig, &mut app);
    let mentioned = Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut view = {
        let (target, mentioned) = (rig.target.clone(), mentioned.clone());
        move |app: &mut App| -> E {
            let Some(snapshot) = git_controller(app).snapshots.get(SNAPSHOT_KEY).and_then(|e| e.snapshot.clone()) else { return div() };
            let inputs = ReviewInputs { target: target.clone(), snapshot, active: true, checkout: GitReviewCheckout::default(), reveal_request: None, mutation_disabled_reason: None };
            let mentioned = mentioned.clone();
            let hooks = GitReviewHooks { on_mention_line: Some(Rc::new(move |_, path, line| mentioned.borrow_mut().push((path.to_owned(), line)))), ..GitReviewHooks::default() };
            git_review_panel(app, GitReviewProps { key: KEY.to_owned(), inputs, hooks, page_tabs: None }).body
        }
    };
    until(&mut rig, &mut app, &mut view, "the review", &mut settled);
    let viewer = format!("review-{KEY}");
    until(&mut rig, &mut app, &mut view, "the header", &mut |_| rect_of(("diff-file-toggle", viewer.as_str(), "", "src/app.ts")).is_some());
    click(&mut rig, &mut app, &mut view, ("diff-file-toggle", viewer.as_str(), "", "src/app.ts"));
    frame(&mut rig, &mut app, &mut view);
    let card = rect_of(("diff-file", viewer.as_str(), "src/app.ts")).unwrap();
    // Under the header: the context line, then the removal of old line 2.
    rig.input.pointer = Some((card.right() - 40.0, card.y + 32.0 + 17.0 + 8.0));
    frame(&mut rig, &mut app, &mut view);
    rig.input.right_pressed = true;
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    let menu = ("review-line-menu", KEY);
    click(&mut rig, &mut app, &mut view, menu_row_key(menu, &[], "mention"));
    assert_eq!(mentioned.borrow().as_slice(), [("src/app.ts".to_owned(), 2)]);
    assert!(rect_of(menu_panel_key(menu, &[])).is_none(), "the menu goes with the choice");
}

/// The branch chip and the status chip, clicked, on a real repository.
#[test]
fn the_composer_chips_open_list_and_switch() {
    let (mut rig, mut app) = start(|repo| {
        git(repo, &["branch", "feature/one"]);
        git(repo, &["branch", "feature/two"]);
        write(repo, "README.md", "# changed\n");
    });
    refresh_summary(&rig, &mut app);
    let opened = Rc::new(std::cell::Cell::new(0));
    let refreshed = Rc::new(std::cell::Cell::new(0));
    let mut view = {
        let (target, opened, refreshed) = (rig.target.clone(), opened.clone(), refreshed.clone());
        move |app: &mut App| -> E {
            let Some(snapshot) = git_controller(app).snapshots.get(SNAPSHOT_KEY).and_then(|e| e.snapshot.clone()) else { return div() };
            let refreshed = refreshed.clone();
            let hooks = BranchChipHooks { begin_mutation: Rc::new(|_| true), end_mutation: Rc::new(|_| {}), refresh: Rc::new(move |_| refreshed.set(refreshed.get() + 1)), on_worktree_changed: Rc::new(|_, _| {}), on_draft_worktree_toggled: None };
            let chips = branch_chips(app, BranchChipProps { key: "chip".into(), conversation_id: SNAPSHOT_KEY.into(), member: 1, target: Some(target.clone()), branch_label: snapshot.branch.clone(), disabled: false, has_worktree: false, worktree_checked: false, offers_worktree: true, is_draft: false, hooks });
            let opened = opened.clone();
            let card = git_status_card("card", &snapshot, false, Rc::new(move |_| opened.set(opened.get() + 1)));
            col().child(div().h(500.0)).child(row().gap(10.0).child(chips).child(card))
        }
    };
    until(&mut rig, &mut app, &mut view, "the summary", &mut known);
    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);

    // The status card opens, and its rows lead to the review.
    let card = hash_key(("git-status-card", "card"));
    click(&mut rig, &mut app, &mut view, card);
    frame(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("git-status-card-next", card));
    assert_eq!(opened.get(), 1);
    click(&mut rig, &mut app, &mut view, ("git-status-card-changes", card));
    assert_eq!(opened.get(), 2);
    click(&mut rig, &mut app, &mut view, card);

    // The branch menu reads the branches when it opens, and a row switches to one.
    click(&mut rig, &mut app, &mut view, hash_key(("branch-chip", "chip")));
    until(&mut rig, &mut app, &mut view, "the branches", &mut |_| rect_of(("branch-row", "chip", "feature/two")).is_some());
    frame(&mut rig, &mut app, &mut view);
    click(&mut rig, &mut app, &mut view, ("branch-row", "chip", "feature/two"));
    let start = Instant::now();
    while git(&rig.repo, &["branch", "--show-current"]).trim() != "feature/two" || refreshed.get() == 0 {
        assert!(start.elapsed() < Duration::from_secs(20), "the switch");
        frame(&mut rig, &mut app, &mut view);
        std::thread::sleep(Duration::from_millis(5));
    }
    frame(&mut rig, &mut app, &mut view);
    assert!(rect_of(("branch-panel", "chip")).is_none(), "the menu closes when the switch lands");
    assert_eq!(read(&rig.repo, "README.md"), "# changed\n", "a checkout that Git allows carries the change along");
}

/// `gitReviewListsPath`: what the timeline asks before it offers to open a file.
#[test]
fn the_review_is_asked_by_name_whether_it_lists_a_path() {
    let (mut rig, mut app) = start(|repo| {
        write(repo, "README.md", "# changed\n");
        write(repo, "draft.txt", "untracked\n");
    });
    let mut view = view_of(&rig);
    open(&mut rig, &mut app, &mut view);
    let snapshot = review_core(&mut app, KEY).unwrap().current_snapshot.clone();
    let task = app.task();
    assert!(git_review_lists_path(&task, &rig.target, &snapshot, "README.md", None).unwrap());
    assert!(!git_review_lists_path(&task, &rig.target, &snapshot, "draft.txt", None).unwrap(), "untracked files are out of review scope");
    assert!(!git_review_lists_path(&task, &rig.target, &snapshot, "nope.txt", None).unwrap());
    // A snapshot gone stale in the meantime is answered with the current one, which is asked once more.
    write(&rig.repo, "src/app.ts", "moved on\n");
    assert!(git_review_lists_path(&task, &rig.target, &snapshot, "src/app.ts", None).unwrap(), "a file only the newer summary knows");
}

/// A controller refresh, from the host's answer to what the snapshots hold, with a lease fencing a late one out.
#[test]
fn a_refresh_lands_in_the_controller_and_an_unchanged_one_keeps_what_is_there() {
    let (rig, mut app) = start(|repo| write(repo, "README.md", "# changed\n"));
    let answered = Arc::new(Mutex::new(Vec::new()));
    let pump = |app: &mut App, n: usize, answered: &Arc<Mutex<Vec<bool>>>| {
        let start = Instant::now();
        while answered.lock().unwrap().len() < n {
            assert!(start.elapsed() < Duration::from_secs(20));
            app.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    let log = answered.clone();
    git_refresh(&mut app, SNAPSHOT_KEY, "ws1", &rig.target, move |_, result| log.lock().unwrap().push(result.unwrap().is_some()));
    pump(&mut app, 1, &answered);
    let first = git_controller(&mut app).snapshots[SNAPSHOT_KEY].snapshot.clone().unwrap();
    assert_eq!(first.changed_files, 1);
    let revision = git_controller(&mut app).revision;

    let log = answered.clone();
    git_refresh(&mut app, SNAPSHOT_KEY, "ws1", &rig.target, move |_, result| log.lock().unwrap().push(result.unwrap().is_some()));
    pump(&mut app, 2, &answered);
    assert_eq!(git_controller(&mut app).revision, revision, "nothing moved, so nobody hears of it");

    // A poll out when a write starts must not land after it.
    write(&rig.repo, "README.md", "# changed again\n");
    let log = answered.clone();
    git_refresh(&mut app, SNAPSHOT_KEY, "ws1", &rig.target, move |_, result| log.lock().unwrap().push(result.unwrap().is_some()));
    git_controller(&mut app).acquire_mutation_lease(SNAPSHOT_KEY, &[SNAPSHOT_KEY.to_owned()]);
    pump(&mut app, 3, &answered);
    assert_eq!(git_controller(&mut app).snapshots[SNAPSHOT_KEY].snapshot.as_ref().unwrap().summary_revision, first.summary_revision);
}

#[test]
fn the_host_refuses_what_upstream_never_asks_for_a_commit_action_does_not_exist() {
    let (rig, app) = start(|_| {});
    let refused = app.host.invoke_blocking("execute_git_action", json!({ "target": rig.target, "action": { "type": "commit", "message": "x" } }));
    assert!(refused.is_err(), "Mewrk itself commits nothing: the model or a terminal does");
}

// ---------------------------------------------------------------- the accessibility tree

type Tree = Vec<(accesskit::NodeId, accesskit::Node)>;

fn a11y_tree(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) -> Tree {
    set_a11y_active(true);
    for _ in 0..4 {
        frame(rig, app, view);
    }
    take_a11y_update().expect("a client gets the tree").nodes
}

fn said<'a>(tree: &'a Tree, words: &str) -> &'a accesskit::Node {
    tree.iter().map(|(_, node)| node).find(|node| node.label() == Some(words) || (node.role() == Role::Label && node.value() == Some(words))).unwrap_or_else(|| panic!("nothing says {words:?}: {:?}", tree.iter().filter_map(|(_, n)| n.label().or(n.value())).collect::<Vec<_>>()))
}

fn nameless(tree: &Tree) -> Vec<String> {
    tree.iter()
        .filter(|(_, node)| matches!(node.role(), Role::Button | Role::Link | Role::TextInput | Role::MultilineTextInput | Role::Tab | Role::Switch | Role::CheckBox | Role::MenuItem | Role::ComboBox) || node.supports_action(accesskit::Action::Click))
        .filter(|(_, node)| node.label().is_none_or(str::is_empty) && node.labelled_by().is_empty() && (matches!(node.role(), Role::TextInput | Role::MultilineTextInput) || node.value().is_none_or(str::is_empty)))
        .map(|(id, node)| format!("{:?} {id:?}", node.role()))
        .collect()
}

#[test]
fn the_review_reads_as_its_sections_with_every_control_named() {
    let (mut rig, mut app) = start(|repo| write(repo, "src/app.ts", "const keep = 1;\nconst next = 2;\nconst tail = 3;\n"));
    let mut view = view_of(&rig);
    open(&mut rig, &mut app, &mut view);
    let tree = a11y_tree(&mut rig, &mut app, &mut view);
    assert_eq!(said(&tree, "Git 审阅").role(), Role::Region);
    assert_eq!(said(&tree, "变更").role(), Role::Region);
    assert_eq!(said(&tree, "变更文件").role(), Role::Navigation);
    assert_eq!(said(&tree, "筛选变更文件").role(), Role::TextInput);
    let toggle = tree.iter().map(|(_, n)| n).find(|n| matches!(n.label(), Some("隐藏文件" | "显示文件"))).expect("the file column's toggle");
    assert_eq!((toggle.role(), toggle.toggled()), (Role::Button, Some(accesskit::Toggled::True)));
    assert_eq!(said(&tree, "显示 1/1 个变更文件").role(), Role::Status);
    assert_eq!(said(&tree, "审阅范围").role(), Role::Button);
    assert_eq!(nameless(&tree), Vec::<String>::new());

    set_a11y_active(false);
}
