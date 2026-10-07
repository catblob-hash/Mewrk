import { useEffect, useRef, useState } from "react";
import type { ChangeEvent, KeyboardEvent, PointerEvent, ReactNode } from "react";
import { createPortal } from "react-dom";
import { ChevronRight } from "lucide-react";
import { useI18n } from "../i18n";
import {
  AUTO_COMPACT_MAX_PERCENT,
  AUTO_COMPACT_MIN_PERCENT,
  NATIVE_RETAINED_MAX_TOKENS,
  autoCompactThresholdTokens,
  clampAutoCompactPercent,
  clampRetainedTokens
} from "../lib/autoCompact";
import { computeContextBreakdown, type ContextSegmentId } from "../lib/contextBreakdown";
import { formatCompactTokenCount } from "../lib/contextTokens";
import type { AutoCompactSettings, CompactionMethod, ContextItem, NativeCompaction } from "../types";
import { Switch } from "./Common";
import { MenuFlyout, MenuSurfacesContext, createMenuSurfaces } from "./MenuFlyout";
import { RollingNumber } from "./RollingNumber";
import { usePopoverAnchor } from "./usePopoverAnchor";

const PANEL_WIDTH = 296;
const RING_RADIUS = 6;
const RING_CIRCUMFERENCE = 2 * Math.PI * RING_RADIUS;
/** Meter color thresholds: warn at 70% and alert at 90%. */
const WARNING_RATIO = 0.7;
const DANGER_RATIO = 0.9;

export interface ContextUsageCounts {
  tools: number;
  mcpServers: number;
  skills: number;
  agentRoles: number;
}

export interface ContextUsageMeterProps {
  contexts: ContextItem[];
  tokens: number;
  /** The meter includes locally estimated components, indicated by `~` in its heading. */
  estimated: boolean;
  /** The current model cannot project context usage. */
  unprojectable?: boolean;
  contextWindow: number | null;
  counts: ContextUsageCounts;
  /** The global auto-compact setting. Without it (and its handler) the panel has no auto-compact row. */
  autoCompact?: AutoCompactSettings;
  onAutoCompactChange?: (next: AutoCompactSettings) => void;
  /**
   * The selected model cannot take a tool mid-conversation, so the handoff tools
   * could never join a run and it never hands off. The setting is global and
   * stays as it is; the submenu says it does not apply here.
   */
  autoCompactUnavailable?: boolean;
  /**
   * The selected model compacts natively, so the submenu offers native
   * compaction beside the handoff.
   */
  nativeCompactionAvailable?: boolean;
  /**
   * The method this conversation auto-compacts by on the selected model
   * (`compactionMethodInEffect`), `null` where it can do neither. Without it
   * the conversation hands off where it can.
   */
  compactionMethod?: CompactionMethod | null;
  /** Records the conversation's choice of method. */
  onCompactionMethodChange?: (method: CompactionMethod) => void;
  /**
   * The native compaction the request on this model starts at (`wireView`):
   * `contexts` is the timeline from its card on, and the breakdown counts its
   * item as compacted history.
   */
  compaction?: NativeCompaction | null;
  /**
   * Compacts the conversation natively at once, into a new conversation
   * (`native_compaction.rs`). Without it the native page has no such button.
   */
  onCompactNow?: () => void;
  /** Why compacting now is out of reach at the moment, or nothing when it is not. */
  compactNowBlocked?: string | null;
}

/**
 * One threshold: a slider with a small number field above it that says the
 * same value exactly, and a line under it that says what it means.
 *
 * Neither control saves while it is being worked: the slider commits when it is
 * released (or on the keyboard's native `change`), the field on Enter or blur,
 * so dragging across the range does not write the document once per step.
 */
