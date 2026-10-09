/**
 * Whether a conversation's (or a preset's) file write guards are on, reading an
 * absent or unknown value as on — the host's own default
 * (`ConversationSettings::file_write_guards_enabled`). Only an explicit `false`
 * turns them off.
 */
export function fileWriteGuardsEnabledOf(
  settings: { fileWriteGuardsEnabled?: boolean | null } | null | undefined
): boolean {
  return settings?.fileWriteGuardsEnabled !== false;
}
