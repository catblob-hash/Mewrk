import { useI18n } from "../i18n";
import type { TranslationFunction } from "../i18n";
import { resolveShortcut, SHORTCUT_COMMANDS, shortcutKeyCaps } from "../lib/shortcuts";
import { useNow } from "../lib/useNow";
import type { GlobalSettings, KeyToken, ResolvedAppLanguage, ShortcutCommandId } from "../types";
import { MewrkIcon } from "./MewrkIcon";
import { ConversationStatusMark } from "./Sidebar";
import type { ConversationStatus } from "./Sidebar";

/** How many of the project's tasks the page lists; the sidebar and the search hold the rest. */
export const DRAFT_LANDING_TASK_LIMIT = 5;

export interface DraftLandingTask {
  id: string;
  title: string;
  updatedAt: string;
  status: ConversationStatus;
}

interface DraftLandingProps {
  /** The project's tasks, most recently updated first; the page lists the first few. */
  recentTasks: DraftLandingTask[];
  onOpenTask: (conversationId: string) => void;
  /** The user's overrides of the command shortcuts, as the document stores them. */
  shortcuts: GlobalSettings["shortcuts"];
  sendShortcut: readonly KeyToken[];
  newlineShortcut: readonly KeyToken[];
  /** Whether a right-click here inserts a context. A locked timeline does not advertise one. */
  canInsert: boolean;
  /** The moment the tasks' ages are measured from. Only tests pass it. */
  now?: number;
}

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

