//! Small conversation-setting readers (lib/hostMessages.ts, environmentPrompt.ts, fileWriteGuards.ts, subagentMessaging.ts).
//!
//! Upstream reads a field that may be absent. Here serde has already filled the defaults into the settings types, so
//! callers hand over the field itself; an `Option` (a lock's recorded value, a partial shape) reads absent the same way.

use crate::model::HostMessageContainer;

/// The container a conversation's (or preset's) host messages come in; absent reads as `User`, the host's default.
pub fn host_message_container_of(value: impl Into<Option<HostMessageContainer>>) -> HostMessageContainer {
    match value.into() {
        Some(HostMessageContainer::Box) => HostMessageContainer::Box,
        _ => HostMessageContainer::User,
    }
}

/// Whether the system prompt opens with the environment section; absent reads as on, the host's default.
pub fn environment_prompt_enabled_of(value: impl Into<Option<bool>>) -> bool {
    value.into() != Some(false)
}

/// Whether the file write guards are on; absent reads as on, the host's default. Only an explicit `false` turns them off.
pub fn file_write_guards_enabled_of(value: impl Into<Option<bool>>) -> bool {
    value.into() != Some(false)
}

/// Whether subagent communication is on; absent reads as off, the opposite of the guards. Only an explicit `true` turns it on.
///
/// On, and with `agent_spawn` enabled, the main agent also gets `send_message` and its children `subagent_update`; the
/// switch alone changes nothing on the wire without `agent_spawn`.
pub fn subagent_messaging_enabled_of(value: impl Into<Option<bool>>) -> bool {
    value.into() == Some(true)
}

/// The tool the subagent-communication switch is about; without it the switch changes nothing.
pub const SUBAGENT_SPAWN_TOOL: &str = "agent_spawn";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_absent_values_as_the_hosts_defaults() {
        assert_eq!(host_message_container_of(None), HostMessageContainer::User);
        assert_eq!(host_message_container_of(HostMessageContainer::Box), HostMessageContainer::Box);
        assert_eq!(host_message_container_of(HostMessageContainer::User), HostMessageContainer::User);
        assert!(environment_prompt_enabled_of(None::<bool>));
        assert!(!environment_prompt_enabled_of(false));
        assert!(file_write_guards_enabled_of(None::<bool>));
        assert!(!file_write_guards_enabled_of(false));
        assert!(!subagent_messaging_enabled_of(None::<bool>));
        assert!(subagent_messaging_enabled_of(true));
    }
}
