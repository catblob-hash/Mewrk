import type { TranslationFunction } from "../i18n";
import type {
  AutoCompactSettings,
  CompactionMethod,
  ModelCapability,
  NativeCompaction,
  NativeCompactSettings,
  ProviderFamily
} from "../types";
import { formatCompactTokenCount } from "./contextTokens";
import { appendsTools, takesNativeCompaction } from "./modelCapabilities";

/**
 * The composer's auto-compact settings. The host owns the behaviour — the
 * handoff (`src-tauri/src/handoff.rs`): past the threshold the conversation is
 * armed to hand off; native compaction (`src-tauri/src/native_compaction.rs`):
 * past its own threshold the provider compacts the context in place. These
 * mirror their ranges and rounding so the menu can say exactly when that happens.
 */
export const AUTO_COMPACT_MIN_PERCENT = 20;
export const AUTO_COMPACT_MAX_PERCENT = 97;
const AUTO_COMPACT_DEFAULT_PERCENT = 80;
/** Native compaction starts at 90%, as Codex compacts (Rust `native_compaction::DEFAULT_THRESHOLD_PERCENT`). */
const NATIVE_DEFAULT_PERCENT = 90;
/** Codex keeps 64,000 tokens of the latest user messages (Rust `DEFAULT_RETAINED_TOKENS`). */
export const NATIVE_RETAINED_DEFAULT_TOKENS = 64_000;
/** Rust `native_compaction::MAX_RETAINED_TOKENS`. */
export const NATIVE_RETAINED_MAX_TOKENS = 128_000;

function defaultNativeCompactSettings(): NativeCompactSettings {
  return {
    thresholdPercent: NATIVE_DEFAULT_PERCENT,
    retainedTokens: NATIVE_RETAINED_DEFAULT_TOKENS
  };
}

export function defaultAutoCompactSettings(): AutoCompactSettings {
  return {
    enabled: true,
    thresholdPercent: AUTO_COMPACT_DEFAULT_PERCENT,
    native: defaultNativeCompactSettings()
  };
}

/** A whole percent inside the range the setting offers. */
export function clampAutoCompactPercent(value: number): number {
  if (!Number.isFinite(value)) return AUTO_COMPACT_DEFAULT_PERCENT;
  return Math.min(AUTO_COMPACT_MAX_PERCENT, Math.max(AUTO_COMPACT_MIN_PERCENT, Math.round(value)));
}

/** A whole token count inside the range the retained budget offers. */
export function clampRetainedTokens(value: number): number {
  if (!Number.isFinite(value)) return NATIVE_RETAINED_DEFAULT_TOKENS;
  return Math.min(NATIVE_RETAINED_MAX_TOKENS, Math.max(0, Math.round(value)));
}

function record(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {};
}

function normalizeNativeCompactSettings(value: unknown, fallback: NativeCompactSettings): NativeCompactSettings {
  const input = record(value);
  return {
    thresholdPercent: typeof input.thresholdPercent === "number"
      ? clampAutoCompactPercent(input.thresholdPercent)
      : fallback.thresholdPercent,
    retainedTokens: typeof input.retainedTokens === "number"
      ? clampRetainedTokens(input.retainedTokens)
      : fallback.retainedTokens
  };
}

export function normalizeAutoCompactSettings(value: unknown, fallback: AutoCompactSettings): AutoCompactSettings {
  const input = record(value);
  return {
    enabled: typeof input.enabled === "boolean" ? input.enabled : fallback.enabled,
    thresholdPercent: typeof input.thresholdPercent === "number"
      ? clampAutoCompactPercent(input.thresholdPercent)
      : fallback.thresholdPercent,
    native: normalizeNativeCompactSettings(input.native, fallback.native ?? defaultNativeCompactSettings())
  };
}

/** Tokens at which a conversation is armed to hand off, or compacts: the percent of the window, rounded down. */
export function autoCompactThresholdTokens(contextWindow: number, percent: number): number {
  return Math.floor((contextWindow * clampAutoCompactPercent(percent)) / 100);
}

/**
 * What a native compaction keeps its user messages within: the setting as the
 * user set it, within what the setting offers. Mirrors Rust
 * `native_compaction::retained_budget`.
 */
export function nativeRetainedBudget(retainedTokens: number): number {
  return clampRetainedTokens(retainedTokens);
}

type CompactionModel = {
  provider: { family: ProviderFamily };
  model: { capabilities?: readonly ModelCapability[] };
};

/**
 * The method a new conversation chooses: native where its model compacts
 * natively, the handoff elsewhere.
 */
export function defaultCompactionMethod(target: CompactionModel | null): CompactionMethod {
  return target && takesNativeCompaction(target.provider, target.model) ? "native" : "handoff";
}

/**
 * The method a conversation auto-compacts by on this model: the one it chose
 * where the model can do it, otherwise the other where the model can do that,
 * and `null` where it can do neither. Native compaction needs the capability;
 * the handoff needs a model that takes its tools mid-conversation. Mirrors Rust
 * `native_compaction::method_in_effect`.
 */
export function compactionMethodInEffect(
  chosen: CompactionMethod,
  target: CompactionModel | null
): CompactionMethod | null {
  if (!target) return null;
  const available = (method: CompactionMethod) => method === "native"
    ? takesNativeCompaction(target.provider, target.model)
    : appendsTools(target.provider, target.model);
  const other: CompactionMethod = chosen === "native" ? "handoff" : "native";
  return [chosen, other].find(available) ?? null;
}

/**
 * What a native compaction's card says, in the timeline and in the history
 * record alike: which model compacted, and from how much context to how much.
 * The card's own text is empty; this is read from its fields and never
 * reaches a model.
 */
export function compactionTitle(
  t: TranslationFunction,
  compaction: Pick<NativeCompaction, "model" | "modelName" | "tokensBefore" | "tokensAfter">
): string {
  return t(
    "由 {model} 压缩，{before} token → {after} token",
    "Compacted by {model}, {before} tokens → {after} tokens",
    {
      model: compaction.modelName?.trim() || compaction.model,
      before: formatCompactTokenCount(compaction.tokensBefore),
      after: formatCompactTokenCount(compaction.tokensAfter)
    }
  );
}
