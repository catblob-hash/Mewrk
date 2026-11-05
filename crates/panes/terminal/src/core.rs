//! What a terminal panel knows and decides, with nothing in it that touches the host: upstream's `TerminalPanel`, its
//! refs and effects written as a state machine. Everything asynchronous there is a call that returns `Effect`s here and
//! a method the answer comes back through, so that every race upstream guards against can be scripted in a test.

use mewrk_client::t;

use crate::api::{next_attempt, OpenTerminalResult, TerminalCommandState, TerminalCommandStatus, TerminalEvent, TerminalLaunchChoice, TerminalPhase, TerminalSessionState};

/// What the panel wants done. `driver` carries these out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// The emulator back to blank: xterm's `reset` and `clear`.
    Reset,
    /// Bytes for the emulator.
    Feed(Vec<u8>),
    /// Starts a session: detach `previous`, run `beforeOpen`, then `open_terminal`. Whatever comes back, comes back
    /// through `opened` with this attempt.
    Open { attempt: u64, previous: Option<String> },
    Write { session: String, data: String },
    Resize { session: String, cols: u16, rows: u16 },
    /// Ask the host to kill the session, the panel's `onClose`. The answer comes back through `closed`.
    Close { attempt: u64 },
    /// A session that was left running and is not coming back.
    Detach { session: String },
    /// The shell exited cleanly on its own: `onCleanExit`.
    CleanExit,
}

pub struct PanelCore {
    pub conversation_id: String,
    pub terminal_id: String,
    pub label: String,
    pub launch: TerminalLaunchChoice,
    pub draft_workspace_id: Option<String>,
    pub open: bool,
    pub input_disabled_reason: Option<String>,
    pub phase: TerminalPhase,
    pub busy: bool,
    pub has_history: bool,
    pub cwd: String,
    pub shell: String,
    pub exit_code: Option<i64>,
    pub failure: Option<String>,
    pub session_id: Option<String>,
    /// Whether the emulator should take focus on its next look: xterm's `focus()`.
    pub want_focus: bool,
    /// Which session the current attempt was told it has, once the open has answered. Until then events wait.
    accepted: Option<String>,
    pending: Vec<TerminalEvent>,
    attempt: u64,
    /// The open still in flight, so a close can wait for the host to have a session to kill.
    opening: bool,
    /// A close waiting for that open.
    close_after_open: bool,
    running: bool,
    control_ready: bool,
    command_revision: i64,
}

impl PanelCore {
    pub fn new(conversation_id: &str, terminal_id: &str, label: &str, initial: Option<&TerminalSessionState>) -> Self {
        Self {
            conversation_id: conversation_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
            label: label.to_owned(),
            launch: TerminalLaunchChoice::default(),
            draft_workspace_id: None,
            open: false,
            input_disabled_reason: None,
            phase: TerminalPhase::Idle,
            busy: initial.is_some_and(|s| s.busy),
            has_history: initial.is_some_and(|s| s.has_history),
            cwd: initial.map_or_else(String::new, |s| s.cwd.clone()),
            shell: initial.map_or_else(String::new, |s| s.shell.clone()),
            exit_code: None,
            failure: None,
            session_id: None,
            want_focus: false,
            accepted: None,
            pending: Vec::new(),
            attempt: next_attempt(),
            opening: false,
            close_after_open: false,
            running: false,
            control_ready: false,
            command_revision: -1,
        }
    }

    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    /// What the rest of the app is told about this terminal.
    pub fn state(&self) -> TerminalSessionState {
        TerminalSessionState {
            terminal_id: self.terminal_id.clone(),
            conversation_id: self.conversation_id.clone(),
            label: self.label.clone(),
            phase: self.phase,
            busy: self.busy,
            has_history: self.has_history,
            cwd: self.cwd.clone(),
            shell: self.shell.clone(),
            session_id: self.session_id.clone(),
        }
    }

