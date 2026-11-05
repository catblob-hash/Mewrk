//! The windows at the root, driven as a user would.

use mewrk_client::model::{AttachedWorkspace, RunTarget};
use mewrk_client::prelude::*;
use mewrk_shell::dialogs::error_view::{error_view, FatalApp};
use mewrk_shell::dialogs::path_choice::{candidates_for, workspace_relative_path};
use mewrk_shell::dialogs::project::{can_submit, duplicate_rows, project_workspace_key, ProjectRow};
use mewrk_ui_chat::rewind::{rewind_preview_summary, rewind_result_summary, summarize_rewind_files, RewindFileEntry};
use mewrk_shell::dialogs::search::{search_entries, search_results};
use mewrk_shell::state::{shell, with_ssh_prompt, without_ssh_prompt, ConfirmationRequest, SshPrompt, SshPromptKind};

use crate::rig::*;

#[test]
fn search_lists_recent_first_narrows_by_every_word_and_opens_with_enter() {
    let (mut rig, mut app) = Rig::new("search");
    let first = rig.add_conversation(&mut app, "ws1", "fix the build");
    let second = rig.add_conversation(&mut app, "ws2", "write the docs");
    let entries = search_entries(&app.doc.workspaces, &app.unloaded);
    assert_eq!(search_results(&entries, "docs beta").len(), 1, "a word from the title and one from the project");
    assert_eq!(search_results(&entries, "docs alpha").len(), 0);

    rig.click(&mut app, "shell-nav-search");
    rig.frames(&mut app, 2);
    rig.type_text(&mut app, "fix");
    rig.plain(&mut app, Key::Enter);
    assert_eq!(app.active_conversation_id().as_deref(), Some(first.as_str()));
    assert!(!shell(&mut app).search_open);

    // The arrows walk the list, and wrap.
    rig.click(&mut app, "shell-nav-search");
    rig.frames(&mut app, 2);
    rig.type_text(&mut app, "the");
    rig.plain(&mut app, Key::Down);
    rig.plain(&mut app, Key::Down);
    rig.plain(&mut app, Key::Up);
    rig.plain(&mut app, Key::Enter);
    assert_eq!(app.active_conversation_id().as_deref(), Some(second.as_str()));

    // Escape and a press on the backdrop both leave it.
    rig.click(&mut app, "shell-nav-search");
    rig.frames(&mut app, 2);
    rig.plain(&mut app, Key::Escape);
    assert!(!shell(&mut app).search_open);
    rig.click(&mut app, "shell-nav-search");
    rig.frames(&mut app, 2);
    rig.click_at(&mut app, 10.0, 880.0);
    assert!(!shell(&mut app).search_open);
}

#[test]
fn a_click_on_a_result_opens_it() {
    let (mut rig, mut app) = Rig::new("search-click");
    let id = rig.add_conversation(&mut app, "ws2", "second");
    rig.click(&mut app, "shell-nav-search");
    rig.click(&mut app, ("conversation-search-result", "ws2", id.as_str()));
    assert_eq!(app.active_conversation_id().as_deref(), Some(id.as_str()));
}

#[test]
fn a_confirmation_runs_its_answer_once_or_not_at_all() {
    let (mut rig, mut app) = Rig::new("confirm");
    let hits = Rc::new(std::cell::Cell::new(0));
    let counter = hits.clone();
    let request = ConfirmationRequest { question: "Sure?".into(), detail: Some("It goes.".into()), confirm_label: "Delete".into(), destructive: true, on_confirm: Rc::new(move |_| counter.set(counter.get() + 1)) };
    shell(&mut app).confirmation = Some(request.clone());
    rig.click(&mut app, "confirm-cancel");
    assert_eq!(hits.get(), 0);
    assert!(shell(&mut app).confirmation.is_none());
    shell(&mut app).confirmation = Some(request);
    rig.click(&mut app, "confirm-ok");
    assert_eq!(hits.get(), 1);
    assert!(shell(&mut app).confirmation.is_none());
}

fn row(id: &str, machine: Option<RunTarget>, path: &str, locked: bool) -> ProjectRow {
    ProjectRow { id: id.to_owned(), machine, path: path.to_owned(), locked }
}

#[test]
fn a_project_row_repeats_only_the_same_directory_on_the_same_machine() {
    let wsl = Some(RunTarget::Wsl { distro: "Ubuntu".into() });
    let rows = vec![row("a", None, "/src/app", true), row("b", wsl.clone(), "/src/app", false), row("c", None, "/src/app", false), row("d", None, "C:\\App", false), row("e", None, "c:\\app", false)];
    let duplicates = duplicate_rows(&rows);
    assert!(duplicates.contains("c") && duplicates.contains("e"));
    assert!(!duplicates.contains("b"), "another machine");
    assert!(!can_submit(&rows));
    assert!(!can_submit(&[row("a", None, "   ", false)]), "a row without a directory");
    assert!(can_submit(&[row("a", None, "/one", false), row("b", wsl, "/one", false)]));
    assert_eq!(project_workspace_key(None, "  /x "), format!("{} /x", mewrk_client::workspaces::run_env_key(None)));
}

