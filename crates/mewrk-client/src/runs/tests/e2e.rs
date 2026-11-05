//! The real host: its conversation store and write gates under the send path, and, once its model layer can run here
//! without a download, the whole path over a real socket to a mock provider speaking OpenAI's chat completions.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::rig::RunCall;
use crate::model::{ContextItem, Conversation};
use crate::runs::composer::Deferred;
use crate::runs::conversation_turns::ConversationTurnStatus;
use crate::runs::state::{EventSink, RunBackend};
use crate::{App, StartOptions};

/// One SSE reply per request, as `crates/mewrk-core/tests/engine.rs`'s server does it. `hold` keeps each reply back until
/// it reads true, for a test that wants a run to stay live.
fn serve(text: &'static [&'static str], hold: Arc<AtomicBool>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { return };
            let count = count.clone();
            let hold = hold.clone();
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let header_end = loop {
                        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                            break at + 4;
                        }
                        match socket.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..header_end]).to_lowercase();
                    let length: usize = head.lines().find_map(|line| line.strip_prefix("content-length:").and_then(|value| value.trim().parse().ok())).unwrap_or(0);
                    while buffer.len() < header_end + length {
                        match socket.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                    }
                    buffer.drain(..header_end + length);
                    count.fetch_add(1, Ordering::SeqCst);
                    let mut frames: Vec<String> = text.iter().map(|delta| json!({ "choices": [{ "delta": { "content": delta } }] }).to_string()).collect();
                    frames.push(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_owned());
                    frames.push(r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5}}"#.to_owned());
                    frames.push("[DONE]".to_owned());
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n");
                    let _ = socket.flush();
                    while hold.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    for frame in frames {
                        let frame = format!("data: {frame}\n\n");
                        let _ = socket.write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes());
                        let _ = socket.flush();
                    }
                    let _ = socket.write_all(b"0\r\n\r\n");
                }
            });
        }
    });
    (address, requests)
}