    /// Whether keystrokes may reach the shell right now. Closing is part of the gate itself, not of the paths that open
    /// it, so an open or ready handshake that lands while the host is killing the session cannot let input through.
    pub fn input_accepted(&self) -> bool {
        self.control_ready && self.input_disabled_reason.is_none() && self.phase != TerminalPhase::Closing
    }

    fn fail_session(&mut self, message: String) {
        self.running = false;
        self.control_ready = false;
        self.failure = Some(message);
        // A failure reported while the host is still killing the session is kept for the close to end on; the close
        // owns the phase until then.
        if self.phase != TerminalPhase::Closing {
            self.phase = TerminalPhase::Error;
        }
    }

    fn apply_command_state(&mut self, next: &TerminalCommandState) {
        if next.revision as i64 <= self.command_revision {
            return;
        }
        self.command_revision = next.revision as i64;
        self.busy = next.status == TerminalCommandStatus::Running;
        self.has_history = next.command_count > 0;
    }

    fn process_event(&mut self, event: TerminalEvent, expected_session: &str, effects: &mut Vec<Effect>) {
        if self.session_id.as_deref() != Some(expected_session) || event.session_id() != expected_session {
            return;
        }
        match event {
            TerminalEvent::Output { data, .. } => {
                if !data.is_empty() {
                    effects.push(Effect::Feed(data));
                }
            }
            TerminalEvent::CommandState { command_state, .. } => self.apply_command_state(&command_state),
            TerminalEvent::Ready { .. } => {
                self.control_ready = true;
                if self.running && self.phase != TerminalPhase::Closing {
                    self.phase = TerminalPhase::Running;
                }
                self.want_focus = self.open;
            }
            TerminalEvent::Error { message, .. } => {
                let message = message.trim();
                self.fail_session(if message.is_empty() { t("终端读取失败", "Failed to read from terminal").to_owned() } else { message.to_owned() });
            }
            TerminalEvent::Exit { exit_code, .. } => {
                self.running = false;
                self.control_ready = false;
                self.busy = false;
                self.exit_code = exit_code;
                // An exit that lands mid-close is recorded but the close still ends the phase.
                if self.phase != TerminalPhase::Error && self.phase != TerminalPhase::Closing {
                    self.phase = TerminalPhase::Exited;
                    // A clean exit ends the session on its own, like the reference shell folding its tab away; anything
                    // else stays on screen for the retry.
                    if exit_code == Some(0) {
                        effects.push(Effect::CleanExit);
                    }
                }
            }
        }
    }

