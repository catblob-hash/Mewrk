import type { HostMessageContainer } from "../types";

/**
 * The container a conversation's (or a preset's) host messages come in, reading
 * an absent or unknown value as `"user"` — the host's own default
 * (`HostMessageContainer::User`).
 */
export function hostMessageContainerOf(
  settings: { hostMessageContainer?: HostMessageContainer | null } | null | undefined
): HostMessageContainer {
  return settings?.hostMessageContainer === "box" ? "box" : "user";
}