function PercentControl({
  label,
  fieldLabel,
  value,
  disabled,
  onCommit,
  hint
}: {
  label: string;
  fieldLabel: string;
  value: number;
  disabled: boolean;
  onCommit: (percent: number) => void;
  /** What the percent being shown means, live while it is dragged. */
  hint: (percent: number) => ReactNode;
}) {
  const [draftPercent, setDraftPercent] = useState(value);
  const [fieldDraft, setFieldDraft] = useState<string | null>(null);
  const committedRef = useRef(value);

  useEffect(() => {
    setDraftPercent(value);
    committedRef.current = value;
  }, [value]);

  const commitPercent = (next: number): void => {
    const clamped = clampAutoCompactPercent(next);
    setDraftPercent(clamped);
    setFieldDraft(null);
    if (clamped === committedRef.current) return;
    committedRef.current = clamped;
    onCommit(clamped);
  };

  const handleSliderChange = (event: ChangeEvent<HTMLInputElement>): void => {
    const next = clampAutoCompactPercent(Number(event.target.value));
    setDraftPercent(next);
    setFieldDraft(null);
    // React reports every step of a drag as onChange; only the native change
    // (keyboard, or a release the browser reports) commits here.
    if (event.nativeEvent.type === "change") commitPercent(next);
  };

  const handleFieldChange = (event: ChangeEvent<HTMLInputElement>): void => {
    const text = event.target.value;
    setFieldDraft(text);
    const parsed = Number(text);
    // A partial entry ("8" on the way to "85") only moves the slider when it
    // already names a value in range; nothing is saved until Enter or blur.
    if (text.trim() !== "" && Number.isInteger(parsed)
      && parsed >= AUTO_COMPACT_MIN_PERCENT && parsed <= AUTO_COMPACT_MAX_PERCENT) {
      setDraftPercent(parsed);
    }
  };

  const commitField = (): void => {
    if (fieldDraft === null) return;
    const parsed = Number(fieldDraft);
    if (fieldDraft.trim() === "" || !Number.isFinite(parsed)) {
      setFieldDraft(null);
      setDraftPercent(committedRef.current);
      return;
    }
    commitPercent(parsed);
  };

  const handleFieldKeyDown = (event: KeyboardEvent<HTMLInputElement>): void => {
    if (event.key === "Enter") {
      event.preventDefault();
      commitField();
    }
  };

  return (
    <div className={`context-usage-compact__threshold${disabled ? " context-usage-compact__threshold--disabled" : ""}`}>
      <div className="context-usage-compact__threshold-head">
        <span>{label}</span>
        <span className="context-usage-compact__field">
          <input
            type="number"
            inputMode="numeric"
            aria-label={fieldLabel}
            min={AUTO_COMPACT_MIN_PERCENT}
            max={AUTO_COMPACT_MAX_PERCENT}
            step={1}
            disabled={disabled}
            value={fieldDraft ?? String(draftPercent)}
            onChange={handleFieldChange}
            onKeyDown={handleFieldKeyDown}
            onBlur={commitField}
          />
          <span aria-hidden="true">%</span>
        </span>
      </div>
      <input
        type="range"
        className="context-usage-compact__slider"
        aria-label={label}
        min={AUTO_COMPACT_MIN_PERCENT}
        max={AUTO_COMPACT_MAX_PERCENT}
        step={1}
        disabled={disabled}
        value={draftPercent}
        onInput={(event) => {
          setFieldDraft(null);
          setDraftPercent(clampAutoCompactPercent(Number(event.currentTarget.value)));
        }}
        onChange={handleSliderChange}
        onPointerUp={(event: PointerEvent<HTMLInputElement>) => commitPercent(Number(event.currentTarget.value))}
      />
      <div className="context-usage-compact__scale" aria-hidden="true">
        <span>{AUTO_COMPACT_MIN_PERCENT}%</span>
        <span>{AUTO_COMPACT_MAX_PERCENT}%</span>
      </div>
      <p className="context-usage-compact__hint">{hint(draftPercent)}</p>
    </div>
  );
}

/**
 * How much of the latest user messages a native compaction keeps, in tokens,
 * as a sentence with the number in it. Saved on Enter or blur, as the
 * threshold field is.
 */