    /// An event from the channel opened for `attempt`.
    pub fn on_event(&mut self, attempt: u64, event: TerminalEvent) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.attempt != attempt {
            return effects;
        }
        match self.accepted.clone() {
            Some(session) => self.process_event(event, &session, &mut effects),
            None => self.pending.push(event),
        }
        effects
    }

    /// Starts a session, unless one is on its way or the host is killing this one.
    pub fn start_session(&mut self) -> Vec<Effect> {
        if matches!(self.phase, TerminalPhase::Connecting | TerminalPhase::Closing) {
            return Vec::new();
        }
        self.attempt = next_attempt();
        self.running = false;
        let previous = self.session_id.take();
        self.control_ready = false;
        self.command_revision = -1;
        self.failure = None;
        self.exit_code = None;
        self.cwd.clear();
        self.shell.clear();
        self.accepted = None;
        self.pending.clear();
        self.opening = true;
        self.phase = TerminalPhase::Connecting;
        vec![Effect::Reset, Effect::Open { attempt: self.attempt, previous }]
    }

    /// An open was given up on before it reached the host: something else took over the attempt.
    pub fn open_abandoned(&mut self, attempt: u64) -> Vec<Effect> {
        if self.attempt == attempt {
            self.opening = false;
        }
        Vec::new()
    }

    /// What the host answered to the open for `attempt`.
    pub fn opened(&mut self, attempt: u64, result: Result<OpenTerminalResult, String>) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.attempt != attempt {
            // A stale answer. Its session may be the very one a newer attempt holds, so it must not be detached.
            return effects;
        }
        self.opening = false;
        match result {
            Ok(result) => {
                self.accepted = Some(result.session_id.clone());
                self.session_id = Some(result.session_id.clone());
                self.running = result.running;
                self.control_ready = result.ready;
                self.cwd = result.cwd;
                self.shell = result.shell;
                if !result.snapshot.is_empty() {
                    effects.push(Effect::Feed(result.snapshot));
                }
                // A close that arrived while this open was in flight keeps its phase; it settles the session itself
                // once the host has killed it.
                if self.phase != TerminalPhase::Closing {
                    self.phase = if !result.running {
                        TerminalPhase::Exited
                    } else if result.ready {
                        TerminalPhase::Running
                    } else {
                        TerminalPhase::Connecting
                    };
                }
                self.apply_command_state(&result.command_state);
                if !result.running {
                    self.busy = false;
                }
                for event in std::mem::take(&mut self.pending) {
                    self.process_event(event, &result.session_id, &mut effects);
                }
                self.want_focus = result.running && result.ready && self.open;
            }
            Err(message) => {
                let message = message.trim();
                self.fail_session(if message.is_empty() { t("无法连接终端", "Unable to connect to terminal").to_owned() } else { message.to_owned() });
            }
        }
        if std::mem::take(&mut self.close_after_open) {
            effects.push(Effect::Close { attempt: self.attempt });
        }
        effects
    }

    /// The tab asks for the kill. A second call while the first is pending asks nothing of the host.
    pub fn close_session(&mut self) -> Vec<Effect> {
        if self.phase == TerminalPhase::Closing {
            return Vec::new();
        }
        // Dismissing a dead session keeps its verdict; only a live one is being killed.
        if matches!(self.phase, TerminalPhase::Connecting | TerminalPhase::Running) {
            self.failure = None;
        }
        self.phase = TerminalPhase::Closing;
        // An open still in flight has not given the host a session to kill yet.
        if self.opening {
            self.close_after_open = true;
            return Vec::new();
        }
        vec![Effect::Close { attempt: self.attempt }]
    }

    /// What the host answered to the kill. A killed session sends no exit event, so the panel settles itself on this
    /// instead of waiting for one that never comes.
    pub fn closed(&mut self, attempt: u64, result: Result<(), String>) {
        if self.attempt != attempt {
            return;
        }
        match result {
            Ok(()) => {
                self.running = false;
                self.control_ready = false;
                self.session_id = None;
                self.busy = false;
                // Whatever the session reported while it was going down stays on the record.
                self.phase = if self.failure.is_some() { TerminalPhase::Error } else { TerminalPhase::Exited };
            }
            Err(message) => {
                // Leave closing first: `fail_session` keeps a failure reported mid-close.
                self.phase = TerminalPhase::Error;
                let message = message.trim();
                self.fail_session(if message.is_empty() { t("无法连接终端", "Unable to connect to terminal").to_owned() } else { message.to_owned() });
            }
        }
    }

    /// The panel is told it is, or is not, the one on screen. Becoming visible is not a request for a shell: a tab is
    /// shown and hidden every time the user moves between terminals, so only a terminal that never started starts, and
    /// that is the first look's job, once it knows its size. A session that ended keeps its verdict until the retry.
    pub fn set_open(&mut self, open: bool) {
        if open && !self.open && self.phase == TerminalPhase::Running {
            self.want_focus = true;
        }
        self.open = open;
    }

    /// Whether the panel should start its session now that it has a size.
    pub fn should_start(&self) -> bool {
        self.open && self.phase == TerminalPhase::Idle
    }

    /// Whether anything can be sent to the shell at all: the check every `onData` starts with. Protocol replies pass
    /// before the handshake is ready, but nothing goes to a shell the host is killing.
    pub fn can_send(&self) -> bool {
        self.sendable_session().is_some()
    }

    fn sendable_session(&self) -> Option<String> {
        if !self.running || self.phase == TerminalPhase::Closing {
            return None;
        }
        self.session_id.clone()
    }

    /// Something from the emulator for the shell. Any Enter may submit a recalled command even when the renderer has
    /// never seen its text, so it reserves the command optimistically through `gate`, which may say no. The host's
    /// barrier remains the authority on whether the line runs.
    pub fn on_data(&mut self, data: String, gate: impl FnOnce() -> bool) -> Vec<Effect> {
        let Some(session) = self.sendable_session() else { return Vec::new() };
        let starts_command = data.contains(['\r', '\n']);
        if starts_command {
            if !gate() {
                return Vec::new();
            }
            self.busy = true;
        }
        vec![Effect::Write { session, data }]
    }

    pub fn on_resize(&self, cols: u16, rows: u16) -> Vec<Effect> {
        if !self.open {
            return Vec::new();
        }
        match self.sendable_or_closing_session() {
            Some(session) => vec![Effect::Resize { session, cols, rows }],
            None => Vec::new(),
        }
    }

    fn sendable_or_closing_session(&self) -> Option<String> {
        self.running.then(|| self.session_id.clone()).flatten()
    }

    /// A write or a resize failed. Only the session it was for can fail on its account.
    pub fn io_failed(&mut self, session: &str, message: String) {
        if self.session_id.as_deref() == Some(session) {
            let message = message.trim();
            self.fail_session(if message.is_empty() { t("无法连接终端", "Unable to connect to terminal").to_owned() } else { message.to_owned() });
        }
    }

    /// The panel goes away: `Detach` if a session was left running. Detached, the renderer no longer sees this terminal's
    /// command state, so the last report says so rather than pin a busy flag nobody will clear.
    pub fn unmount(&mut self) -> Vec<Effect> {
        self.attempt = next_attempt();
        self.running = false;
        self.control_ready = false;
        self.phase = TerminalPhase::Idle;
        self.busy = false;
        self.session_id.take().map(|session| Effect::Detach { session }).into_iter().collect()
    }

    /// What the panel says about itself: upstream's `statusLabel`, which is also the overlay's message once settled.
    pub fn status_label(&self) -> String {
        if let Some(reason) = &self.input_disabled_reason {
            return reason.clone();
        }
        match self.phase {
            TerminalPhase::Connecting => t("正在连接", "Connecting").to_owned(),
            TerminalPhase::Running => {
                if self.busy {
                    t("运行中", "Running").to_owned()
                } else if self.has_history {
                    t("待机", "Idle").to_owned()
                } else {
                    t("空终端", "Empty terminal").to_owned()
                }
            }
            TerminalPhase::Closing => t("正在终止", "Terminating").to_owned(),
            TerminalPhase::Exited => match self.exit_code {
                None => t("已退出", "Exited").to_owned(),
                Some(code) => mewrk_client::tf("已退出（代码 {code}）", "Exited (code {code})", &[("code", &code)]),
            },
            TerminalPhase::Error => self.failure.clone().unwrap_or_else(|| t("终端错误", "Terminal error").to_owned()),
            TerminalPhase::Idle => t("未启动", "Not started").to_owned(),
        }
    }

    /// A session that ended is the only one worth restarting.
    pub fn settled(&self) -> bool {
        matches!(self.phase, TerminalPhase::Exited | TerminalPhase::Error)
    }

    pub fn veiled(&self) -> bool {
        matches!(self.phase, TerminalPhase::Connecting | TerminalPhase::Closing)
    }
}

