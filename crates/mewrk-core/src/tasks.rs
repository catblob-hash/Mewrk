//! What runs beside a turn: subagents and background shells, and the per-conversation state they share.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use agent_rt::Cancel;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::model::now_ms;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Agent,
    Shell,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Completed,
    Failed(String),
    Stopped,
}

impl TaskState {
    pub fn finished(&self) -> bool {
        *self != TaskState::Running
    }
}

/// How much of a task's output is kept in memory. The head is dropped, because the end is what says how it went.
const OUTPUT_CAP: usize = 256 << 10;

pub struct Task {
    pub id: String,
    /// The address `task_wait` and `send_message` take.
    pub name: String,
    pub label: String,
    pub kind: TaskKind,
    pub started_at: u64,
    pub cancel: Cancel,
    pub tokens: AtomicU64,
    pub tool_calls: AtomicU32,
    pub finished_at: AtomicU64,
    state: Mutex<TaskState>,
    output: Mutex<String>,
    /// Messages for a running subagent, delivered at its next step.
    pub mailbox: Mutex<Vec<String>>,
    done: Notify,
    /// A subagent's transcript.
    pub run: Mutex<Option<Arc<crate::engine::RunHandle>>>,
    /// Set once a waiting tool has reported this task, so it does not also arrive as a notification.
    pub reported: AtomicBool,
    /// The subagent whose task this is, by its task id. `None` is the conversation's own.
    /// A child waits for and lists its own tasks, never its parent's or itself.
    pub owner: Mutex<Option<String>>,
    /// The stop came from `task_stop` rather than a person, and the result has to say which.
    pub stopped_by_model: AtomicBool,
}

impl Task {
    pub fn new(name: impl Into<String>, label: impl Into<String>, kind: TaskKind) -> Arc<Self> {
        Arc::new(Self {
            id: crate::model::new_id(),
            name: name.into(),
            label: label.into(),
            kind,
            started_at: now_ms(),
            cancel: Cancel::new(),
            tokens: AtomicU64::new(0),
            tool_calls: AtomicU32::new(0),
            finished_at: AtomicU64::new(0),
            state: Mutex::new(TaskState::Running),
            output: Mutex::new(String::new()),
            mailbox: Mutex::new(Vec::new()),
            done: Notify::new(),
            run: Mutex::new(None),
            reported: AtomicBool::new(false),
            owner: Mutex::new(None),
            stopped_by_model: AtomicBool::new(false),
        })
    }

    pub fn state(&self) -> TaskState {
        self.state.lock().clone()
    }

    pub fn finish(&self, state: TaskState) {
        {
            let mut s = self.state.lock();
            if s.finished() {
                return;
            }
            *s = state;
        }
        self.finished_at.store(now_ms(), Ordering::Relaxed);
        self.done.notify_waiters();
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    pub fn append_output(&self, s: &str) {
        let mut out = self.output.lock();
        out.push_str(s);
        if out.len() > OUTPUT_CAP * 2 {
            let mut cut = out.len() - OUTPUT_CAP;
            while !out.is_char_boundary(cut) {
                cut += 1;
            }
            out.drain(..cut);
        }
    }

    pub fn set_output(&self, s: String) {
        *self.output.lock() = s;
    }

    pub fn output(&self) -> String {
        self.output.lock().clone()
    }

    /// Resolves once the task has finished.
    pub async fn wait(&self) {
        loop {
            let notified = self.done.notified();
            if self.state().finished() {
                return;
            }
            notified.await;
        }
    }
}

/// Everything about a conversation that outlives one run, and that its subagents share.
#[derive(Default)]
pub struct Session {
    pub tasks: Mutex<Vec<Arc<Task>>>,
    /// Host messages waiting for the next request: a task finished, and so on.
    pub notes: Mutex<Vec<String>>,
    /// Tools the user has allowed for the rest of this conversation.
    pub always_allowed: Mutex<HashSet<String>>,
    /// What each file looked like when the model last read it, keyed by canonical path. Writes that
    /// would overwrite something the model has not seen are refused.
    pub reads: Mutex<HashMap<PathBuf, FileStamp>>,
    /// Plan mode as the run sees it. The UI's toggle and `exit_plan_mode` both land here.
    pub plan_mode: AtomicBool,
    pub plan_text: Mutex<String>,
    /// Bumped every time the task list changes, so the UI can skip rebuilding its rows.
    pub generation: AtomicU64,
    /// Where each workspace's next shell command starts, once a command has moved inside it.
    pub shell_cwd: Mutex<HashMap<PathBuf, PathBuf>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime_ms: u64,
    pub len: u64,
}

impl Session {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn add_task(&self, task: Arc<Task>) {
        self.tasks.lock().push(task);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn find_task(&self, key: &str) -> Option<Arc<Task>> {
        self.tasks.lock().iter().find(|t| t.id == key || t.name == key).cloned()
    }

    pub fn snapshot(&self) -> Vec<Arc<Task>> {
        self.tasks.lock().clone()
    }

    pub fn has_notes(&self) -> bool {
        !self.notes.lock().is_empty()
    }

    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock())
    }

    pub fn add_note(&self, text: String) {
        self.notes.lock().push(text);
    }

    pub fn stop_all(&self) {
        for t in self.tasks.lock().iter() {
            t.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_keeps_the_tail_and_cuts_on_a_character() {
        let t = Task::new("a", "a", TaskKind::Shell);
        t.append_output(&"世".repeat(OUTPUT_CAP));
        t.append_output("end");
        let out = t.output();
        assert!(out.len() <= OUTPUT_CAP * 2);
        assert!(out.ends_with("end"));
    }

    #[test]
    fn the_first_final_state_wins() {
        let t = Task::new("a", "a", TaskKind::Shell);
        t.finish(TaskState::Completed);
        t.finish(TaskState::Stopped);
        assert_eq!(t.state(), TaskState::Completed);
    }
}