function RetainedControl({
  value,
  onCommit
}: {
  value: number;
  onCommit: (tokens: number) => void;
}) {
  const { t } = useI18n();
  const [fieldDraft, setFieldDraft] = useState<string | null>(null);
  const commit = (): void => {
    if (fieldDraft === null) return;
    const parsed = Number(fieldDraft);
    setFieldDraft(null);
    if (fieldDraft.trim() === "" || !Number.isFinite(parsed)) return;
    const tokens = clampRetainedTokens(parsed);
    if (tokens !== value) onCommit(tokens);
  };
  const field = (
    <input
      type="number"
      inputMode="numeric"
      aria-label={t("保留最近的用户消息（token）", "Latest user messages kept (tokens)")}
      min={0}
      max={NATIVE_RETAINED_MAX_TOKENS}
      step={1000}
      value={fieldDraft ?? String(value)}
      onChange={(event) => setFieldDraft(event.target.value)}
      onKeyDown={(event) => {
        if (event.key === "Enter") {
          event.preventDefault();
          commit();
        }
      }}
      onBlur={commit}
    />
  );
  return (
    <div className="context-usage-compact__retained">
      <span>{t("保留最近的", "Keep the latest")}</span>
      <span className="context-usage-compact__field context-usage-compact__field--tokens">{field}</span>
      <span>{t("token 用户消息", "tokens of user messages")}</span>
    </div>
  );
}

/**
 * The auto-compact submenu. The switch for auto-compact heads it; under it, at
 * the left, the two methods a conversation chooses between — the handoff and
 * native compaction — as two buttons. The one pressed is this conversation's
 * choice, and the page under them is that method's: its threshold, and for
 * native compaction how much of the latest user messages it keeps and a
 * button that compacts at once. The choice is the conversation's; the switch
 * and the numbers are global.
 *
 * It opens beside its row as `PopoverMenu`'s submenus do, a panel of its own
 * (`MenuFlyout`), and hangs upward from its row, which sits at the foot of a
 * panel that usually opens above the composer. The panel counts a press in it as
 * a press in the panel.
 */
function AutoCompactFlyout({
  settings,
  contextWindow,
  handoffAvailable,
  nativeAvailable,
  method,
  onChange,
  onMethodChange,
  onCompactNow,
  compactNowBlocked
}: {
  settings: AutoCompactSettings;
  contextWindow: number | null;
  handoffAvailable: boolean;
  nativeAvailable: boolean;
  /** The method this conversation compacts by on this model. */
  method: CompactionMethod | null;
  onChange: (next: AutoCompactSettings) => void;
  onMethodChange?: (method: CompactionMethod) => void;
  onCompactNow?: () => void;
  compactNowBlocked?: string | null;
}) {
  const { t } = useI18n();
  const hasWindow = contextWindow !== null && contextWindow > 0;
  const noWindow = t(
    "当前模型没有设置上下文窗口，无法自动压缩",
    "The current model has no context window set, so it cannot auto-compact"
  );
  const native = settings.native;
  const methodTab = (value: CompactionMethod, label: string, available: boolean, unavailable: string) => (
    <button
      type="button"
      role="tab"
      aria-selected={method === value}
      className={method === value ? "is-active" : ""}
      disabled={!available || !onMethodChange}
      title={available ? undefined : unavailable}
      onClick={() => {
        if (method !== value) onMethodChange?.(value);
      }}
    >
      {label}
    </button>
  );

  return (
    <MenuFlyout
      role="group"
      aria-label={t("自动压缩", "Auto-compact")}
      className="context-usage-compact__flyout"
      hang="up"
    >
      <div className="context-usage-compact__switch">
        <span>{t("启用自动压缩", "Enable auto-compact")}</span>
        <Switch
          checked={settings.enabled}
          label={t("启用自动压缩", "Enable auto-compact")}
          onChange={(enabled) => onChange({ ...settings, enabled })}
        />
      </div>
      <div className="context-usage-compact__tabs" role="tablist" aria-label={t("压缩方式", "Compaction method")}>
        {methodTab(
          "handoff",
          t("交接", "Handoff"),
          handoffAvailable,
          t(
            "当前模型不支持中途追加工具，交接用的工具无法在对话中途加入。",
            "The selected model cannot take a tool mid-conversation, so the handoff tools could never join."
          )
        )}
        {methodTab(
          "native",
          t("原生压缩", "Native compaction"),
          nativeAvailable,
          t("当前模型不支持原生压缩。", "The selected model does not compact natively.")
        )}
      </div>
      {method === "handoff" && (
        <section className="context-usage-compact__page" role="tabpanel" aria-label={t("交接", "Handoff")}>
          <PercentControl
            label={t("交接阈值", "Handoff threshold")}
            fieldLabel={t("交接阈值（百分比）", "Handoff threshold (percent)")}
            value={settings.thresholdPercent}
            disabled={!settings.enabled}
            onCommit={(thresholdPercent) => onChange({ ...settings, thresholdPercent })}
            hint={(percent) => hasWindow
              ? t(
                "上下文达到 {tokens} tokens 时，模型写好交接文档，在新的交接会话中继续",
                "At {tokens} tokens of context, the model writes handoff notes and continues in a new handover conversation",
                { tokens: formatCompactTokenCount(autoCompactThresholdTokens(contextWindow, percent)) }
              )
              : noWindow}
          />
        </section>
      )}
      {method === "native" && (
        <section className="context-usage-compact__page" role="tabpanel" aria-label={t("原生压缩", "Native compaction")}>
          <PercentControl
            label={t("原生压缩阈值", "Native compaction threshold")}
            fieldLabel={t("原生压缩阈值（百分比）", "Native compaction threshold (percent)")}
            value={native.thresholdPercent}
            disabled={!settings.enabled}
            onCommit={(thresholdPercent) => onChange({ ...settings, native: { ...native, thresholdPercent } })}
            hint={(percent) => hasWindow
              ? t(
                "上下文达到 {tokens} tokens 时，模型把上下文原生压缩成一个压缩项，在新会话中继续",
                "At {tokens} tokens of context, the model compacts it natively into one item and continues in a new conversation",
                { tokens: formatCompactTokenCount(autoCompactThresholdTokens(contextWindow, percent)) }
              )
              : noWindow}
          />
          <RetainedControl
            value={native.retainedTokens}
            onCommit={(retainedTokens) => onChange({ ...settings, native: { ...native, retainedTokens } })}
          />
          {onCompactNow && (
            <button
              type="button"
              className="button button--secondary context-usage-compact__now"
              disabled={Boolean(compactNowBlocked)}
              title={compactNowBlocked ?? undefined}
              onClick={onCompactNow}
            >
              {t("立即压缩", "Compact now")}
            </button>
          )}
        </section>
      )}
      <p className="context-usage-compact__note">
        {t(
          "方式按对话各自记；开关、阈值和保留量对所有对话生效。",
          "The method is this conversation's own; the switch, thresholds and budget apply to every conversation."
        )}
      </p>
    </MenuFlyout>
  );
}