/// What the terminal's panel is called in the accessibility tree, and what tests and the tab strip address it by.
pub fn terminal_panel_id(conversation_id: &str, terminal_id: &str) -> String {
    format!("conversation-terminal-{conversation_id}-{terminal_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::TerminalCommandStatus::{Idle, Running};

    fn state(revision: u64, status: TerminalCommandStatus, count: u64) -> TerminalCommandState {
        TerminalCommandState { revision, status, command_id: (status == Running).then(|| "c".to_owned()), command_count: count }
    }

    fn response(session: &str) -> OpenTerminalResult {
        OpenTerminalResult { session_id: session.into(), created: true, running: true, ready: true, cwd: "C:/workspace".into(), shell: "PowerShell".into(), snapshot: Vec::new(), command_state: state(0, Idle, 0) }
    }

    fn panel() -> PanelCore {
        let mut p = PanelCore::new("conversation-1", "terminal-1", "终端 1", None);
        p.set_open(true);
        p
    }

    /// The panel with a session open, the way every test starts.
    fn running() -> (PanelCore, u64) {
        let mut p = panel();
        assert!(p.should_start());
        p.start_session();
        let attempt = p.attempt();
        p.opened(attempt, Ok(response("session-1")));
        (p, attempt)
    }

    fn output(session: &str, data: &[u8]) -> TerminalEvent {
        TerminalEvent::Output { session_id: session.into(), data: data.to_vec() }
    }

    #[test]
    fn only_starts_when_first_opened_and_folding_does_not_detach() {
        let mut p = PanelCore::new("conversation-1", "terminal-1", "终端 1", None);
        assert!(!p.should_start(), "collapsed: no shell yet");
        p.set_open(true);
        assert!(p.should_start());
        let effects = p.start_session();
        assert_eq!(effects, [Effect::Reset, Effect::Open { attempt: p.attempt(), previous: None }]);
        assert_eq!(p.status_label(), "正在连接");
        p.opened(p.attempt(), Ok(response("session-1")));
        assert_eq!(p.status_label(), "空终端");
        p.set_open(false);
        assert_eq!(p.phase, TerminalPhase::Running);
        assert_eq!(p.unmount(), [Effect::Detach { session: "session-1".into() }]);
    }

    #[test]
    fn writes_the_snapshot_before_queued_events_and_ignores_events_from_another_session() {
        let mut p = panel();
        p.start_session();
        let attempt = p.attempt();
        assert!(p.on_event(attempt, TerminalEvent::CommandState { session_id: "session-1".into(), command_state: state(2, Running, 1) }).is_empty());
        assert!(p.on_event(attempt, output("session-1", b"B")).is_empty());
        assert!(p.on_event(attempt, output("old-session", b"X")).is_empty());
        let effects = p.opened(attempt, Ok(OpenTerminalResult { snapshot: b"A".to_vec(), command_state: state(1, Idle, 0), ..response("session-1") }));
        assert_eq!(effects, [Effect::Feed(b"A".to_vec()), Effect::Feed(b"B".to_vec())]);
        assert_eq!(p.status_label(), "运行中");
    }

    #[test]
    fn scopes_input_and_resize_to_the_active_session() {
        let (mut p, _) = running();
        let effects = p.on_data("pwd\r".into(), || true);
        assert_eq!(effects, [Effect::Write { session: "session-1".into(), data: "pwd\r".into() }]);
        assert_eq!(p.status_label(), "运行中");
        assert_eq!(p.on_resize(118, 34), [Effect::Resize { session: "session-1".into(), cols: 118, rows: 34 }]);
    }

    #[test]
    fn uses_the_open_response_command_state_even_when_its_snapshot_looks_like_a_prompt() {
        let mut p = panel();
        p.start_session();
        p.opened(p.attempt(), Ok(OpenTerminalResult { snapshot: b"\r\nPS C:\\workspace> ".to_vec(), command_state: state(8, Running, 3), ..response("session-1") }));
        assert_eq!(p.status_label(), "运行中");
        assert!(p.state().busy && p.state().has_history);
    }

    #[test]
    fn blocks_terminal_input_while_a_git_mutation_owns_the_workspace() {
        let (mut p, _) = running();
        p.input_disabled_reason = Some("Git 写操作进行中".into());
        assert!(!p.input_accepted());
        assert_eq!(p.status_label(), "Git 写操作进行中");
        p.input_disabled_reason = None;
        assert!(p.input_accepted());
    }

    #[test]
    fn relays_protocol_replies_before_the_trusted_shell_handshake_is_ready() {
        let mut p = panel();
        p.start_session();
        let attempt = p.attempt();
        p.opened(attempt, Ok(OpenTerminalResult { ready: false, ..response("session-1") }));
        assert_eq!(p.status_label(), "正在连接");
        assert!(!p.input_accepted());
        assert_eq!(p.on_data("\x1b[1;1R".into(), || panic!("a reply starts no command")), [Effect::Write { session: "session-1".into(), data: "\x1b[1;1R".into() }]);
        p.on_event(attempt, TerminalEvent::Ready { session_id: "session-1".into() });
        assert_eq!(p.status_label(), "空终端");
        assert!(p.input_accepted());
    }

    #[test]
    fn does_not_send_a_command_when_the_workspace_lock_loses_the_start_race() {
        let (mut p, _) = running();
        assert!(p.on_data("git status\r".into(), || false).is_empty());
        assert_eq!(p.status_label(), "空终端");
    }

    #[test]
    fn a_stale_open_response_does_nothing() {
        let mut p = panel();
        p.start_session();
        let first = p.attempt();
        // Failing makes a new attempt possible, which is how a stale answer can exist here.
        p.opened(first, Err("no".into()));
        p.start_session();
        let second = p.attempt();
        assert_ne!(first, second);
        p.opened(second, Ok(response("shared-session")));
        assert!(p.opened(first, Ok(response("shared-session"))).is_empty());
        assert_eq!(p.session_id.as_deref(), Some("shared-session"));
    }

    #[test]
    fn restores_the_authoritative_idle_state_after_a_command_finishes_while_detached() {
        let (mut p, _) = running();
        p.on_event(p.attempt(), TerminalEvent::CommandState { session_id: "session-1".into(), command_state: state(3, Running, 1) });
        let left = p.state();
        let mut p = PanelCore::new("conversation-1", "terminal-1", "终端 1", Some(&left));
        p.set_open(true);
        assert!(p.busy, "what the store last knew shows until the host answers");
        p.start_session();
        p.opened(p.attempt(), Ok(OpenTerminalResult { created: false, snapshot: b"\r\nfinished".to_vec(), command_state: state(4, Idle, 1), ..response("session-1") }));
        assert_eq!(p.status_label(), "待机");
    }

    #[test]
    fn clears_an_active_command_when_the_shell_exits() {
        let (mut p, attempt) = running();
        p.on_event(attempt, TerminalEvent::CommandState { session_id: "session-1".into(), command_state: state(1, Running, 1) });
        let effects = p.on_event(attempt, TerminalEvent::Exit { session_id: "session-1".into(), exit_code: Some(0) });
        assert_eq!(effects, [Effect::CleanExit]);
        assert_eq!(p.status_label(), "已退出（代码 0）");
        let s = p.state();
        assert_eq!((s.phase, s.busy, s.has_history), (TerminalPhase::Exited, false, true));
    }

    #[test]
    fn shows_streamed_errors_and_only_reconnects_when_asked() {
        let (mut p, attempt) = running();
        p.on_event(attempt, TerminalEvent::CommandState { session_id: "session-1".into(), command_state: state(1, Running, 1) });
        p.on_event(attempt, TerminalEvent::Error { session_id: "session-1".into(), message: "PTY 读取失败".into() });
        assert_eq!(p.status_label(), "PTY 读取失败");
        let s = p.state();
        assert_eq!((s.phase, s.busy, s.has_history), (TerminalPhase::Error, true, true));
        assert!(!p.should_start(), "reconnecting on its own would erase the failure");
        let effects = p.start_session();
        assert!(matches!(effects.as_slice(), [Effect::Reset, Effect::Open { .. }]));
    }

    #[test]
    fn closes_on_the_hosts_confirmation_and_stays_settled_until_the_retry() {
        let (mut p, attempt) = running();
        p.on_data("Get-Location\r".into(), || true);
        assert!(!p.settled());
        let effects = p.close_session();
        assert_eq!(effects, [Effect::Close { attempt }]);
        assert_eq!(p.status_label(), "正在终止");
        assert!(!p.input_accepted());
        assert!(p.close_session().is_empty(), "a second close asks the host nothing");

        p.closed(attempt, Ok(()));
        assert_eq!(p.status_label(), "已退出");
        let s = p.state();
        assert_eq!((s.phase, s.busy, s.session_id), (TerminalPhase::Exited, false, None));
        assert!(p.settled());
        // The killed session is gone at the host too, so going away detaches nothing.
        p.set_open(false);
        p.set_open(true);
        assert!(!p.should_start(), "coming back into view is not a request for a shell");
        assert_eq!(p.status_label(), "已退出");

        let effects = p.start_session();
        assert!(matches!(effects.as_slice(), [Effect::Reset, Effect::Open { previous: None, .. }]));
        p.opened(p.attempt(), Ok(response("session-2")));
        let s = p.state();
        assert_eq!((s.phase, s.session_id.as_deref()), (TerminalPhase::Running, Some("session-2")));
    }

    #[test]
    fn reports_a_refused_close_in_place_and_keeps_the_session() {
        let (mut p, attempt) = running();
        p.close_session();
        p.closed(attempt, Err("终端仍在被 Git 操作使用".into()));
        assert_eq!(p.status_label(), "终端仍在被 Git 操作使用");
        assert!(p.settled());
        assert!(!p.should_start(), "no reconnect on its own");
        // The shell is still up at the host, so the panel keeps its id, and the retry releases it before reattaching.
        assert_eq!(p.state().session_id.as_deref(), Some("session-1"));
        let effects = p.start_session();
        assert!(matches!(&effects[1], Effect::Open { previous: Some(s), .. } if s == "session-1"));
    }

    #[test]
    fn a_close_waits_for_the_open_it_races_so_the_host_has_a_session_to_kill() {
        let mut p = panel();
        p.start_session();
        let attempt = p.attempt();
        assert!(p.close_session().is_empty(), "the host has nothing to kill yet");
        assert_eq!(p.status_label(), "正在终止");
        let effects = p.opened(attempt, Ok(response("session-1")));
        assert_eq!(effects, [Effect::Close { attempt }]);
        assert_eq!(p.status_label(), "正在终止", "the phase stays shut");
        assert!(!p.input_accepted());
        assert!(p.on_data("dir\r".into(), || panic!("nothing is typed into a shell being killed")).is_empty());
        // The host's one-shot ready handshake arriving now must not reopen the gate either.
        p.on_event(attempt, TerminalEvent::Ready { session_id: "session-1".into() });
        assert!(!p.input_accepted());
        assert_eq!(p.status_label(), "正在终止");
        p.closed(attempt, Ok(()));
        assert_eq!(p.status_label(), "已退出");
    }

    #[test]
    fn keeps_the_exit_code_a_shell_reports_while_it_is_being_closed() {
        let (mut p, attempt) = running();
        p.close_session();
        // The user had typed `exit` just before: the shell's own exit lands first.
        assert!(p.on_event(attempt, TerminalEvent::Exit { session_id: "session-1".into(), exit_code: Some(0) }).is_empty());
        assert_eq!(p.status_label(), "正在终止");
        assert!(!p.settled());
        p.closed(attempt, Ok(()));
        assert_eq!(p.status_label(), "已退出（代码 0）");
        assert!(p.settled());
    }

    #[test]
    fn dismisses_a_session_that_already_exited_without_erasing_its_verdict() {
        let (mut p, attempt) = running();
        p.on_event(attempt, TerminalEvent::Exit { session_id: "session-1".into(), exit_code: Some(0) });
        assert_eq!(p.close_session(), [Effect::Close { attempt }]);
        p.closed(attempt, Ok(()));
        assert_eq!(p.status_label(), "已退出（代码 0）");
    }

    #[test]
    fn reports_a_clean_exit_once_and_stays_put_on_a_failing_one() {
        let (mut p, attempt) = running();
        assert_eq!(p.on_event(attempt, TerminalEvent::Exit { session_id: "session-1".into(), exit_code: Some(1) }), []);
        assert_eq!(p.status_label(), "已退出（代码 1）");
    }

    #[test]
    fn reports_the_terminal_it_leaves_behind_as_no_longer_observed() {
        let (mut p, attempt) = running();
        p.on_event(attempt, TerminalEvent::CommandState { session_id: "session-1".into(), command_state: state(1, Running, 1) });
        assert_eq!(p.unmount(), [Effect::Detach { session: "session-1".into() }]);
        let s = p.state();
        assert_eq!((s.phase, s.busy, s.has_history), (TerminalPhase::Idle, false, true));
        assert!(p.on_event(attempt, output("session-1", b"late")).is_empty(), "a stopped attempt hears nothing");
    }

    #[test]
    fn derives_command_history_and_busy_state_only_from_authoritative_lifecycle_events() {
        let (mut p, attempt) = running();
        p.on_data("Get-Location\r".into(), || true);
        assert_eq!(p.status_label(), "运行中");
        assert!(p.busy && !p.has_history);
        p.on_event(attempt, TerminalEvent::CommandState { session_id: "session-1".into(), command_state: state(1, Running, 1) });
        assert!(p.busy && p.has_history);
        // A prompt in the output means nothing.
        p.on_event(attempt, output("session-1", b"\r\nPS C:\\workspace> "));
        assert_eq!(p.status_label(), "运行中");
        // A report older than what is known means nothing.
        p.on_event(attempt, TerminalEvent::CommandState { session_id: "session-1".into(), command_state: state(0, Idle, 0) });
        assert_eq!(p.status_label(), "运行中");
        p.on_event(attempt, TerminalEvent::CommandState { session_id: "session-1".into(), command_state: state(2, Idle, 1) });
        assert_eq!(p.status_label(), "待机");
        assert!(!p.busy && p.has_history);
    }

    #[test]
    fn keeps_an_opening_failure_passive_until_the_user_retries_it() {
        let mut p = panel();
        p.start_session();
        p.opened(p.attempt(), Err("shell 启动失败".into()));
        assert_eq!(p.status_label(), "shell 启动失败");
        assert!(!p.should_start(), "a failed open must not loop");
        p.start_session();
        p.opened(p.attempt(), Ok(response("session-1")));
        assert_eq!(p.status_label(), "空终端");
    }

    #[test]
    fn a_write_that_fails_ends_only_the_session_it_was_for() {
        let (mut p, _) = running();
        p.io_failed("another", "no".into());
        assert_eq!(p.phase, TerminalPhase::Running);
        p.io_failed("session-1", "broken pipe".into());
        assert_eq!(p.status_label(), "broken pipe");
    }

    #[test]
    fn nothing_goes_to_a_shell_that_is_gone_and_a_resize_waits_to_be_seen() {
        let (mut p, attempt) = running();
        p.set_open(false);
        assert!(p.on_resize(100, 30).is_empty(), "a hidden panel does not resize its shell");
        p.set_open(true);
        p.on_event(attempt, TerminalEvent::Exit { session_id: "session-1".into(), exit_code: Some(1) });
        assert!(p.on_data("x".into(), || true).is_empty());
        assert!(p.on_resize(100, 30).is_empty());
    }

    #[test]
    fn the_panel_id_is_what_the_tab_points_at() {
        assert_eq!(terminal_panel_id("conversation-1", "terminal-1"), "conversation-terminal-conversation-1-terminal-1");
    }
}