/// Lets queued work run for a while, for asserting that something did not happen.
fn idle(app: &mut App, milliseconds: u64) {
    let deadline = Instant::now() + Duration::from_millis(milliseconds);
    while Instant::now() < deadline {
        app.tick();
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn pump(app: &mut App, what: &str, timeout: Duration, mut done: impl FnMut(&mut App) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        app.tick();
        if done(app) {
            return true;
        }
        if Instant::now() > deadline {
            eprintln!("timed out waiting for {what}");
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct RealHost {
    app: App,
    task: crate::Task,
    workspace_id: String,
    requests: Arc<AtomicUsize>,
    hold: Arc<AtomicBool>,
    dir: std::path::PathBuf,
}

impl Drop for RealHost {
    fn drop(&mut self) {
        // A sidecar a run with `MEWRK_RUNS_E2E` set downloaded lives in here, and the next test of this process wants it.
        if std::env::var_os(RUN_ANYWAY).is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

const ID: &str = "e2e-conversation";

/// Runs the model half whatever the state of the host, downloading its sidecar if it has to.
const RUN_ANYWAY: &str = "MEWRK_RUNS_E2E";

/// Whether a run can reach the mock without installing anything. The host's model layer is still the Node sidecar, which
/// a development build fetches from Mewrk's channel (100 MB) on first use unless it finds one built in the source tree.
/// `crates/mewrk-host/aisdk-rs` is replacing it in-process; this turns on by itself when the host depends on that.
fn model_layer_ready() -> bool {
    let host_manifest = include_str!("../../../../mewrk-host/host/Cargo.toml");
    let sidecar = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../mewrk-host/aisdk-service/dist/main.mjs");
    std::env::var_os(RUN_ANYWAY).is_some() || host_manifest.contains("aisdk-rs") || sidecar.exists()
}

/// `true` when the test may go on.
fn model_half(test: &str) -> bool {
    let ready = model_layer_ready();
    if !ready {
        eprintln!(
            "SKIPPED {test}: the host's model layer is not ready here. It needs `crates/mewrk-host/aisdk-rs` in the host, or a built sidecar, or {RUN_ANYWAY}=1 to let the host download one."
        );
    }
    ready
}

/// An app on a real host, with a provider on the mock's socket and one empty conversation on screen.
fn real_host(text: &'static [&'static str], backend: impl FnOnce(crate::Task) -> Option<Arc<dyn RunBackend>>) -> RealHost {
    let dir = std::env::temp_dir().join(format!("mewrk-runs-e2e-{}", crate::runs::util::create_id("e2e")));
    std::fs::create_dir_all(&dir).unwrap();
    let hold = Arc::new(AtomicBool::new(false));
    let (address, requests) = serve(text, hold.clone());
    let mut app = App::start(StartOptions {
        identifier: "mewrk.runs.e2e".into(),
        version: "0.0.0".into(),
        data_dir: dir.clone(),
        resource_dir: None,
        wake: Arc::new(|| {}),
        exit: Arc::new(|_| {}),
        show_window: Arc::new(|| {}),
    })
    .map_err(|_| "App::start failed")
    .unwrap();
    // A loopback address needs no key, which keeps this test out of the machine's real credential store. The id is new
    // each time so that even reading its absent key finds nothing there to ask about.
    let provider_id = crate::runs::util::create_id("mock");
    let provider = serde_json::from_value(json!({
        "id": provider_id, "name": "Mock", "enabled": true, "family": "openai_chat", "baseUrl": address,
        "models": [{ "id": "mock-model", "name": "", "group": "", "capabilities": [], "reasoningContent": "plaintext" }], "activeModelId": "mock-model"
    }))
    .unwrap();
    app.doc.assets.api_providers.push(provider);
    app.doc.global_settings.active_provider_id = Some(provider_id);
    let task = app.task();
    let lists: Vec<Vec<Conversation>> = app.doc.workspaces.iter_mut().map(|workspace| std::mem::take(&mut workspace.conversations)).collect();
    let document = serde_json::to_value(&app.doc).unwrap();
    for (workspace, conversations) in app.doc.workspaces.iter_mut().zip(lists) {
        workspace.conversations = conversations;
    }
    task.invoke::<Value>("save_document", json!({ "document": document, "durable": true })).expect("the host takes the provider");
    let workspace_id = app.doc.workspaces[0].id.clone();
    let conversation: Conversation = serde_json::from_value(json!({ "id": ID, "title": "New task", "createdAt": "2026-07-24T00:00:00Z", "updatedAt": "2026-07-24T00:00:00Z", "settings": { "enabledTools": [] }, "contexts": [] })).unwrap();
    let created: Conversation = task.invoke("create_conversation", json!({ "workspaceId": workspace_id, "conversation": conversation })).expect("the host files the conversation");
    app.doc.workspaces[0].conversations.insert(0, created);
    if let Some(backend) = backend(task.clone()) {
        app.set_run_backend(backend);
    }
    app.set_active(Some(&workspace_id), Some(ID));
    app.init_runs();
    assert!(pump(&mut app, "adoption", Duration::from_secs(10), |app| app.runs().resumable_runs_adopted));
    RealHost { app, task, workspace_id, requests, hold, dir }
}

impl RealHost {
    fn stored(&self) -> Conversation {
        self.task.invoke::<Option<Conversation>>("load_conversation", json!({ "conversationId": ID })).unwrap().expect("the host has it")
    }
}

fn user_contents(contexts: &[ContextItem]) -> Vec<String> {
    contexts.iter().filter_map(|context| if let ContextItem::User { content, .. } = context { Some(content.clone()) } else { None }).collect()
}

fn replies(contexts: &[ContextItem]) -> Vec<String> {
    contexts.iter().filter_map(|context| if let ContextItem::Assistant { content, .. } = context { Some(content.clone()) } else { None }).collect()
}

/// The real host for everything but the model: its store and write gates are what these tests are about.
struct ModelStandIn {
    task: crate::Task,
    runs: Mutex<Vec<Arc<RunCall>>>,
}

impl RunBackend for ModelStandIn {
    fn invoke(&self, command: &str, args: Value, events: Option<EventSink>) -> Result<Value, String> {
        match command {
            "run_model" => {
                let call = Arc::new(RunCall { args, sink: events.expect("run_model streams"), reply: Deferred::new() });
                self.runs.lock().unwrap().push(call.clone());
                call.reply.wait()
            }
            "cancel_conversation_run" | "cancel_model_run" => Ok(json!(true)),
            other => self.task.invoke::<Value>(other, args),
        }
    }
}

fn real_store() -> (RealHost, Arc<ModelStandIn>) {
    let mut stand_in = None;
    let host = real_host(&[], |task| {
        let backend = Arc::new(ModelStandIn { task, runs: Mutex::default() });
        stand_in = Some(backend.clone());
        Some(backend)
    });
    (host, stand_in.expect("made above"))
}

fn run_of(host: &mut RealHost, stand_in: &ModelStandIn, index: usize) -> Arc<RunCall> {
    assert!(pump(&mut host.app, "the run", Duration::from_secs(20), |_| stand_in.runs.lock().unwrap().len() > index), "error: {:?}", host.app.model_run_error(ID).map(|error| error.message));
    stand_in.runs.lock().unwrap()[index].clone()
}

fn empty_response() -> Value {
    json!({ "contexts": [], "usage": {}, "model": "mock-model", "providerName": "Mock", "durationMs": 1 })
}

/// What the write gates of the real store do to a second turn: the read model has to match what the host wrote, or its
/// `expected_context_ids` silently keep the host's prose and the message never reaches the store.
#[test]
fn runs_real_store_takes_a_sent_turn_a_queued_message_and_its_promotion() {
    let (mut host, stand_in) = real_store();
    host.app.set_composer_draft(ID, "First");
    host.app.send_message();
    let first = run_of(&mut host, &stand_in, 0);
    assert_eq!(first.last_context()["content"], json!("First"));
    let stored = host.stored();
    assert_eq!(user_contents(&stored.contexts), ["First"], "durable before the request goes out");
    assert_eq!(stored.title, "First");
    assert!(stored.settings.tool_lock.is_some(), "the tool surface is durable with the turn");

    host.app.set_composer_draft(ID, "Second");
    host.app.send_message();
    assert_eq!(host.app.composer_draft(ID), "");
    let task = host.task.clone();
    assert!(pump(&mut host.app, "the queued message at the host", Duration::from_secs(10), move |_| {
        task.invoke::<Option<Conversation>>("load_conversation", json!({ "conversationId": ID })).ok().flatten().is_some_and(|stored| !stored.queued_messages.is_empty())
    }));
    assert_eq!(host.stored().queued_messages[0].content, "Second");

    first.finish(empty_response());
    let second = run_of(&mut host, &stand_in, 1);
    assert_eq!(second.last_context()["content"], json!("Second"));
    assert_eq!(user_contents(&host.stored().contexts), ["First", "Second"]);
    assert!(host.stored().queued_messages.is_empty(), "promoted, not left behind");
    second.finish(empty_response());
    assert!(pump(&mut host.app, "the run to end", Duration::from_secs(10), |app| !app.model_run_is_active(ID)));
    assert_eq!(host.app.model_run_error(ID).map(|error| error.message), None);

    // A later turn builds on what the host holds, after a refresh replaced the read model with it.
    idle(&mut host.app, 300);
    host.app.set_composer_draft(ID, "Third");
    host.app.send_message();
    let third = run_of(&mut host, &stand_in, 2);
    assert_eq!(user_contents(&host.stored().contexts), ["First", "Second", "Third"]);
    third.finish(empty_response());
    assert!(pump(&mut host.app, "the run to end", Duration::from_secs(10), |app| !app.model_run_is_active(ID)));
}

/// A Stop pauses the queue where the settlement's refresh reads it back: at the host.
#[test]
fn runs_real_store_holds_a_stopped_queue_across_the_settlements_refresh() {
    let (mut host, stand_in) = real_store();
    host.app.set_composer_draft(ID, "First");
    host.app.send_message();
    let first = run_of(&mut host, &stand_in, 0);
    host.app.set_composer_draft(ID, "Held back");
    host.app.send_message();
    host.app.cancel_run(ID, |_, _| {});
    assert!(host.app.conversation(ID).unwrap().queue_paused);
    idle(&mut host.app, 50);
    first.finish(json!({ "contexts": [], "usage": {}, "model": "mock-model", "providerName": "Mock", "durationMs": 1, "stopReason": "cancelled" }));
    assert!(pump(&mut host.app, "the stop to settle", Duration::from_secs(10), |app| !app.model_run_is_active(ID) && !app.model_is_stopping(ID)));
    // Nothing follows on its own, even once the settlement's refresh has come back from the host.
    idle(&mut host.app, 600);
    assert_eq!(stand_in.runs.lock().unwrap().len(), 1);
    assert_eq!(host.app.conversation(ID).unwrap().queued_messages.len(), 1);
    assert!(host.app.conversation(ID).unwrap().queue_paused, "the host's copy holds the pause");
    assert!(host.stored().queue_paused);
    // The round produced nothing, so it leaves no record; the message that started it stays.
    assert!(host.app.conversation_turns(ID).is_empty());

    // Sending again resumes the queue: what waited goes first, then the new message, a round each.
    host.app.set_composer_draft(ID, "After the stop");
    host.app.send_message();
    let second = run_of(&mut host, &stand_in, 1);
    assert_eq!(second.last_context()["content"], json!("Held back"));
    second.finish(empty_response());
    let third = run_of(&mut host, &stand_in, 2);
    assert_eq!(third.last_context()["content"], json!("After the stop"));
    assert_eq!(user_contents(&host.stored().contexts), ["First", "Held back", "After the stop"]);
    assert!(!host.stored().queue_paused);
    third.finish(empty_response());
    assert!(pump(&mut host.app, "the run to end", Duration::from_secs(10), |app| !app.model_run_is_active(ID)));
}

/// A refused write is refused by the host itself here: a workspace it never heard of.
#[test]
fn runs_real_store_says_why_when_the_host_refuses_the_turn() {
    let (mut host, stand_in) = real_store();
    // Somebody deleted it at the host while the read model kept it.
    host.task.invoke::<Value>("delete_conversation", json!({ "workspaceId": host.workspace_id, "conversationId": ID })).expect("the host deletes it");
    host.app.set_composer_draft(ID, "Into the void");
    host.app.send_message();
    assert!(pump(&mut host.app, "the refusal", Duration::from_secs(10), |app| app.model_run_error(ID).is_some()));
    let error = host.app.model_run_error(ID).unwrap();
    assert!(!error.retryable);
    assert!(stand_in.runs.lock().unwrap().is_empty(), "a history without the message never goes out");
    assert_eq!(host.app.composer_draft(ID), "Into the void", "the message stays with the user");
}

/// The whole path, model included: a reply over a real socket.
#[test]
fn runs_end_to_end_a_message_reaches_the_mock_provider_and_its_reply_comes_back() {
    if !model_half("runs_end_to_end_a_message_reaches_the_mock_provider_and_its_reply_comes_back") {
        return;
    }
    let mut host = real_host(&["Hello", ", ", "world"], |_| None);
    host.app.set_composer_draft(ID, "Say hello");
    host.app.send_message();
    assert!(pump(&mut host.app, "the run to end", Duration::from_secs(120), |app| app.composer_draft(ID).is_empty() && !app.model_run_is_active(ID)), "error: {:?}", host.app.model_run_error(ID).map(|error| error.message));
    assert_eq!(host.app.model_run_error(ID).map(|error| error.message), None);
    assert!(host.requests.load(Ordering::SeqCst) > 0);
    assert_eq!(host.app.doc.workspaces[0].conversations[0].title, "Say hello");
    assert_eq!(replies(&host.app.doc.workspaces[0].conversations[0].contexts), ["Hello, world"]);
    let stored = host.stored();
    assert_eq!(user_contents(&stored.contexts), ["Say hello"]);
    assert_eq!(replies(&stored.contexts), ["Hello, world"]);
    let turns = host.app.conversation_turns(ID);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].status, ConversationTurnStatus::Completed);
    assert_eq!(turns[0].usage.input_tokens, Some(100));
    assert!(!host.app.context_usage_view(ID).estimated);
}

#[test]
fn runs_end_to_end_a_queued_message_follows_the_live_run_as_its_own_turn() {
    if !model_half("runs_end_to_end_a_queued_message_follows_the_live_run_as_its_own_turn") {
        return;
    }
    let mut host = real_host(&["Reply"], |_| None);
    host.hold.store(true, Ordering::SeqCst);
    host.app.set_composer_draft(ID, "First");
    host.app.send_message();
    let requests = host.requests.clone();
    assert!(pump(&mut host.app, "the provider", Duration::from_secs(120), |_| requests.load(Ordering::SeqCst) > 0));
    host.app.set_composer_draft(ID, "Second");
    host.app.send_message();
    assert_eq!(host.app.conversation(ID).unwrap().queued_messages.len(), 1);
    host.hold.store(false, Ordering::SeqCst);
    assert!(pump(&mut host.app, "both turns", Duration::from_secs(60), |app| {
        !app.model_run_is_active(ID) && app.conversation(ID).is_some_and(|conversation| conversation.queued_messages.is_empty() && replies(&conversation.contexts).len() == 2)
    }));
    assert_eq!(host.app.model_run_error(ID).map(|error| error.message), None);
    assert_eq!(host.requests.load(Ordering::SeqCst), 2);
    let stored = host.stored();
    assert_eq!(user_contents(&stored.contexts), ["First", "Second"]);
    assert_eq!(replies(&stored.contexts), ["Reply", "Reply"]);
    assert_eq!(host.app.conversation_turns(ID).len(), 2);
}

/// A Stop goes all the way through the host's own cancellation.
#[test]
fn runs_end_to_end_a_stop_settles_through_the_host() {
    if !model_half("runs_end_to_end_a_stop_settles_through_the_host") {
        return;
    }
    let mut host = real_host(&["Reply"], |_| None);
    host.hold.store(true, Ordering::SeqCst);
    host.app.set_composer_draft(ID, "First");
    host.app.send_message();
    let requests = host.requests.clone();
    assert!(pump(&mut host.app, "the provider", Duration::from_secs(120), |_| requests.load(Ordering::SeqCst) > 0));
    let stopped = Arc::new(Mutex::new(None));
    let heard = stopped.clone();
    host.app.cancel_run(ID, move |_, settled| *heard.lock().unwrap() = Some(settled));
    assert!(pump(&mut host.app, "the stop", Duration::from_secs(20), |_| stopped.lock().unwrap().is_some()));
    assert_eq!(*stopped.lock().unwrap(), Some(true), "the host took the stop and the run settled");
    host.hold.store(false, Ordering::SeqCst);
    assert!(!host.app.model_run_is_active(ID));
    assert_eq!(user_contents(&host.stored().contexts), ["First"]);
}