function startOfDay(time: number): number {
  const date = new Date(time);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

/**
 * How long ago a task was last touched, as coarsely as a glance needs: minutes, hours, days, and
 * past a week the date itself, which is easier to place than "23 days ago".
 *
 * Within a day it counts elapsed time; beyond that it counts midnights, the way "yesterday" is
 * meant — 26 hours before one in the morning is the day before yesterday.
 */
export function taskAgeLabel(
  updatedAt: string,
  now: number,
  t: TranslationFunction,
  language: ResolvedAppLanguage
): string {
  const then = Date.parse(updatedAt);
  if (Number.isNaN(then)) return "";
  // A clock that moved back must not read as a task from the future.
  const elapsed = Math.max(0, now - then);
  if (elapsed < MINUTE) return t("刚刚", "Just now");
  if (elapsed < HOUR) {
    const count = Math.floor(elapsed / MINUTE);
    return t("{count} 分钟前", "{count} min ago", { count });
  }
  // Rounded, because a day that changes the clocks is not 24 hours long — and on the one that
  // runs to 25, a full day's hours can pass without a midnight, which is still today.
  const days = Math.round((startOfDay(now) - startOfDay(then)) / DAY);
  if (elapsed < DAY || days < 1) {
    const count = Math.floor(elapsed / HOUR);
    return t("{count} 小时前", count === 1 ? "{count} hour ago" : "{count} hours ago", { count });
  }
  if (days === 1) return t("昨天", "Yesterday");
  if (days < 7) return t("{count} 天前", "{count} days ago", { count: days });
  const date = new Date(then);
  return date.toLocaleDateString(language, {
    month: "short",
    day: "numeric",
    ...(date.getFullYear() === new Date(now).getFullYear() ? {} : { year: "numeric" })
  });
}

/** The commands worth knowing on a task nothing has been sent in yet, in the order listed. */
const LANDING_COMMANDS: readonly ShortcutCommandId[] = [
  "conversation.next",
  "app.conversation_settings.open",
  "panel.browser.toggle"
];

interface ShortcutRow {
  id: string;
  label: string;
  caps: string[];
}

/**
 * The page of a task nothing has been sent in: the timeline's empty state on the main
 * conversation. The stock notice only said the timeline was empty; this one says what the
 * page is for and fills the room above the composer with what a new task can start from — the
 * project's tasks by recency, which the sidebar keeps in the user's own order instead, and the
 * keys that work here, read from the user's bindings so a rebound key shows as bound.
 *
 * It keeps the `.empty-state` box: the timeline centres it and tells an empty timeline from a
 * filled one by that class, and a right-click anywhere on it is still a right-click on the
 * timeline, which is how a context is written by hand before anything is sent.
 */
export function DraftLanding({
  recentTasks,
  onOpenTask,
  shortcuts,
  sendShortcut,
  newlineShortcut,
  canInsert,
  now: fixedNow
}: DraftLandingProps) {
  const { t, resolvedLanguage } = useI18n();
  const tasks = recentTasks.slice(0, DRAFT_LANDING_TASK_LIMIT);
  // The page can sit open for hours and nothing else redraws it, so the ages keep their own
  // time — by the minute, which is the finest they are told in.
  const clock = useNow(fixedNow === undefined && tasks.length > 0, MINUTE);
  const now = fixedNow ?? clock;

  const commandLabels: Partial<Record<ShortcutCommandId, string>> = {
    "conversation.next": t("下一个对话", "Next conversation"),
    "app.conversation_settings.open": t("本对话设置", "Conversation settings"),
    "panel.browser.toggle": t("内置浏览器", "Built-in browser")
  };
  const rows: ShortcutRow[] = [
    { id: "send", label: t("发送", "Send"), caps: shortcutKeyCaps(sendShortcut) },
    { id: "newline", label: t("换行", "New line"), caps: shortcutKeyCaps(newlineShortcut) },
    // Not a key, but it is the one gesture of this page nothing on screen gives away.
    ...(canInsert
      ? [{ id: "insert", label: t("插入上下文", "Insert context"), caps: [t("右键", "Right-click")] }]
      : []),
    ...LANDING_COMMANDS.flatMap((id) => {
      const command = SHORTCUT_COMMANDS.find((entry) => entry.id === id);
      const preference = command ? resolveShortcut(shortcuts, command) : null;
      // A command the user switched off or left unbound has no key to show.
      if (!preference?.enabled || !preference.binding.length) return [];
      return [{ id, label: commandLabels[id] ?? id, caps: shortcutKeyCaps(preference.binding) }];
    })
  ].filter((row) => row.caps.length > 0);

  return (
    <div className="empty-state draft-landing">
      <div className="draft-landing__hero">
        <MewrkIcon className="draft-landing__mark" plate={false} size={46} />
        <h2 className="draft-landing__title">{t("喵。开工。", "Mew. Work.")}</h2>
        <p className="draft-landing__lede">
          {canInsert
            ? t(
              "在下方说说要做什么，或右键手写上下文。",
              "Say what to do below, or right-click to hand-write context."
            )
            : t("在下方说说要做的事。", "Say what to do below.")}
        </p>
      </div>
      <div className="draft-landing__columns">
        {tasks.length > 0 && (
          <section className="draft-landing__section" aria-label={t("最近任务", "Recent tasks")}>
            <h3 className="draft-landing__heading">{t("最近任务", "Recent tasks")}</h3>
            <ul className="draft-landing__list">
              {tasks.map((task) => (
                <li key={task.id}>
                  <button
                    type="button"
                    className="draft-landing__task"
                    onClick={() => onOpenTask(task.id)}
                  >
                    <ConversationStatusMark status={task.status} />
                    <span className="draft-landing__task-title">{task.title}</span>
                    <span className="draft-landing__task-age">
                      {taskAgeLabel(task.updatedAt, now, t, resolvedLanguage)}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </section>
        )}
        <section className="draft-landing__section" aria-label={t("快捷键", "Keyboard shortcuts")}>
          <h3 className="draft-landing__heading">{t("快捷键", "Keyboard shortcuts")}</h3>
          <ul className="draft-landing__list">
            {rows.map((row) => (
              <li key={row.id} className="draft-landing__shortcut">
                <span className="draft-landing__shortcut-label">{row.label}</span>
                <span className="draft-landing__keys">
                  {row.caps.map((cap) => <kbd key={cap}>{cap}</kbd>)}
                </span>
              </li>
            ))}
          </ul>
        </section>
      </div>
    </div>
  );
}
