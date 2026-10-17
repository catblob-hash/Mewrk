/**
 * Whether a conversation's (or a preset's) system prompt opens with the
 * environment section — working directory, workspaces, platform, date —
 * reading an absent or unknown value as on, the host's own default
 * (`ConversationSettings::environment_prompt_enabled`). Only an explicit
 * `false` leaves it out.
 */
export function environmentPromptEnabledOf(
  settings: { environmentPromptEnabled?: boolean | null } | null | undefined
): boolean {
  return settings?.environmentPromptEnabled !== false;
}
