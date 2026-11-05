//! The document against a real host, across real restarts.
//!
//! The host holds an exclusive lease on its data folder for the life of the process, so a restart has to be a new
//! process: `document_survives_restarts` runs this test binary again once per phase, selecting [`phase`] through the
//! environment. `HOME` points into the temp folder for those processes, and they never call `prepare_process`, which is
//! what installs the keychain-backed credential vault: nothing here reads a credential, so no keychain prompt can appear.
//!
//! A project normally gets its directory from the native picker, which can't be driven here. The host also accepts a
//! directory a project in its previous document already held, so the test seeds one project on disk between phases and
//! registers the new one on a directory that project holds; a directory nothing holds is what `add_workspace` is
//! refused on, which the test also checks, as the real path to the save failure state.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mewrk_client::doc::{self, DocumentSaveStatus, NewConversation};
use mewrk_client::drafts::{draft_conversation_id, visible_conversations};
use mewrk_client::model::{AttachedWorkspace, ContextItem, Conversation};
use mewrk_client::{App, StartOptions};
use serde_json::{json, Value};

const PHASE: &str = "MEWRK_DOC_TEST_PHASE";
const ROOT: &str = "MEWRK_DOC_TEST_ROOT";
/// Shaped like the ids the host mints, which are the ones it renews when copying.
const FIRST: &str = "ctx_0123456789abcdef0123456789abcdef";

fn start(root: &Path) -> App {
    let options = StartOptions { identifier: "mewrk.doc.test".into(), version: "0.0.0".into(), data_dir: root.join("data"), resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) };
    App::start(options).map_err(|e| format!("{e:?}")).expect("the host starts")
}

