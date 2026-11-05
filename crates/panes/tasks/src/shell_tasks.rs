//! `lib/shellTasks.ts`: the shape of a shell command as the host reports it, and the rows a conversation has.

use std::collections::HashSet;
use std::rc::Rc;

use mewrk_client::{App, Channel};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShellTaskOutcome {
    Succeeded,
    Failed,
    Stopped,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ShellTaskSnapshot {
    pub shell_task_id: String,
    pub conversation_id: String,
    pub tool_name: String,
    pub command: String,
    pub stopping: bool,
    pub started_at: String,
    pub ended_at: Option<String>,
    /// A row is running exactly while this is `None`.
    pub outcome: Option<ShellTaskOutcome>,
    pub exit_code: Option<i32>,
    pub background: bool,
    pub workspace_root: Option<String>,
    pub duration_ms: Option<u64>,
    pub cwd: Option<String>,
    pub call_id: Option<String>,
    pub explanation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[derive(Default)]
pub struct ShellTaskOutputHandle {
    pub snapshot: String,
    pub live: bool,
    pub dropped_head_bytes: u64,
    pub subscription_id: u64,
}

/// What the app keeps for every conversation, upstream's `shellTasks` state in `App.tsx`.
#[derive(Default)]
pub struct ShellTaskStore {
    pub tasks: Vec<ShellTaskSnapshot>,
    /// So a list requested before an eviction cannot bring the row back.
    evicted: HashSet<String>,
    subscribed: bool,
    /// Told when the host lets go of a row and its transcript, so a page showing it can close: `(conversation, task)`.
    /// A row kept would still open, onto a pane that can no longer read anything.
    pub on_evicted: Option<Rc<dyn Fn(&mut App, &str, &str)>>,
}

impl ShellTaskStore {
    /// One `AppPushEvent`. Returns the (conversation, task) a `shellTaskEvicted` removed, for a pane showing it to close.
    pub fn apply_event(&mut self, event: &Value) -> Option<(String, String)> {
        match event.get("type").and_then(Value::as_str)? {
            "shellTaskStarted" | "shellTaskEnded" => {
                let task: ShellTaskSnapshot = serde_json::from_value(event.get("task")?.clone()).ok()?;
                // A host that restarted mints ids from one again, so a start clears an old tombstone.
                if event["type"] == "shellTaskStarted" {
                    self.evicted.remove(&task.shell_task_id);
                }
                self.tasks.retain(|t| t.shell_task_id != task.shell_task_id);
                self.tasks.push(task);
                None
            }
            "shellTaskEvicted" => {
                let id = event.get("shellTaskId")?.as_str()?.to_owned();
                let conversation = event.get("conversationId")?.as_str()?.to_owned();
                self.evicted.insert(id.clone());
                self.tasks.retain(|t| t.shell_task_id != id);
                Some((conversation, id))
            }
            _ => None,
        }
    }

    /// Takes the answer of `list_shell_tasks` for one conversation, leaving every other conversation's rows alone.
    pub fn reconcile(&mut self, conversation_id: &str, listed: Vec<ShellTaskSnapshot>) {
        let evicted = &self.evicted;
        self.tasks.retain(|t| t.conversation_id != conversation_id);
        self.tasks.extend(listed.into_iter().filter(|t| !evicted.contains(&t.shell_task_id)));
    }

    pub fn for_conversation(&self, conversation_id: &str) -> Vec<ShellTaskSnapshot> {
        self.tasks.iter().filter(|t| t.conversation_id == conversation_id).cloned().collect()
    }
}

/// Starts following the host's push events. Idempotent, so whoever shows a task pane first may call it.
pub fn install(app: &mut App) {
    if std::mem::replace(&mut app.local::<ShellTaskStore>().subscribed, true) {
        return;
    }
    app.on_push_event(|app, event| {
        let store = app.local::<ShellTaskStore>();
        let evicted = store.apply_event(event);
        let handler = store.on_evicted.clone();
        if let (Some((conversation, task)), Some(handler)) = (evicted, handler) {
            handler(app, &conversation, &task);
        }
        fluent::el::repaint();
    });
}

/// `listShellTasks` into the store: push events carry the live changes, this recovers what was missed.
pub fn reconcile(app: &App, conversation_id: &str) {
    let id = conversation_id.to_owned();
    app.invoke::<Vec<ShellTaskSnapshot>>("list_shell_tasks", json!({ "conversationId": conversation_id }), move |app, result| match result {
        Ok(tasks) => {
            app.local::<ShellTaskStore>().reconcile(&id, tasks);
            fluent::el::repaint();
        }
        Err(e) => eprintln!("Failed to list shell tasks: {e}"),
    });
}

/// Asks one running command to stop. False means it had already finished, or belongs to another conversation.
pub fn stop_shell_task(app: &App, conversation_id: &str, shell_task_id: &str, done: impl FnOnce(&mut App, Result<bool, String>) + Send + 'static) {
    app.invoke("stop_shell_task", json!({ "conversationId": conversation_id, "shellTaskId": shell_task_id }), done);
}

/// Stops a task that survives across turns: a child's name, `workflow:<runId>` or `shell:<id>`. Not the model run.
pub fn stop_conversation_task(app: &App, conversation_id: &str, task: &str, done: impl FnOnce(&mut App, Result<bool, String>) + Send + 'static) {
    app.invoke("stop_conversation_task", json!({ "conversationId": conversation_id, "task": task }), done);
}

/// Starts watching one command. The host installs the channel and takes its buffer in one step, so nothing printed
/// between "read" and "subscribe" goes missing.
pub fn open_shell_task_output(app: &App, conversation_id: &str, shell_task_id: &str, channel: &Channel, done: impl FnOnce(&mut App, Result<ShellTaskOutputHandle, String>) + Send + 'static) {
    app.invoke("open_shell_task_output", json!({ "conversationId": conversation_id, "shellTaskId": shell_task_id, "onEvent": channel.arg() }), done);
}

/// Stops watching. Without a subscription id whatever sink is installed goes, which is only right for a caller that
/// never received a handle.
pub fn detach_shell_task_output(app: &App, conversation_id: &str, shell_task_id: &str, subscription_id: Option<u64>) {
    app.invoke_logged("detach_shell_task_output", json!({ "conversationId": conversation_id, "shellTaskId": shell_task_id, "subscriptionId": subscription_id }));
}