/**
 * Context meter at the composer's lower right. The ring opens a breakdown.
 *
 * The ring keeps its fixed footprint without showing unstable numeric text.
 * Its `aria-label` and `title` expose the current value.
 */
export function ContextUsageMeter({
  contexts,
  tokens,
  estimated,
  unprojectable = false,
  contextWindow,
  counts,
  autoCompact,
  onAutoCompactChange,
  autoCompactUnavailable = false,
  nativeCompactionAvailable = false,
  compactionMethod,
  onCompactionMethodChange,
  compaction = null,
  onCompactNow,
  compactNowBlocked = null
}: ContextUsageMeterProps) {
  const { t } = useI18n();
  const [surfaces] = useState(createMenuSurfaces);
  const { open, position, triggerRef, panelRef, toggle, close } = usePopoverAnchor({
    align: "end",
    width: PANEL_WIDTH,
    // The auto-compact submenu is a panel of its own on the page, not inside this one.
    keepOpenOnPress: surfaces.contains
  });
  const [compactMenuOpen, setCompactMenuOpen] = useState(false);
  // A reopened panel starts with its submenu closed, as `PopoverMenu` does.
  useEffect(() => {
    if (!open) setCompactMenuOpen(false);
  }, [open]);

  // The dialog must receive focus so its action remains reachable by keyboard.
  // Before positioning it is hidden and unfocusable; `preventScroll` avoids
  // triggering the hook's scroll listener, which would immediately close it.
  const positioned = position !== null;
  useEffect(() => {
    if (!open || !positioned) return;
    panelRef.current?.focus({ preventScroll: true });
  }, [open, positioned, panelRef]);

  const breakdown = computeContextBreakdown({
    contexts,
    compaction,
    used: tokens,
    window: unprojectable ? null : contextWindow
  });
  const ratio = unprojectable ? null : breakdown.ratio;
  const tone = ratio === null
    ? ""
    : ratio >= DANGER_RATIO
      ? " context-usage-meter__trigger--danger"
      : ratio >= WARNING_RATIO
        ? " context-usage-meter__trigger--warning"
        : "";

  const prefix = estimated ? "~" : "";
  const headline = unprojectable
    ? t("不可投影", "Not projectable")
    : breakdown.window === null
      ? `${prefix}${formatCompactTokenCount(breakdown.used)}`
      : t(
        "{used} / {window}（{percent}%）",
        "{used} / {window} ({percent}%)",
        {
          used: `${prefix}${formatCompactTokenCount(breakdown.used)}`,
          window: formatCompactTokenCount(breakdown.window),
          percent: Math.round((ratio ?? 0) * 100)
        }
      );

  const segmentLabel = (id: ContextSegmentId): string => {
    if (id === "systemPrompt") return t("系统提示词", "System prompt");
    if (id === "compacted") return t("压缩的历史", "Compacted history");
    if (id === "user") return t("用户消息", "User messages");
    if (id === "assistant") return t("助手回复", "Assistant replies");
    if (id === "reasoning") return t("思考", "Reasoning");
    if (id === "tool") return t("工具调用", "Tool calls");
    return t("其他（工具定义等）", "Other (tool schemas, etc.)");
  };
  // Distinguish zero from a nonzero share too small to display at one decimal.
  const formatShare = (share: number): string => (
    share > 0 && share < 0.001 ? "<0.1%" : `${(share * 100).toFixed(1)}%`
  );

  const countItems: { id: string; label: string; value: number }[] = [
    { id: "tools", label: t("工具", "Tools"), value: counts.tools },
    { id: "mcp", label: t("MCP", "MCP"), value: counts.mcpServers },
    { id: "skills", label: t("技能", "Skills"), value: counts.skills },
    { id: "roles", label: t("角色", "Roles"), value: counts.agentRoles }
  ];

  // The row is out of reach only when neither method applies to the model.
  const compactUnavailable = autoCompactUnavailable && !nativeCompactionAvailable;
  // The method in effect: the caller's word, or the handoff where it can.
  const method: CompactionMethod | null = compactionMethod !== undefined
    ? compactionMethod
    : compactUnavailable
      ? null
      : autoCompactUnavailable ? "native" : "handoff";
  const compactHint = ((): string => {
    if (!autoCompact) return "";
    if (compactUnavailable || method === null) return t("当前模型不支持", "Not for this model");
    if (!autoCompact.enabled) return t("已关闭", "Off");
    if (method === "native") {
      return t("原生 {percent}%", "Native {percent}%", {
        percent: clampAutoCompactPercent(autoCompact.native.thresholdPercent)
      });
    }
    const percent = `${clampAutoCompactPercent(autoCompact.thresholdPercent)}%`;
    // Where there is nothing to choose between, the percent says it all.
    return nativeCompactionAvailable ? t("交接 {percent}", "Handoff {percent}", { percent }) : percent;
  })();

  const triggerLabel = unprojectable
    ? t("上下文用量：不可投影", "Context usage: not projectable")
    : t("上下文用量：{headline}", "Context usage: {headline}", { headline });

  return (
    <div className="context-usage-meter">
      <button
        ref={triggerRef}
        type="button"
        data-drag-exclude
        className={`context-usage-meter__trigger${tone}${open ? " context-usage-meter__trigger--open" : ""}`}
        aria-label={triggerLabel}
        title={triggerLabel}
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={toggle}
      >
        <svg
          className="context-usage-meter__ring"
          width="16"
          height="16"
          viewBox="0 0 16 16"
          aria-hidden="true"
          focusable="false"
        >
          <circle className="context-usage-meter__track" cx="8" cy="8" r={RING_RADIUS} />
          {ratio !== null && ratio > 0 && (
            <circle
              className="context-usage-meter__fill"
              cx="8"
              cy="8"
              r={RING_RADIUS}
              strokeDasharray={`${RING_CIRCUMFERENCE * ratio} ${RING_CIRCUMFERENCE}`}
            />
          )}
        </svg>
      </button>
      {open && createPortal(
        <div
          ref={panelRef}
          tabIndex={-1}
          className={`popover-menu__panel context-usage-panel${position?.flipped ? " popover-menu__panel--flipped" : ""}`}
          role="dialog"
          aria-label={t("上下文窗口用量", "Context window usage")}
          style={{
            left: position?.left ?? 0,
            top: position?.top ?? 0,
            width: PANEL_WIDTH,
            zIndex: position?.layer,
            visibility: position ? "visible" : "hidden"
          }}
        >
          <div className="context-usage-panel__head">
            <span>{t("上下文窗口", "Context window")}</span>
            <strong><RollingNumber value={headline} /></strong>
          </div>
          <div className="context-usage-panel__bar" aria-hidden="true">
            {breakdown.segments.map((segment) => (
              <span
                key={segment.id}
                data-segment={segment.id}
                style={{ flexGrow: segment.tokens }}
              />
            ))}
            {breakdown.free !== null && breakdown.free > 0 && (
              <span data-segment="free" style={{ flexGrow: breakdown.free }} />
            )}
            {autoCompact?.enabled && method !== null && breakdown.window !== null && (
              <i
                className={`context-usage-panel__threshold${method === "native" ? " context-usage-panel__threshold--native" : ""}`}
                style={{
                  left: `${clampAutoCompactPercent(method === "native"
                    ? autoCompact.native.thresholdPercent
                    : autoCompact.thresholdPercent)}%`
                }}
              />
            )}
          </div>
          <ul className="context-usage-panel__rows">
            {breakdown.segments.map((segment) => (
              <li key={segment.id}>
                <span className="context-usage-panel__swatch" data-segment={segment.id} />
                <span className="context-usage-panel__name">{segmentLabel(segment.id)}</span>
                <span className="context-usage-panel__tokens">
                  <RollingNumber value={formatCompactTokenCount(segment.tokens)} />
                </span>
                <span className="context-usage-panel__share">
                  <RollingNumber value={formatShare(segment.share)} />
                </span>
              </li>
            ))}
            {breakdown.free !== null && (
              <li>
                <span className="context-usage-panel__swatch" data-segment="free" />
                <span className="context-usage-panel__name">{t("剩余空间", "Free space")}</span>
                <span className="context-usage-panel__tokens">
                  <RollingNumber value={formatCompactTokenCount(breakdown.free)} />
                </span>
                <span className="context-usage-panel__share">
                  <RollingNumber value={formatShare(breakdown.freeShare ?? 0)} />
                </span>
              </li>
            )}
            {!breakdown.segments.length && breakdown.free === null && (
              <li className="context-usage-panel__empty">
                {t("还没有可拆解的上下文", "Nothing to break down yet")}
              </li>
            )}
          </ul>
          <div className="context-usage-panel__counts">
            {countItems.map((item) => (
              <span key={item.id}>
                {item.label}
                <strong>{item.value}</strong>
              </span>
            ))}
          </div>
          {autoCompact && onAutoCompactChange && (
            <div className="context-usage-compact">
              <button
                type="button"
                className="popover-menu__item context-usage-compact__trigger"
                aria-haspopup="true"
                aria-expanded={compactMenuOpen && !compactUnavailable}
                disabled={compactUnavailable}
                title={compactUnavailable
                  ? t(
                    "当前模型不支持中途追加工具，交接用的工具无法在对话中途加入，所以不会自动压缩。",
                    "The selected model cannot take a tool mid-conversation, so the handoff tools could never join and it does not auto-compact."
                  )
                  : undefined}
                onClick={() => setCompactMenuOpen((current) => !current)}
              >
                <span className="context-usage-compact__label">{t("自动压缩", "Auto-compact")}</span>
                <span className="popover-menu__hint">{compactHint}</span>
                <ChevronRight
                  size={13}
                  aria-hidden="true"
                  className="popover-menu__chevron"
                />
              </button>
              {compactMenuOpen && !compactUnavailable && (
                <MenuSurfacesContext.Provider value={surfaces}>
                  <AutoCompactFlyout
                    settings={autoCompact}
                    contextWindow={unprojectable ? null : contextWindow}
                    handoffAvailable={!autoCompactUnavailable}
                    nativeAvailable={nativeCompactionAvailable}
                    method={method}
                    onChange={onAutoCompactChange}
                    onMethodChange={onCompactionMethodChange}
                    onCompactNow={onCompactNow && (() => {
                      close(false);
                      onCompactNow();
                    })}
                    compactNowBlocked={compactNowBlocked}
                  />
                </MenuSurfacesContext.Provider>
              )}
            </div>
          )}
        </div>,
        document.body
      )}
    </div>
  );
}