fn pump_until(app: &mut App, what: &str, mut done: impl FnMut(&mut App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        app.tick();
        if done(app) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Lets everything already asked for run: debounces, commands and their replies.
fn settle(app: &mut App, duration: Duration) {
    let until = Instant::now() + duration;
    pump_until(app, "the clock", |_| Instant::now() > until);
}

/// Waits for an answer delivered to a closure.
fn wait<T: Send + 'static>(app: &mut App, what: &str, ask: impl FnOnce(&mut App, Box<dyn FnOnce(&mut App, T) + Send>)) -> T {
    let slot = Arc::new(Mutex::new(None));
    let filled = slot.clone();
    ask(app, Box::new(move |_, value| *filled.lock().unwrap() = Some(value)));
    pump_until(app, what, |_| slot.lock().unwrap().is_some());
    let value = slot.lock().unwrap().take();
    value.unwrap()
}

/// Everything on disk and at the host: the durability barrier upstream's `flush_document_saves` is for.
fn save(app: &mut App) {
    wait(app, "the flush", |app, done| app.flush_latest_document(true, done)).expect("the document saves");
    // A reply from the host installs its authoritative body, which schedules an ordinary save as any change does.
    pump_until(app, "the trailing save", |app| app.save_status() == DocumentSaveStatus::Saved);
    wait(app, "the disk", |app, done| app.flush_document_saves(done)).expect("the host's writes finish");
}

fn host_body(app: &mut App, conversation_id: &str) -> Option<Conversation> {
    let id = conversation_id.to_owned();
    wait(app, "load_conversation", move |app, done| {
        app.invoke::<Option<Conversation>>("load_conversation", json!({ "conversationId": id }), move |app, result| done(app, result.expect("load_conversation")));
    })
}

fn user(id: &str, content: &str) -> ContextItem {
    serde_json::from_value(json!({ "id": id, "kind": "user", "content": content, "createdAt": "2026-11-04T00:00:00.000Z" })).unwrap()
}

/// The ids of a timeline, less the marker the host adds at startup after a user message nobody answered, which is what
/// every conversation here ends on.
fn ids(contexts: &[ContextItem]) -> Vec<&str> {
    contexts.iter().map(ContextItem::id).filter(|id| !id.starts_with("ctx_orphan_")).collect()
}

fn order(app: &App, workspace_id: &str) -> Vec<String> {
    app.workspace(workspace_id).unwrap().conversations.iter().map(|conversation| conversation.id.clone()).collect()
}

fn run_phase(root: &Path, name: &str) {
    let output = Command::new(std::env::current_exe().unwrap()).args(["--exact", "phase", "--nocapture"]).env(PHASE, name).env(ROOT, root).env("HOME", root.join("home")).env("MEWRK_LANG", "en").output().unwrap();
    assert!(output.status.success(), "phase {name} failed:\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[test]
fn document_survives_restarts() {
    let root = std::env::temp_dir().join(format!("mewrk-doc-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for dir in ["data", "home", "held/first", "held/second", "unheld"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    run_phase(&root, "fresh");
    seed_project(&root);
    run_phase(&root, "create");
    run_phase(&root, "reopen");
    run_phase(&root, "after_delete");
    let _ = std::fs::remove_dir_all(&root);
}

/// Writes a project into the host's document while no process holds it.
fn seed_project(root: &Path) {
    let path = root.join("data/document.v1.json");
    let mut document: Value = serde_json::from_slice(&std::fs::read(&path).expect("the first run wrote the document")).unwrap();
    let project = json!({
        "id": "ws_seed",
        "name": "Seed",
        "kind": "directory",
        "path": root.join("held/first"),
        "additionalWorkspaces": [{ "path": root.join("held/second") }],
        "createdAt": "2026-11-04T00:00:00.000Z",
        "defaultConversationPresetId": "",
        "lastConversationSettings": null,
        "conversations": []
    });
    document["workspaces"].as_array_mut().expect("a list of workspaces").insert(0, project);
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
}

/// One restart's worth of the test, in its own process. Does nothing when not asked for.
#[test]
fn phase() {
    let Ok(name) = std::env::var(PHASE) else { return };
    let root = PathBuf::from(std::env::var(ROOT).unwrap());
    let mut app = start(&root);
    doc::init(&mut app);
    match name.as_str() {
        "fresh" => fresh(&mut app),
        "create" => create(&mut app, &root),
        "reopen" => reopen(&mut app),
        "after_delete" => after_delete(&mut app),
        other => panic!("no phase {other}"),
    }
}

/// A fresh install: the temporary project's draft is on screen.
fn fresh(app: &mut App) {
    let temporary = mewrk_client::workspaces::TEMPORARY_WORKSPACE_ID;
    assert!(app.workspace(temporary).is_some(), "the host seeds the temporary project");
    assert_eq!(app.active_workspace_id().as_deref(), Some(temporary));
    assert_eq!(app.active_conversation_id(), Some(draft_conversation_id(temporary)));
    assert!(app.draft_is_active());
    pump_until(app, "the first save", |app| app.save_status() == DocumentSaveStatus::Saved);
    save(app);
}

fn create(app: &mut App, root: &Path) {
    assert!(app.workspace("ws_seed").is_some(), "the seeded project loads");

    // A directory no picker returned and no project holds: the host refuses the save, and says why.
    let refused = AttachedWorkspace { machine: None, path: root.join("unheld").to_string_lossy().into_owned() };
    app.add_workspace("Refused", &[refused], None);
    let refused_id = app.doc.workspaces[0].id.clone();
    assert_eq!(app.doc.workspaces[0].name, "Refused");
    // The flush the project's barrier sends fails first, and its answer goes to the barrier; the retry it schedules is
    // the background save, whose failure is what the reason comes from. A capability rescan can push that retry back.
    pump_until(app, "the refusal's reason", |app| app.save_failure_reason().is_some());
    assert_eq!(app.save_status(), DocumentSaveStatus::Error);
    let reason = app.save_failure_reason().expect("the host's reason is kept");
    assert!(reason.contains(&refused_id), "{reason}");
    // Taking it out again recovers.
    app.forget_draft(&refused_id);
    app.update_document(|doc| doc.workspaces.retain(|workspace| workspace.id != refused_id));
    pump_until(app, "the recovery", |app| app.save_status() == DocumentSaveStatus::Saved);
    assert_eq!(app.save_failure_reason(), None);

    // Create a project on a directory the seeded project holds. A new project opens the list, and its draft opens.
    let held = AttachedWorkspace { machine: None, path: root.join("held/second").to_string_lossy().into_owned() };
    app.add_workspace("Demo", std::slice::from_ref(&held), None);
    let project = app.doc.workspaces[0].id.clone();
    assert_eq!(app.doc.workspaces[0].name, "Demo");
    assert_eq!(app.doc.workspaces.last().unwrap().id, mewrk_client::workspaces::TEMPORARY_WORKSPACE_ID);
    pump_until(app, "the project's draft", |app| app.active_workspace_id().as_deref() == Some(project.as_str()));
    assert_eq!(app.active_conversation_id(), Some(draft_conversation_id(&project)));
    pump_until(app, "the project's save", |app| app.save_status() == DocumentSaveStatus::Saved);

    // Registering the same directory again reuses the project.
    app.add_workspace("Again", &[held], None);
    assert_eq!(app.doc.workspaces.iter().filter(|workspace| workspace.path == app.doc.workspaces[0].path).count(), 1);

    // A draft becomes a conversation under the id it was minted with, and the host then has it.
    let minted = app.draft_of_workspace(&project).unwrap().materializes_as;
    app.update_active_conversation(|conversation| conversation.contexts = vec![user("ctx_hand", "written by hand")]);
    let redeemed = wait(app, "the redemption", |app, done| app.redeem_draft(true, done)).unwrap().unwrap();
    assert_eq!(redeemed.conversation_id, minted);
    assert!(app.draft_of_workspace(&project).is_none());
    assert_eq!(ids(&host_body(app, &minted).expect("the host has it").contexts), vec!["ctx_hand"]);

    // Create conversations; each opens at the top of the list.
    let second = app.create_conversation(NewConversation { workspace_id: Some(project.clone()), ..NewConversation::default() }).unwrap();
    assert_eq!(app.active_conversation_id().as_deref(), Some(second.as_str()));
    assert_eq!(order(app, &project), vec![second.clone(), minted.clone()]);

    // The first turn's content, then a rename.
    app.update_conversation(&project, &second, |conversation| {
        conversation.contexts = vec![user(FIRST, "first"), user("ctx_2", "second")];
        conversation.updated_at = doc::now_iso();
    });
    app.rename_conversation(&project, &second, "Renamed by hand");
    assert_eq!(app.find_conversation(&project, &second).unwrap().title, "Renamed by hand");
    save(app);
    let stored = host_body(app, &second).unwrap();
    assert_eq!(stored.title, "Renamed by hand");
    assert_eq!(ids(&stored.contexts), vec![FIRST, "ctx_2"]);

    // A fork copies the history through the host, under the host's new ids, and follows its origin's title.
    app.select_conversation(&project, &second);
    app.fork_conversation_at(1);
    let forked = app.active_conversation_id().unwrap();
    assert_ne!(forked, second);
    assert_eq!(app.find_conversation(&project, &forked).unwrap().title, "Renamed by hand-fork-1");
    assert_eq!(app.find_conversation(&project, &forked).unwrap().parent_conversation_id.as_deref(), Some(second.as_str()));
    pump_until(app, "the forked history", |app| app.find_conversation(&project, &forked).is_some_and(|conversation| conversation.contexts.len() == 1));
    let forked_contexts = app.find_conversation(&project, &forked).unwrap().contexts.clone();
    assert!(forked_contexts[0].id().starts_with("ctx_") && forked_contexts[0].id() != FIRST, "the host renews ids");
    app.rename_conversation(&project, &second, "Renamed again");
    pump_until(app, "the fork's title", |app| app.find_conversation(&project, &forked).unwrap().title == "Renamed again-fork-1");
    save(app);

    // A branch from a user message: history before it copied, the message itself waiting in the composer.
    app.select_conversation(&project, &second);
    let at = app.find_conversation(&project, &second).unwrap().contexts[1].clone();
    app.branch_from_user_context(&at);
    let branched = app.active_conversation_id().unwrap();
    assert_ne!(branched, second);
    assert_eq!(app.find_conversation(&project, &branched).unwrap().parent_conversation_id.as_deref(), Some(second.as_str()));
    assert_eq!(app.composer_draft(&branched), "second");
    pump_until(app, "the branch's history", |app| app.find_conversation(&project, &branched).is_some_and(|conversation| conversation.contexts.len() == 1));
    save(app);
    assert_eq!(host_body(app, &branched).unwrap().contexts.len(), 1);
    // Deleting it within the session, which also takes the draft of the project back on screen.
    app.delete_conversation(&project, &branched);
    pump_until(app, "the branch's deletion", |app| app.find_conversation(&project, &branched).is_none() && !app.conversation_is_deleting(&branched));
    assert_eq!(app.active_conversation_id(), Some(draft_conversation_id(&project)));
    save(app);

    // Reorder: the origin goes above its fork, the hand-written one goes last.
    app.reorder_sidebar_conversation(&project, &second, &forked, false);
    assert_eq!(order(app, &project), vec![second.clone(), forked.clone(), minted.clone()]);
    save(app);

    // What upstream kept in localStorage.
    app.ui_store_update(|store| store.set("mewrk.sidebar-width", "312"));
    app.flush_ui_store();

    // Written for the next process: which is which.
    std::fs::write(std::path::Path::new(&std::env::var(ROOT).unwrap()).join("ids.json"), serde_json::to_vec(&json!({ "project": project, "minted": minted, "second": second, "forked": forked })).unwrap()).unwrap();
}

fn saved_ids() -> Value {
    serde_json::from_slice(&std::fs::read(std::path::Path::new(&std::env::var(ROOT).unwrap()).join("ids.json")).unwrap()).unwrap()
}

fn reopen(app: &mut App) {
    let saved = saved_ids();
    let (project, minted, second, forked) = (saved["project"].as_str().unwrap(), saved["minted"].as_str().unwrap(), saved["second"].as_str().unwrap(), saved["forked"].as_str().unwrap());

    // Order and metadata came back; bodies did not, and they aren't mistaken for empty ones.
    assert_eq!(order(app, project), vec![second.to_owned(), forked.to_owned(), minted.to_owned()]);
    assert_eq!(app.find_conversation(project, second).unwrap().title, "Renamed again");
    assert_eq!(app.find_conversation(project, forked).unwrap().title, "Renamed again-fork-1");
    assert!(app.is_body_unloaded(minted));
    assert!(app.find_conversation(project, minted).unwrap().contexts.is_empty());
    let workspace = app.workspace(project).unwrap();
    assert_eq!(visible_conversations(&workspace.conversations, &app.unloaded).len(), 3, "an unloaded body is not an empty one");
    assert_eq!(app.ui_store().get("mewrk.sidebar-width"), Some("312"));

    // A metadata write on an unloaded conversation keeps the host's body.
    app.rename_conversation(project, minted, "Metadata only");
    save(app);
    let stored = host_body(app, minted).unwrap();
    assert_eq!(stored.title, "Metadata only");
    assert_eq!(ids(&stored.contexts), vec!["ctx_hand"]);
    assert!(app.is_body_unloaded(minted));

    // Load a body on demand.
    assert_eq!(wait(app, "the body", |app, done| app.ensure_conversation_loaded(minted, done)), Ok(true));
    assert!(!app.is_body_unloaded(minted));
    assert_eq!(ids(&app.find_conversation(project, minted).unwrap().contexts), vec!["ctx_hand"]);
    assert_eq!(wait(app, "an unknown body", |app, done| app.ensure_conversation_loaded("conv_nowhere", done)), Ok(false));

    // Opening one loads it by itself.
    app.select_conversation(project, second);
    pump_until(app, "the opened body", |app| !app.is_body_unloaded(second));
    assert_eq!(ids(&app.find_conversation(project, second).unwrap().contexts), vec![FIRST, "ctx_2"]);

    // Delete the origin: its fork moves up to the top level, as the host's store does on delete, and a draft opens.
    app.delete_conversation(project, second);
    assert!(app.conversation_is_deleting(second));
    pump_until(app, "the deletion", |app| app.find_conversation(project, second).is_none() && !app.conversation_is_deleting(second));
    assert_eq!(app.find_conversation(project, forked).unwrap().parent_conversation_id, None);
    assert_eq!(app.active_conversation_id(), Some(draft_conversation_id(project)));
    assert_eq!(order(app, project), vec![forked.to_owned(), minted.to_owned()]);
    save(app);
    assert!(host_body(app, second).is_none());
    assert_eq!(host_body(app, forked).unwrap().parent_conversation_id, None);

    // Delete one project; the others stay, and what's open moves off it.
    app.delete_workspace(project);
    pump_until(app, "the project's deletion", |app| app.workspace(project).is_none());
    assert!(app.draft_of_workspace(project).is_none());
    assert_ne!(app.active_workspace_id().as_deref(), Some(project));
    assert!(app.workspace("ws_seed").is_some());
    settle(app, Duration::from_millis(50));
    save(app);
}

fn after_delete(app: &mut App) {
    let saved = saved_ids();
    let (project, minted, second) = (saved["project"].as_str().unwrap(), saved["minted"].as_str().unwrap(), saved["second"].as_str().unwrap());
    assert!(app.workspace(project).is_none(), "the project stays deleted");
    assert!(app.conversation(second).is_none());
    assert!(host_body(app, second).is_none(), "the host forgot the deleted conversation");
    // Upstream's `deleteWorkspace` sends no delete commands, only the save without the project, so the store still has
    // the project's conversations and the host adopts them into the temporary project at the next start: they come back,
    // though the dialog says "permanently". Same here until upstream's behaviour is decided otherwise.
    let temporary = app.workspace(mewrk_client::workspaces::TEMPORARY_WORKSPACE_ID).unwrap();
    assert!(temporary.conversations.iter().any(|conversation| conversation.id == minted));
    assert!(app.workspace("ws_seed").is_some());
    assert_eq!(app.ui_store().get("mewrk.sidebar-width"), Some("312"));
    save(app);
}
