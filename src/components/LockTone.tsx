import { DatabaseZap, Lock } from "lucide-react";
import type { ReactNode } from "react";
import { useI18n, type TranslationFunction } from "../i18n";
import type { LockTone } from "../lib/toolLock";
import { Switch } from "./Common";

/**
 * What a toned row says about itself (`lockTone` in `toolLock.ts`).
 *
 * Orange is a warning and the row still moves: the last request's cache is
 * warm, and moving the row throws it away.
 */
export interface LockHints {
  cache: string;
}

export function lockHints(t: TranslationFunction): LockHints {
  return {
    cache: t(
      "缓存还热：改动这一项会让它失效。",
      "The cache is still warm: changing this throws it away."
    )
  };
}

/** The class a row carries for its tone, with the leading space, or nothing. */
export function lockToneClass(base: string, tone: LockTone | null | undefined): string {
  return tone ? ` ${base}--${tone}` : "";
}

/**
 * What the conversation says about a web backend selector, and the note said
 * under it.
 *
 * - `cache`: orange (`backendTone` in `toolLock.ts`). The selector moves
 *   through the caller's cache-break warning.
 * - `settled`: a native backend a run has already used (`backendPinned`). That
 *   is a fact about the transcript rather than a lock, so the selector is
 *   simply held, with the reason as its hint.
 */
export interface BackendLock {
  kind: "cache" | "settled";
  note: string;
}

/** The tone a backend selector is drawn in: only a cache lock has one. */
export function backendLockTone(lock: BackendLock | null | undefined): LockTone | null {
  return lock?.kind === "cache" ? "cache" : null;
}

/**
 * A selector field's hint under the lock: a settled selector's reason replaces
 * the standing advice, since it is the one thing left to say about a field that
 * cannot move; an orange note is said above it, since the field still moves and
 * the advice still applies.
 */
export function lockedFieldHint(lock: BackendLock | null | undefined, standing: string): ReactNode {
  if (!lock) return standing;
  if (lock.kind === "settled") return lock.note;
  return <><span className="lock-note lock-note--cache">{lock.note}</span>{standing}</>;
}

/**
 * The composer's model menu marks each model whose prompt cache for this
 * conversation is still warm (`modelCacheWarmUntil` in `toolLock.ts`), in the
 * cache tone, with the moment it runs out on hover.
 */
export function ModelCacheMark({ until }: { until: number }) {
  const { t, resolvedLanguage } = useI18n();
  const time = new Intl.DateTimeFormat(resolvedLanguage, { hour: "2-digit", minute: "2-digit" })
    .format(new Date(until));
  const label = t("提示缓存有效，{time} 过期", "Prompt cache warm until {time}", { time });
  return (
    <span className="model-cache-mark" role="img" aria-label={label} title={label}>
      <DatabaseZap size={12} aria-hidden="true" />
    </span>
  );
}

/** The trailing lock a toned row carries, in the tone's color. */
export function LockMark({ tone, className }: { tone: LockTone | null | undefined; className?: string }) {
  if (!tone) return null;
  return <Lock className={`lock-mark lock-mark--${tone}${className ? ` ${className}` : ""}`} size={13} aria-hidden="true" />;
}

/**
 * A switch row the conversation's lock may tone: a title, a line saying what the
 * switch does, and the note for its tone below that. An orange row moves through
 * the caller's cache-break warning, which the caller owns — every page shares
 * one. A row held for a reason of its own (`disabled`) says that reason instead.
 */
export function LockableSwitchRow({
  title,
  description,
  checked,
  onChange,
  label,
  tone = null,
  hints,
  disabled = false,
  disabledNote
}: {
  title: string;
  description: ReactNode;
  checked: boolean;
  onChange: (checked: boolean) => void;
  /** The switch's accessible name, which says its state. */
  label: string;
  tone?: LockTone | null;
  hints?: LockHints;
  /** Off for a reason that is not the lock's; `disabledNote` says which. */
  disabled?: boolean;
  disabledNote?: string;
}) {
  const shownTone = disabled ? null : tone;
  const note = disabled ? disabledNote : shownTone && hints?.[shownTone];
  return (
    <div className={`tool-toggle-row${disabled ? " tool-toggle-row--disabled" : lockToneClass("tool-toggle-row", shownTone)}`}>
      <span>
        <strong>{title}</strong>
        <small>{description}</small>
        {note && <small className={`lock-note${lockToneClass("lock-note", shownTone)}`}>{note}</small>}
      </span>
      <LockMark tone={shownTone} />
      <Switch
        checked={checked}
        disabled={disabled}
        tone={shownTone ?? undefined}
        onChange={onChange}
        label={label}
      />
    </div>
  );
}
