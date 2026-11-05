//! A headless [`App`] with a stand-in host: upstream's `runtimeMocks`, for the logic its `App.*.test.tsx` exercise.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::model::{ContextItem, Conversation};
use crate::runs::composer::Deferred;
use crate::runs::state::{EventSink, RunBackend, RunsState};
use crate::runs::types::ModelRunRequest;
use crate::{App, StartOptions};

/// One `run_model` the host is holding.
pub struct RunCall {
    pub args: Value,
    pub sink: EventSink,
    pub reply: Deferred<Result<Value, String>>,
}

impl RunCall {
    pub fn emit(&self, event: Value) {
        (self.sink)(event);
    }

    pub fn finish(&self, response: Value) {
        self.reply.resolve(Ok(response));
    }

    pub fn fail(&self, message: &str) {
        self.reply.resolve(Err(message.to_owned()));
    }

    /// The contexts of this request, by id.
    pub fn context_ids(&self) -> Vec<String> {
        self.args["request"]["contexts"].as_array().unwrap().iter().map(|context| context["id"].as_str().unwrap().to_owned()).collect()
    }

    pub fn last_context(&self) -> Value {
        self.args["request"]["contexts"].as_array().unwrap().last().cloned().unwrap_or(Value::Null)
    }
}

/// What a command answers when a test has not said otherwise.
#[derive(Default)]
pub struct FakeHost {
    pub calls: Mutex<Vec<(String, Value)>>,
    pub runs: Mutex<Vec<Arc<RunCall>>>,
    /// Answers `run_model` straight away, in order, instead of holding it.
    pub auto_replies: Mutex<VecDeque<Result<Value, String>>>,
    /// Durable saves still to refuse.
    pub fail_durable_saves: AtomicUsize,
    /// Every save refused while set.
    pub save_gate: Mutex<Option<Deferred<Result<(), String>>>>,
    pub resumable_failures: AtomicUsize,
    /// Per-command answers, first in first out.
    pub replies: Mutex<HashMap<String, VecDeque<Result<Value, String>>>>,
    pub attach: Mutex<Option<Box<dyn FnOnce(EventSink) -> Value + Send>>>,
    pub attach_sink: Mutex<Option<EventSink>>,
    /// What `load_conversation` knows, by id.
    pub store: Mutex<HashMap<String, Conversation>>,
    /// What `update_conversation` refuses, by conversation id.
    pub refuse_updates: Mutex<HashMap<String, String>>,
    /// Answered `update_conversation` without the conversation's contexts changing, as a stale read model is.
    pub keep_prose: Mutex<bool>,
}

impl FakeHost {
    pub fn count(&self, command: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|(name, _)| name == command).count()
    }

    pub fn calls_of(&self, command: &str) -> Vec<Value> {
        self.calls.lock().unwrap().iter().filter(|(name, _)| name == command).map(|(_, args)| args.clone()).collect()
    }

    pub fn reply(&self, command: &str, reply: Result<Value, String>) {
        self.replies.lock().unwrap().entry(command.to_owned()).or_default().push_back(reply);
    }

    pub fn auto_reply(&self, response: Value) {
        self.auto_replies.lock().unwrap().push_back(Ok(response));
    }

    pub fn run_count(&self) -> usize {
        self.runs.lock().unwrap().len()
    }

    pub fn run(&self, index: usize) -> Arc<RunCall> {
        self.runs.lock().unwrap()[index].clone()
    }

    pub fn last_run(&self) -> Arc<RunCall> {
        self.runs.lock().unwrap().last().unwrap().clone()
    }

    pub fn store_conversation(&self, conversation: Conversation) {
        self.store.lock().unwrap().insert(conversation.id.clone(), conversation);
    }
}