#[test]
fn the_new_project_dialog_grows_rows_and_edit_locks_the_first() {
    let (mut rig, mut app) = Rig::new("project");
    rig.click(&mut app, "new-project");
    assert!(app.workspace_dialog_open());
    rig.click(&mut app, ("project-add", "create"));
    rig.frames(&mut app, 2);
    assert!(rig.exists(&mut app, ("project-remove", "create", "row-1")));
    rig.click(&mut app, ("project-remove", "create", "row-1"));
    rig.frames(&mut app, 6);
    assert!(!rig.exists(&mut app, ("project-remove", "create", "row-1")));
    // Nothing chosen yet, so nothing to create.
    rig.click(&mut app, ("project-submit", "create"));
    assert!(app.workspace_dialog_open());
    rig.click(&mut app, ("project-cancel", "create"));
    assert!(!app.workspace_dialog_open());

    // Editing: the name is saved, and the first workspace stays.
    let path = workspace_of(&app, "ws1").path.clone();
    app.set_project_editor(Some("ws1".to_owned()));
    rig.frames(&mut app, 3);
    rig.click(&mut app, fluent::el::hash_key(("field", ("project-name", "edit:ws1"))));
    rig.frames(&mut app, 2);
    rig.cmd(&mut app, 'a');
    rig.type_text(&mut app, "renamed");
    rig.click(&mut app, ("project-submit", "edit:ws1"));
    assert!(app.project_editor().is_none());
    assert_eq!(workspace_of(&app, "ws1").name, "renamed");
    assert_eq!(workspace_of(&app, "ws1").path, path);
}

