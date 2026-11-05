//! The host's terminal commands and what they carry: upstream's `lib/terminal.ts`.

use std::sync::atomic::{AtomicU64, Ordering};

use mewrk_client::prelude::*;
use serde::Deserialize;

pub use mewrk_client::workspaces::{terminal_shell_label, TerminalShell};

/// The name a shell goes by on the wire, which is also what a tab's number is counted under.
pub fn terminal_shell_wire_name(shell: TerminalShell) -> String {
    serde_json::to_value(shell).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// Where a terminal runs and what it runs: the conversation's workspace number, the same 1-based address the model uses,
/// and the shell. `None` leaves either to the host: workspace 1, and that machine's default shell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalLaunchChoice {
    pub workspace: Option<u32>,
    pub shell: Option<TerminalShell>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalCommandState {
    pub revision: u64,
    pub status: TerminalCommandStatus,
    pub command_id: Option<String>,
    pub command_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalCommandStatus {
    Idle,
    Running,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalEvent {
    Output {
        #[serde(rename = "sessionId")]
        session_id: String,
        data: Vec<u8>,
    },
    CommandState {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "commandState")]
        command_state: TerminalCommandState,
    },
    Ready {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Exit {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "exitCode")]
        exit_code: Option<i64>,
    },
    Error {
        #[serde(rename = "sessionId")]
        session_id: String,
        message: String,
    },
}

impl TerminalEvent {
    pub fn session_id(&self) -> &str {
        match self {
            Self::Output { session_id, .. } | Self::CommandState { session_id, .. } | Self::Ready { session_id } | Self::Exit { session_id, .. } | Self::Error { session_id, .. } => session_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalPhase {
    Idle,
    Connecting,
    Running,
    Closing,
    Exited,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSessionState {
    pub terminal_id: String,
    pub conversation_id: String,
    pub label: String,
    pub phase: TerminalPhase,
    pub busy: bool,
    pub has_history: bool,
    pub cwd: String,
    pub shell: String,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTerminalResult {
    pub session_id: String,
    pub created: bool,
    pub running: bool,
    pub ready: bool,
    pub cwd: String,
    pub shell: String,
    pub snapshot: Vec<u8>,
    pub command_state: TerminalCommandState,
}

/// Mints attempts for every panel, so that a panel mounted again for the same terminal can never mistake a late answer
/// to its predecessor's open for its own.
pub(crate) fn next_attempt() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The arguments `open_terminal` takes. `draft_workspace_id` is set only for the draft, a new task the host has no row
/// for yet, and names the project it is aimed at. Its terminals are opened under `conversation_id`, the id it will
/// materialize as, so they are that conversation's once it is real.
pub fn open_terminal_args(conversation_id: &str, terminal_id: &str, cols: u16, rows: u16, launch: TerminalLaunchChoice, draft_workspace_id: Option<&str>, channel: &Channel) -> Value {
    json!({
        "conversationId": conversation_id,
        "terminalId": terminal_id,
        "cols": cols,
        "rows": rows,
        "workspace": launch.workspace,
        "shell": launch.shell,
        "draftWorkspaceId": draft_workspace_id,
        "onEvent": channel.arg(),
    })
}

pub fn write_terminal(app: &App, conversation_id: &str, terminal_id: &str, session_id: &str, data: &str, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
    app.invoke::<Value>("write_terminal", json!({ "conversationId": conversation_id, "terminalId": terminal_id, "sessionId": session_id, "data": data }), move |app, result| done(app, result.map(|_| ())));
}

pub fn resize_terminal(app: &App, conversation_id: &str, terminal_id: &str, session_id: &str, cols: u16, rows: u16, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
    app.invoke::<Value>("resize_terminal", json!({ "conversationId": conversation_id, "terminalId": terminal_id, "sessionId": session_id, "cols": cols, "rows": rows }), move |app, result| done(app, result.map(|_| ())));
}

pub fn detach_args(conversation_id: &str, terminal_id: &str, session_id: &str) -> Value {
    json!({ "conversationId": conversation_id, "terminalId": terminal_id, "sessionId": session_id })
}

pub fn close_args(conversation_id: &str, terminal_id: &str) -> Value {
    json!({ "conversationId": conversation_id, "terminalId": terminal_id })
}

/// How many of a conversation's terminals still have a shell running at the host, whether or not anyone is looking at
/// them. A draft's are counted under the id it will materialize as.
pub fn live_terminal_count(app: &App, conversation_id: &str, done: impl FnOnce(&mut App, Result<usize, String>) + Send + 'static) {
    app.invoke::<usize>("live_terminal_count", json!({ "conversationId": conversation_id }), done);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_read_the_way_the_host_writes_them() {
        let output: TerminalEvent = serde_json::from_value(json!({"type": "output", "sessionId": "s", "data": [104, 105]})).unwrap();
        assert_eq!(output, TerminalEvent::Output { session_id: "s".into(), data: b"hi".to_vec() });
        let state: TerminalEvent = serde_json::from_value(json!({"type": "command_state", "sessionId": "s", "commandState": {"revision": 3, "status": "running", "commandId": "c", "commandCount": 2}})).unwrap();
        assert!(matches!(state, TerminalEvent::CommandState { command_state: TerminalCommandState { revision: 3, status: TerminalCommandStatus::Running, .. }, .. }));
        let exit: TerminalEvent = serde_json::from_value(json!({"type": "exit", "sessionId": "s", "exitCode": null})).unwrap();
        assert_eq!(exit, TerminalEvent::Exit { session_id: "s".into(), exit_code: None });
        let ready: TerminalEvent = serde_json::from_value(json!({"type": "ready", "sessionId": "s"})).unwrap();
        assert_eq!(ready.session_id(), "s");
    }

    #[test]
    fn a_shell_is_named_on_the_wire_the_way_the_host_reads_it() {
        assert_eq!(terminal_shell_wire_name(TerminalShell::Powershell), "powershell");
        assert_eq!(terminal_shell_wire_name(TerminalShell::Zsh), "zsh");
        assert_eq!(serde_json::from_value::<TerminalShell>(json!("fish")).unwrap(), TerminalShell::Fish);
    }
}