impl RunBackend for FakeHost {
    fn invoke(&self, command: &str, args: Value, events: Option<EventSink>) -> Result<Value, String> {
        self.calls.lock().unwrap().push((command.to_owned(), args.clone()));
        if let Some(reply) = self.replies.lock().unwrap().get_mut(command).and_then(VecDeque::pop_front) {
            return reply;
        }
        match command {
            "run_model" => {
                let call = Arc::new(RunCall { args, sink: events.expect("run_model streams"), reply: Deferred::new() });
                self.runs.lock().unwrap().push(call.clone());
                if let Some(reply) = self.auto_replies.lock().unwrap().pop_front() {
                    return reply;
                }
                call.reply.wait()
            }
            "attach_model_run" => match self.attach.lock().unwrap().take() {
                Some(script) => {
                    let sink = events.expect("attach streams");
                    *self.attach_sink.lock().unwrap() = Some(sink.clone());
                    Ok(script(sink))
                }
                None => Ok(json!({ "status": "none" })),
            },
            "save_document" => {
                let gate = self.save_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.wait()?;
                }
                if args["durable"] == json!(true) && self.fail_durable_saves.load(Ordering::SeqCst) > 0 {
                    self.fail_durable_saves.fetch_sub(1, Ordering::SeqCst);
                    return Err("disk unavailable".to_owned());
                }
                Ok(Value::Null)
            }
            "update_conversation" => {
                let conversation: Conversation = serde_json::from_value(args["conversation"].clone()).map_err(|error| error.to_string())?;
                if let Some(reason) = self.refuse_updates.lock().unwrap().get(&conversation.id) {
                    return Err(reason.clone());
                }
                let mut store = self.store.lock().unwrap();
                let previous = store.get(&conversation.id).cloned();
                let mut next = conversation;
                if *self.keep_prose.lock().unwrap() {
                    if let Some(previous) = previous {
                        next.contexts = previous.contexts;
                    }
                }
                store.insert(next.id.clone(), next.clone());
                Ok(serde_json::to_value(next).unwrap())
            }
            "load_conversation" => {
                let id = args["conversationId"].as_str().unwrap_or("");
                Ok(self.store.lock().unwrap().get(id).map_or(Value::Null, |conversation| serde_json::to_value(conversation).unwrap()))
            }
            "list_resumable_runs" => {
                if self.resumable_failures.load(Ordering::SeqCst) > 0 {
                    self.resumable_failures.fetch_sub(1, Ordering::SeqCst);
                    return Err("宿主暂不可用".to_owned());
                }
                Ok(json!([]))
            }
            "list_wake_pending_conversations" | "list_pending_tool_prompts" | "list_pending_fork_requests" | "list_pending_fork_starts" | "list_fork_decisions" => Ok(json!([])),
            "cancel_conversation_run" | "cancel_model_run" => Ok(json!(true)),
            "take_run_settlement" | "load_conversation_plan" | "flush_document_saves" => Ok(Value::Null),
            "resolve_tool_prompt" | "request_tool_approval" => Ok(json!({ "expiresInMs": 0 })),
            "get_tool_explanations" => Ok(json!({})),
            "steer_model_run" | "resolve_fork_request" => Ok(Value::Null),
            other => Err(format!("the fake host has no {other}")),
        }
    }
}

pub const WS: &str = "workspace-1";
pub const CONV: &str = "conversation-1";

/// `documentWithModel`: one workspace, one conversation, one provider with one model.
fn install_document(app: &mut App, capabilities: &[&str]) {
    let provider = serde_json::from_value(json!({
        "id": "provider-1", "name": "Test Provider", "enabled": true, "family": "openai_chat", "baseUrl": "http://localhost:9/v1",
        "models": [{ "id": "model-1", "name": "", "group": "", "capabilities": capabilities, "reasoningContent": "plaintext" }], "activeModelId": "model-1"
    }))
    .unwrap();
    app.doc.assets.api_providers = vec![provider];
    app.doc.global_settings.active_provider_id = Some("provider-1".to_owned());
    let mut workspace = app.doc.workspaces.first().cloned().unwrap_or_else(|| panic!("a fresh host seeds a workspace"));
    workspace.id = WS.to_owned();
    workspace.path = "/workspace".to_owned();
    workspace.conversations = vec![conversation(CONV)];
    app.doc.workspaces = vec![workspace];
}

/// A conversation with nothing in it.
pub fn conversation(id: &str) -> Conversation {
    serde_json::from_value(json!({ "id": id, "title": "New task", "createdAt": "2026-07-24T00:00:00Z", "updatedAt": "2026-07-24T00:00:00Z", "settings": { "enabledTools": [] }, "contexts": [] })).unwrap()
}

pub fn response(contexts: Value) -> Value {
    json!({ "contexts": contexts, "usage": {}, "model": "model-1", "providerName": "Test Provider", "durationMs": 5 })
}

