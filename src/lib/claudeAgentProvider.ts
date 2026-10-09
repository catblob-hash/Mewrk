import type { ApiProvider } from "../types";
import { createId } from "./id";

export const CLAUDE_AGENT_PROVIDER_FAMILY = "claude_agent" as const;
export const CLAUDE_AGENT_PROVIDER_NAME = "Claude Agent";
/** Anthropic's terms for driving Claude Code as an agent runtime. */
export const CLAUDE_AGENT_LEGAL_URL = "https://code.claude.com/docs/en/legal-and-compliance";

/**
 * The Claude Agent family drives Mewrk's bundled, version-locked Claude Code
 * executable through the official Claude Agent SDK instead of speaking HTTP itself,
 * and it reuses that CLI's own login: it has neither an API key nor a base URL.
 * Like Codex it is a built-in row identified by family, not by a fixed id: ids stay
 * random UUIDs so credentials never collide across data domains.
 */
export function isClaudeAgentProvider(provider: Pick<ApiProvider, "family">): boolean {
  return provider.family === CLAUDE_AGENT_PROVIDER_FAMILY;
}

/**
 * Ensure exactly one Claude Agent row: keep the first existing one (and drop
 * later duplicates), or append a fresh disabled row. Existing row order is
 * retained. A `baseUrl` and `familySettings.claude_executable` left over from
 * older versions are flattened rather than rejected — the host ignores both.
 */
export function ensureClaudeAgentProvider(providers: ApiProvider[]): ApiProvider[] {
  let found = false;
  const deduplicated = providers.filter((provider) => {
    if (!isClaudeAgentProvider(provider)) {
      return true;
    }
    if (found) {
      return false;
    }
    found = true;
    return true;
  }).map((provider) => {
    if (!isClaudeAgentProvider(provider)) return provider;
    const hasLegacyExecutable = Object.prototype.hasOwnProperty.call(
      provider.familySettings,
      "claude_executable"
    );
    if (provider.baseUrl === "" && !hasLegacyExecutable) return provider;
    const familySettings = hasLegacyExecutable
      ? Object.fromEntries(
        Object.entries(provider.familySettings).filter(([key]) => key !== "claude_executable")
      )
      : provider.familySettings;
    return {
      ...provider,
      baseUrl: "",
      familySettings,
    };
  });

  if (found) {
    return deduplicated;
  }

  return [
    ...deduplicated,
    {
      id: createId("provider"),
      name: CLAUDE_AGENT_PROVIDER_NAME,
      enabled: false,
      family: CLAUDE_AGENT_PROVIDER_FAMILY,
      baseUrl: "",
      familySettings: {},
      notes: "",
      models: [],
      activeModelId: null,
    },
  ];
}
