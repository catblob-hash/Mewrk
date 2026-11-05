//! The authoritative terminal sessions: upstream's `lib/terminalController.ts`. It only holds terminals the renderer is
//! observing, so a busy flag can never outlive the panel that could clear it. The host is the authority on the rest (its
//! command lease still blocks Git writes).

use std::collections::{HashMap, HashSet};

use crate::api::{TerminalPhase, TerminalSessionState};

/// The store key of one terminal. A terminal id is only unique within its conversation, so the pair is the identity, as
/// it is for the host.
pub fn terminal_session_key(conversation_id: &str, terminal_id: &str) -> String {
    format!("{conversation_id}/{terminal_id}")
}

pub fn terminal_session_is_live(state: &TerminalSessionState) -> bool {
    matches!(state.phase, TerminalPhase::Connecting | TerminalPhase::Running | TerminalPhase::Closing)
}

#[derive(Default)]
pub struct TerminalController {
    sessions: HashMap<String, TerminalSessionState>,
    closing: HashSet<String>,
    /// Bumped on every change. A view reads this to see that something moved, as a subscriber would.
    pub revision: u64,
}

impl TerminalController {
    pub fn current(&self) -> &HashMap<String, TerminalSessionState> {
        &self.sessions
    }

    pub fn session(&self, conversation_id: &str, terminal_id: &str) -> Option<&TerminalSessionState> {
        self.sessions.get(&terminal_session_key(conversation_id, terminal_id))
    }

    fn remove(&mut self, key: &str) {
        if self.sessions.remove(key).is_some() {
            self.revision += 1;
        }
    }

    /// Upserts a live session, doing nothing when every field is unchanged. A report that the session is not live
    /// (idle, exited, failed) removes it instead.
    pub fn update(&mut self, next: TerminalSessionState) {
        let key = terminal_session_key(&next.conversation_id, &next.terminal_id);
        if !terminal_session_is_live(&next) {
            self.remove(&key);
            return;
        }
        if self.sessions.get(&key) == Some(&next) {
            return;
        }
        self.sessions.insert(key, next);
        self.revision += 1;
    }

    pub fn register(&mut self, session: TerminalSessionState) {
        self.sessions.insert(terminal_session_key(&session.conversation_id, &session.terminal_id), session);
        self.revision += 1;
    }

    /// Marks the session busy with history; false for unknown sessions.
    pub fn mark_command_started(&mut self, conversation_id: &str, terminal_id: &str) -> bool {
        let Some(session) = self.sessions.get_mut(&terminal_session_key(conversation_id, terminal_id)) else { return false };
        session.busy = true;
        session.has_history = true;
        self.revision += 1;
        true
    }

    /// Asks to close a terminal, answering whether this call is the one that should go to the host: concurrent calls
    /// for the same terminal share one task.
    pub fn begin_close(&mut self, conversation_id: &str, terminal_id: &str) -> bool {
        self.closing.insert(terminal_session_key(conversation_id, terminal_id))
    }

    /// What the host said to a close. On success the session goes; on failure it stays, and a retry may go out.
    pub fn finish_close(&mut self, conversation_id: &str, terminal_id: &str, ok: bool) {
        let key = terminal_session_key(conversation_id, terminal_id);
        self.closing.remove(&key);
        if ok {
            self.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(terminal_id: &str, phase: TerminalPhase) -> TerminalSessionState {
        TerminalSessionState { terminal_id: terminal_id.into(), conversation_id: "conversation-1".into(), label: "终端".into(), phase, busy: false, has_history: false, cwd: "/w".into(), shell: "zsh".into(), session_id: Some("s".into()) }
    }

    #[test]
    fn revision_moves_on_register_and_update_but_not_on_identical_updates() {
        let mut c = TerminalController::default();
        c.register(session("t", TerminalPhase::Running));
        assert_eq!(c.revision, 1);
        c.update(session("t", TerminalPhase::Running));
        assert_eq!(c.revision, 1);
        c.update(TerminalSessionState { busy: true, ..session("t", TerminalPhase::Running) });
        assert_eq!(c.revision, 2);
    }

    #[test]
    fn upserts_unknown_live_sessions_through_update() {
        let mut c = TerminalController::default();
        c.update(session("t", TerminalPhase::Connecting));
        assert_eq!(c.session("conversation-1", "t").unwrap().phase, TerminalPhase::Connecting);
    }

    #[test]
    fn drops_a_session_the_renderer_no_longer_observes_as_live() {
        for phase in [TerminalPhase::Idle, TerminalPhase::Exited, TerminalPhase::Error] {
            let mut c = TerminalController::default();
            c.update(TerminalSessionState { busy: true, ..session("t", TerminalPhase::Running) });
            c.update(session("t", phase));
            assert!(c.session("conversation-1", "t").is_none(), "{phase:?}");
        }
    }

    #[test]
    fn keeps_the_same_terminal_id_apart_per_conversation() {
        let mut c = TerminalController::default();
        c.update(session("composer", TerminalPhase::Running));
        c.update(TerminalSessionState { conversation_id: "conversation-2".into(), ..session("composer", TerminalPhase::Running) });
        assert_eq!(c.current().len(), 2);
        c.update(session("composer", TerminalPhase::Idle));
        assert_eq!(c.current().len(), 1);
        assert!(c.session("conversation-2", "composer").is_some());
    }

    #[test]
    fn marks_command_start_as_busy_with_history_and_rejects_unknown_terminals() {
        let mut c = TerminalController::default();
        c.register(session("t", TerminalPhase::Running));
        assert!(c.mark_command_started("conversation-1", "t"));
        let s = c.session("conversation-1", "t").unwrap();
        assert!(s.busy && s.has_history);
        assert!(!c.mark_command_started("conversation-1", "nope"));
    }

    #[test]
    fn one_close_goes_out_and_a_failure_keeps_the_session_and_allows_a_retry() {
        let mut c = TerminalController::default();
        c.register(session("t", TerminalPhase::Running));
        assert!(c.begin_close("conversation-1", "t"));
        assert!(!c.begin_close("conversation-1", "t"), "a concurrent close shares the first");
        c.finish_close("conversation-1", "t", false);
        assert!(c.session("conversation-1", "t").is_some());
        assert!(c.begin_close("conversation-1", "t"), "the failure allows a retry");
        c.finish_close("conversation-1", "t", true);
        assert!(c.session("conversation-1", "t").is_none());
    }
}