pub fn stopped_response(reason: &str) -> Value {
    json!({ "contexts": [], "usage": {}, "model": "model-1", "providerName": "Test Provider", "durationMs": 500, "stopReason": reason })
}

pub fn reply_context(id: &str, content: &str) -> Value {
    json!({ "id": id, "kind": "assistant", "content": content, "createdAt": "2026-07-24T00:00:05Z" })
}

pub struct Rig {
    pub app: App,
    pub host: Arc<FakeHost>,
    dir: std::path::PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    /// A running app, ready as far as adoption.
    pub fn new() -> Self {
        Self::configured(&[], |_, _| {})
    }

    /// `configure` runs before the run code starts, like arranging mocks before `render`.
    pub fn configured(capabilities: &[&str], configure: impl FnOnce(&mut App, &FakeHost)) -> Self {
        let dir = std::env::temp_dir().join(format!("mewrk-runs-{}", crate::runs::util::create_id("rig")));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App::start(StartOptions {
            identifier: "mewrk.runs.test".into(),
            version: "0.0.0".into(),
            data_dir: dir.clone(),
            resource_dir: None,
            wake: Arc::new(|| {}),
            exit: Arc::new(|_| {}),
            show_window: Arc::new(|| {}),
        })
        .map_err(|_| "App::start failed")
        .unwrap();
        crate::i18n::set_english(true);
        let host = Arc::new(FakeHost::default());
        install_document(&mut app, capabilities);
        host.store_conversation(conversation(CONV));
        configure(&mut app, &host);
        app.set_run_backend(host.clone());
        let doc_host = host.clone();
        app.set_doc_backend(Arc::new(move |command, args| doc_host.invoke(command, args, None)));
        app.set_active(Some(WS), Some(CONV));
        let mut rig = Self { app, host, dir };
        rig.app.init_runs();
        rig
    }

    /// Lets everything queued for the UI thread run until `done` says so.
    pub fn until(&mut self, what: &str, mut done: impl FnMut(&mut Rig) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.app.tick();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Lets queued work run for a while, for asserting that something did not happen.
    pub fn idle(&mut self, milliseconds: u64) {
        let deadline = Instant::now() + Duration::from_millis(milliseconds);
        while Instant::now() < deadline {
            self.app.tick();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn adopted(&mut self) {
        self.until("adoption", |rig| rig.state().resumable_runs_adopted);
    }

    pub fn state(&mut self) -> &mut RunsState {
        self.app.local::<RunsState>()
    }

    pub fn runs(&self) -> usize {
        self.host.run_count()
    }

    /// Types into the composer and sends, as the Send button does.
    pub fn send(&mut self, text: &str) {
        self.app.set_composer_draft(CONV, text);
        self.app.send_message();
    }

    pub fn wait_for_run(&mut self, count: usize) -> Arc<RunCall> {
        self.until(&format!("run {count}"), |rig| rig.runs() >= count);
        self.host.run(count - 1)
    }

    pub fn run_exits(&mut self) {
        self.until("the run to leave", |rig| !rig.app.model_run_is_active(CONV));
    }

    pub fn conversation(&self) -> &Conversation {
        self.app.doc.workspaces[0].conversations.iter().find(|conversation| conversation.id == CONV).unwrap()
    }

    pub fn context_ids(&self) -> Vec<String> {
        self.conversation().contexts.iter().map(|context| context.id().to_owned()).collect()
    }

    pub fn set_contexts(&mut self, contexts: Vec<ContextItem>) {
        self.app.doc.workspaces[0].conversations[0].contexts = contexts.clone();
        let mut stored = self.host.store.lock().unwrap();
        stored.get_mut(CONV).unwrap().contexts = contexts;
    }

    pub fn turns(&self) -> Vec<crate::runs::conversation_turns::ConversationTurn> {
        // The app's accessor wants `&mut`; tests read the record as upstream's read localStorage.
        let app = &self.app as *const App as *mut App;
        // Tests only touch the app from this thread, and nothing else holds a borrow here.
        unsafe { (*app).conversation_turns(CONV) }
    }
}

pub fn request_of(call: &RunCall) -> ModelRunRequest {
    ModelRunRequest { run: serde_json::from_value(call.args["request"].clone()).unwrap(), fork_prompt_context_id: None, compact_now: false }
}
