/**
 * Whether a conversation's (or a preset's) subagent communication is on,
 * reading an absent or unknown value as off — the host's own default
 * (`ConversationSettings::subagent_messaging_enabled`). Only an explicit `true`
 * turns it on, the opposite of the file write guards.
 *
 * On, and with `agent_spawn` enabled, the main agent also gets `send_message`
 * and the subagents it starts get `subagent_update`; the switch alone changes
 * nothing on the wire without `agent_spawn`.
 */
export function subagentMessagingEnabledOf(
  settings: { subagentMessagingEnabled?: boolean | null } | null | undefined
): boolean {
  return settings?.subagentMessagingEnabled === true;
}

/** The tool the switch is about; without it the switch changes nothing. */
export const SUBAGENT_SPAWN_TOOL = "agent_spawn";