#[test]
fn ssh_prompts_queue_oldest_first_and_an_answer_reaches_the_host() {
    let prompt = |id: &str| SshPrompt { id: id.to_owned(), machine: "build".to_owned(), kind: SshPromptKind::Confirm, prompt: "Allow?".to_owned(), host_key: None, retry: false };
    let mut queue = Vec::new();
    with_ssh_prompt(&mut queue, prompt("a"));
    with_ssh_prompt(&mut queue, prompt("b"));
    with_ssh_prompt(&mut queue, prompt("a"));
    assert_eq!(queue.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    without_ssh_prompt(&mut queue, "a");
    assert_eq!(queue[0].id, "b");

    let (mut rig, mut app) = Rig::new("ssh");
    shell(&mut app).ssh_prompts.push(prompt("gone"));
    rig.click(&mut app, ("ssh-deny", "gone"));
    // The host has no such question; whichever way it answers, the dialog is not stuck sending.
    rig.frames(&mut app, 4);
    assert!(rig.exists(&mut app, ("dialog-close", fluent::el::hash_key(("dialog", ("ssh", "gone"))))) || shell(&mut app).ssh_prompts.is_empty());
}

fn file(path: &str, action: &str, skipped: Option<&str>) -> RewindFileEntry {
    RewindFileEntry { path: path.to_owned(), display_path: path.to_owned(), action: action.to_owned(), skipped: skipped.map(str::to_owned) }
}

#[test]
fn the_rewind_summaries_say_what_upstream_says() {
    mewrk_client::i18n::set_english(true);
    let files = [file("a", "restore", None), file("b", "restore", None), file("c", "delete", None), file("d", "restore", Some("changed since"))];
    let summary = summarize_rewind_files(&files);
    assert_eq!((summary.restored, summary.deleted, summary.skipped), (2, 1, 1));
    assert_eq!(rewind_preview_summary(&summary), "Will restore 2 files, delete 1 file created since then; 1 will be skipped");
    assert_eq!(rewind_result_summary(&summary), "restored 2 files, deleted 1 file created since then; skipped 1: changed since");
    assert_eq!(rewind_preview_summary(&summarize_rewind_files(&[])), "No file will be restored");
}

#[test]
fn the_rewind_window_previews_through_the_real_host_and_restores_the_conversation() {
    let (mut rig, mut app) = Rig::new("rewind");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    use mewrk_ui_chat::handlers::{open_rewind_dialog, TimelineEdits};
    let context = app.conversation(&id).unwrap().contexts[0].clone();
    open_rewind_dialog(&mut app, &context, None);
    rig.until(&mut app, "the window", |app| app.local::<TimelineEdits>().rewind.is_some());
    // With no file changes there is nothing to restore, and the conversation half is free to go.
    rig.frames(&mut app, 4);
    rig.click(&mut app, "rewind-both");
    assert!(app.local::<TimelineEdits>().rewind.is_some(), "no files, so the both button does nothing");
    rig.click(&mut app, "rewind-conversation-only");
    rig.until(&mut app, "the rewind", |app| app.local::<TimelineEdits>().rewind.is_none());
    assert!(app.conversation(&id).unwrap().contexts.is_empty());
    assert_eq!(app.composer_draft(&id), "hello", "the message goes back to the composer");
}

#[test]
fn a_path_could_be_in_every_workspace_unless_it_says_which() {
    use mewrk_pane_files::links::PathOpenRequest;
    use mewrk_pane_files::pane::FilesPaneWorkspace;
    let workspaces = vec![FilesPaneWorkspace { number: 1, machine: None, path: "/one".into() }, FilesPaneWorkspace { number: 2, machine: Some(RunTarget::Wsl { distro: "U".into() }), path: "/two".into() }];
    let request = |path: &str, workspace: Option<u32>| PathOpenRequest { path: path.to_owned(), base_dir: None, line: None, workspace, review: false, new_page: false, machine: None, anchor: None };
    let relative = candidates_for(&request("a.txt", None), &workspaces);
    assert_eq!(relative.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["/one/a.txt", "/two/a.txt"]);
    let named = candidates_for(&request("a.txt", Some(2)), &workspaces);
    assert_eq!(named.len(), 1);
    assert_eq!(named[0].path, "/two/a.txt");
    assert!(candidates_for(&request("a.txt", Some(9)), &workspaces).is_empty());
    // An absolute path names a machine: every machine the conversation is on, and this one.
    let absolute = candidates_for(&request("/etc/hosts", None), &workspaces);
    assert_eq!(absolute.len(), 2);
    assert!(absolute.iter().all(|c| c.workspace.is_none() && c.path == "/etc/hosts"));
}

#[test]
fn a_path_inside_the_checkout_is_relative_to_it_and_one_that_escapes_is_not() {
    assert_eq!(workspace_relative_path("src/App.tsx", None, Some("/repo")).as_deref(), Some("src/App.tsx"));
    assert_eq!(workspace_relative_path("/repo/src/App.tsx", None, Some("/repo")).as_deref(), Some("src/App.tsx"));
    assert_eq!(workspace_relative_path("../x", Some("/repo/src"), Some("/repo")).as_deref(), Some("x"));
    assert_eq!(workspace_relative_path("../../x", Some("/repo/src"), Some("/repo")), None);
    assert_eq!(workspace_relative_path("/other/x", None, Some("/repo")), None);
    assert_eq!(workspace_relative_path("C:\\Repo\\a.txt", None, Some("c:/repo")).as_deref(), Some("a.txt"), "Windows compares without case");
    assert_eq!(workspace_relative_path("//server/share/a", None, Some("/repo")), None, "a network location is refused");
}

#[allow(dead_code)]
fn unused(_: AttachedWorkspace) {}

#[test]
fn closing_a_window_gives_the_keyboard_back_to_what_had_it() {
    let (mut rig, mut app) = Rig::new("focus-return");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    mewrk_shell::actions::open_pane(&mut app, "tasks");
    rig.frames(&mut app, 2);
    let handle = hash_key(("pane-tiles-handle", "side"));
    focus_request_id(handle);
    rig.frames(&mut app, 2);
    assert_eq!(focused_id(), handle);

    shell(&mut app).search_open = true;
    rig.frames(&mut app, 4);
    assert_ne!(focused_id(), handle, "the window holds the keyboard while it is open");

    // A window with nothing of its own to focus still takes the keyboard from what lies under it.
    shell(&mut app).search_open = false;
    rig.frames(&mut app, 4);
    assert_eq!(focused_id(), handle);
    shell(&mut app).confirmation = Some(ConfirmationRequest { question: "Sure?".to_owned(), detail: None, confirm_label: "Yes".to_owned(), destructive: false, on_confirm: Rc::new(|_: &mut App| {}) });
    rig.frames(&mut app, 4);
    assert_ne!(focused_id(), handle);
    shell(&mut app).confirmation = None;
    rig.frames(&mut app, 4);
    assert_eq!(focused_id(), handle);

    shell(&mut app).search_open = true;
    rig.frames(&mut app, 4);

    shell(&mut app).search_open = false;
    rig.frames(&mut app, 4);
    assert_eq!(focused_id(), handle);
}

#[test]
fn a_confirmation_holds_the_keyboard_so_enter_answers_it() {
    let (mut rig, mut app) = Rig::new("confirm-enter");
    let hits = Rc::new(std::cell::Cell::new(0));
    let counter = hits.clone();
    shell(&mut app).confirmation = Some(ConfirmationRequest { question: "Sure?".into(), detail: None, confirm_label: "Delete".into(), destructive: true, on_confirm: Rc::new(move |_| counter.set(counter.get() + 1)) });
    rig.frames(&mut app, 3);
    assert_eq!(rig.focused(), fluent::el::hash_key("confirm-ok"), "autoFocus");
    rig.plain(&mut app, Key::Enter);
    assert_eq!(hits.get(), 1);
    assert!(shell(&mut app).confirmation.is_none());
}

#[test]
fn a_project_already_registered_leaves_its_dialog_up_with_what_was_chosen() {
    let (mut rig, mut app) = Rig::new("project-refused");
    let existing = workspace_of(&app, "ws1").path.clone();
    let projects = app.doc.workspaces.len();
    app.set_workspace_dialog_open(true, false);
    rig.frames(&mut app, 2);
    mewrk_shell::dialogs::project::form(&mut app, "create").rows[0].path = existing.clone();
    rig.click(&mut app, ("project-submit", "create"));
    rig.frames(&mut app, 3);
    assert!(app.workspace_dialog_open(), "`if (existing) return`: nothing closed it");
    assert_eq!(app.doc.workspaces.len(), projects);
    assert_eq!(mewrk_shell::dialogs::project::form(&mut app, "create").rows[0].path, existing);

    // Leaving it is what forgets it, and however it closes, the next one starts over.
    app.set_workspace_dialog_open(false, false);
    rig.frames(&mut app, 2);
    app.set_workspace_dialog_open(true, false);
    rig.frames(&mut app, 2);
    assert_eq!(mewrk_shell::dialogs::project::form(&mut app, "create").rows[0].path, "");
}

/// `ErrorView` has no `App` to be part of, so it gets a window of its own.
struct ErrorWindow {
    app: FatalApp,
    input: fluent::ui::Input,
    mem: fluent::ui::UiMemory,
    scene: fluent::Scene,
    text: fluent::TextCtx,
}

impl ErrorWindow {
    fn new() -> Self {
        Self { app: FatalApp::new("The document could not be read".to_owned(), None, true), input: fluent::ui::Input::default(), mem: fluent::ui::UiMemory::default(), scene: fluent::Scene::default(), text: fluent::TextCtx::new(1.0) }
    }

    fn frame(&mut self) {
        let mut clipboard = fluent::ui::NoClipboard;
        let theme = fluent::Theme::light();
        let size = (1000.0, 700.0);
        self.scene.clear();
        let mut ui = fluent::Ui::new(&mut self.scene, &mut self.text, theme, &self.input, &mut self.mem, &mut clipboard, 1.0, size, 0.016, 0.0);
        fluent::el::set_theme(theme);
        fluent::el::set_window_size(size.0, size.1);
        let view = error_view(&mut self.app);
        fluent::el::render(view, &mut self.app, &mut ui);
        drop(ui);
        self.input.end_frame();
    }

    fn press_at(&mut self, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame();
        self.input.pressed = true;
        self.input.down = true;
        self.frame();
        self.input.released = true;
        self.input.down = false;
        self.frame();
        self.frame();
    }
}

#[test]
fn the_error_view_says_what_a_reset_leaves_moves_the_window_and_answers_on_enter() {
    let (_rig, _app) = Rig::new("error-view");
    let consequence = mewrk_shell::dialogs::error_view::consequence();
    assert!(consequence.contains("项目文件夹里的文件不受影响") || consequence.contains("Files in your project folders are not touched"), "{consequence}");
    let mut window = ErrorWindow::new();
    window.frame();
    window.frame();
    // The strip along the top, where the page has no row of its own.
    window.press_at(500.0, 20.0);
    assert!(std::mem::take(&mut window.app.drag_requested));
    assert!(!window.app.zoom_requested);
    // A second press on the same spot zooms it instead, and a third starts over.
    window.press_at(500.0, 20.0);
    assert!(std::mem::take(&mut window.app.zoom_requested));
    assert!(!window.app.drag_requested);
    window.press_at(500.0, 20.0);
    assert!(std::mem::take(&mut window.app.drag_requested));
    // The reset asks first, and the answer is one key away.
    let reset = fluent::el::rect_of("error-view-reset").expect("the button");
    window.press_at(reset.x + reset.w / 2.0, reset.y + reset.h / 2.0);
    assert!(!window.app.reset_requested);
    window.input.keys.push(KeyPress { key: Key::Enter, shift: false, cmd: false, alt: false, ctrl: false });
    window.frame();
    window.frame();
    assert!(window.app.reset_requested);
}
