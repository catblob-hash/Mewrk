import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ComponentProps } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { configureI18n, translate } from "../i18n";
import type { TranslationFunction } from "../i18n";
import { DRAFT_LANDING_TASK_LIMIT, DraftLanding, taskAgeLabel } from "./DraftLanding";
import type { DraftLandingTask } from "./DraftLanding";
import type { ConversationStatus } from "./Sidebar";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
/**
 * A local wall-clock moment. Day counts are calendar days in the local zone, so the fixed dates
 * are built in local time too, and the day-level cases hold in whatever zone the suite runs in.
 */
const local = (year: number, month: number, day: number, hour = 12, minute = 0) => (
  new Date(year, month - 1, day, hour, minute).getTime()
);
/** A fixed "now", so every age is a known distance from it. */
const NOW = local(2026, 10, 12, 12);

const ago = (elapsed: number) => new Date(NOW - elapsed).toISOString();
const at = (time: number) => new Date(time).toISOString();

const zh: TranslationFunction = (zhCn, enUs, parameters) => translate("zh-CN", zhCn, enUs, parameters);
const en: TranslationFunction = (zhCn, enUs, parameters) => translate("en-US", zhCn, enUs, parameters);

type LandingProps = ComponentProps<typeof DraftLanding>;

function task(id: string, title: string, updatedAt: string, status: ConversationStatus = "idle"): DraftLandingTask {
  return { id, title, updatedAt, status };
}

function landingProps(): LandingProps {
  return {
    recentTasks: [],
    onOpenTask: vi.fn(),
    shortcuts: {},
    sendShortcut: ["Enter"],
    newlineShortcut: ["Shift", "Enter"],
    canInsert: true,
    now: NOW
  };
}

function renderLanding(overrides: Partial<LandingProps> = {}) {
  return render(<DraftLanding {...landingProps()} {...overrides} />);
}

/** The task rows the page draws, in the order it draws them. Read by class, so it holds in either language. */
function taskRows(): HTMLElement[] {
  return Array.from(document.querySelectorAll<HTMLElement>(".draft-landing__task"));
}

function rowTitles(): (string | null | undefined)[] {
  return taskRows().map((row) => row.querySelector(".draft-landing__task-title")?.textContent);
}

/** The keycaps a shortcut row draws, one entry per `<kbd>`. */
function keyCaps(label: string): string[] {
  const row = screen.getByText(label, { selector: ".draft-landing__shortcut-label" })
    .closest<HTMLElement>(".draft-landing__shortcut");
  return Array.from(row!.querySelectorAll("kbd"), (kbd) => kbd.textContent ?? "");
}

describe("DraftLanding", () => {
  afterEach(() => configureI18n("zh-CN"));

  describe("brand and lede", () => {
    it("keeps the empty-state box the timeline relies on, under the brand and title", () => {
      const { container } = renderLanding();
      // The timeline centres `.empty-state` and tells an empty timeline from a filled one by it.
      expect(container.firstElementChild).toHaveClass("empty-state", "draft-landing");
      expect(screen.getByRole("heading", { level: 2, name: "喵。开工。" })).toBeInTheDocument();
      // The app's mark without its plate: the plate is the Dock icon, not a page ornament.
      const mark = container.querySelector(".draft-landing__mark");
      expect(mark?.querySelector("rect")).toBeNull();
      expect(mark?.querySelector(".mewrk-icon__mark")).not.toBeNull();
    });

    it("says a right-click inserts a context, and lists that gesture, where the timeline accepts edits", () => {
      const { container } = renderLanding({ canInsert: true });
      expect(container.querySelector(".draft-landing__lede")).toHaveTextContent("右键");
      expect(keyCaps("插入上下文")).toEqual(["右键"]);
    });

    it("says nothing of right-click on a timeline that cannot take an insert", () => {
      // A locked timeline opens no insert menu, so the page must not advertise one.
      const { container } = renderLanding({ canInsert: false });
      expect(container.querySelector(".draft-landing__lede")).toHaveTextContent("在下方说说要做的事。");
      expect(container.querySelector(".draft-landing__lede")).not.toHaveTextContent("右键");
      expect(screen.queryByText("插入上下文")).toBeNull();
    });
  });

  describe("recent tasks", () => {
    it("draws the tasks in the order it is handed them", () => {
      // The caller sorts by recency; the page must not re-sort, so an older task given first stays first.
      renderLanding({
        recentTasks: [
          task("t-old", "较早的任务", ago(3 * DAY)),
          task("t-new", "最新的任务", ago(5 * MINUTE))
        ]
      });
      expect(rowTitles()).toEqual(["较早的任务", "最新的任务"]);
    });

    it("lists at most DRAFT_LANDING_TASK_LIMIT tasks", () => {
      const recentTasks = Array.from({ length: DRAFT_LANDING_TASK_LIMIT + 3 }, (_, index) => (
        task(`t-${index}`, `任务 ${index}`, ago((index + 1) * MINUTE))
      ));
      renderLanding({ recentTasks });
      expect(rowTitles()).toEqual(recentTasks.slice(0, DRAFT_LANDING_TASK_LIMIT).map((item) => item.title));
    });

    it("opens the task whose row was clicked, by its id", async () => {
      const user = userEvent.setup();
      const onOpenTask = vi.fn();
      renderLanding({
        recentTasks: [task("t-a", "甲任务", ago(MINUTE)), task("t-b", "乙任务", ago(HOUR))],
        onOpenTask
      });
      await user.click(screen.getByRole("button", { name: /乙任务/ }));
      expect(onOpenTask).toHaveBeenCalledTimes(1);
      expect(onOpenTask).toHaveBeenCalledWith("t-b");
    });

    it("has no recent-tasks section without tasks, and still shows the shortcuts", () => {
      renderLanding({ recentTasks: [] });
      expect(screen.queryByRole("region", { name: "最近任务" })).toBeNull();
      expect(screen.getByRole("region", { name: "快捷键" })).toBeInTheDocument();
    });

    it("marks each row's status with that status's class, and names the labelled ones", () => {
      const statuses: ConversationStatus[] = ["idle", "running", "blocked", "completed"];
      renderLanding({
        recentTasks: statuses.map((status, index) => (
          task(`t-${status}`, `状态 ${status}`, ago((index + 1) * MINUTE), status)
        ))
      });
      const rows = taskRows();
      rows.forEach((row, index) => {
        expect(row.querySelector(".conversation-status")).toHaveClass(`conversation-status--${statuses[index]}`);
      });
      // An idle mark is decoration; the other three say what state the task is in.
      expect(within(rows[0]).queryByRole("img")).toBeNull();
      expect(within(rows[1]).getByRole("img", { name: "正在进行" })).toBeInTheDocument();
      expect(within(rows[2]).getByRole("img", { name: "等待你处理" })).toBeInTheDocument();
      expect(within(rows[3]).getByRole("img", { name: "已完成" })).toBeInTheDocument();
    });

    it("shows each task's age beside its title", () => {
      renderLanding({
        recentTasks: [
          task("t-a", "甲任务", ago(12 * MINUTE)),
          task("t-b", "乙任务", at(local(2026, 10, 9, 12)))
        ]
      });
      const rows = taskRows();
      expect(rows[0].querySelector(".draft-landing__task-age")).toHaveTextContent("12 分钟前");
      expect(rows[1].querySelector(".draft-landing__task-age")).toHaveTextContent("3 天前");
    });

    it("measures the ages from the clock when no `now` is given, and re-measures them each minute", () => {
      vi.useFakeTimers();
      try {
        vi.setSystemTime(NOW);
        renderLanding({ now: undefined, recentTasks: [task("t-a", "甲任务", new Date(NOW).toISOString())] });
        const age = () => taskRows()[0].querySelector(".draft-landing__task-age");
        expect(age()).toHaveTextContent("刚刚");
        // The page can sit open for hours with nothing else redrawing it, so its ages keep time.
        act(() => { vi.advanceTimersByTime(MINUTE); });
        expect(age()).toHaveTextContent("1 分钟前");
        act(() => { vi.advanceTimersByTime(MINUTE); });
        expect(age()).toHaveTextContent("2 分钟前");
      } finally {
        vi.useRealTimers();
      }
    });
  });

  describe("shortcuts", () => {
    // jsdom reports no Apple platform, so the Control modifier reads as Ctrl, as it does on Windows.
    it("shows the default keys, each key as its own keycap", () => {
      renderLanding();
      expect(keyCaps("发送")).toEqual(["Enter"]);
      expect(keyCaps("换行")).toEqual(["Shift", "Enter"]);
      expect(keyCaps("下一个对话")).toEqual(["Ctrl", "Tab"]);
      expect(keyCaps("本对话设置")).toEqual(["Ctrl", "Shift", ","]);
      expect(keyCaps("内置浏览器")).toEqual(["Ctrl", "B"]);
    });

    it("shows a rebound command's own keys rather than its default", () => {
      renderLanding({
        shortcuts: { "panel.browser.toggle": { binding: ["Control", "Shift", "KeyY"], enabled: true } }
      });
      expect(keyCaps("内置浏览器")).toEqual(["Ctrl", "Shift", "Y"]);
    });

    it("omits a command the user switched off, and leaves the other rows alone", () => {
      // A disabled command does not dispatch, so there is no key worth showing for it.
      renderLanding({
        shortcuts: { "panel.browser.toggle": { binding: ["Control", "KeyB"], enabled: false } }
      });
      expect(screen.queryByText("内置浏览器")).toBeNull();
      expect(keyCaps("下一个对话")).toEqual(["Ctrl", "Tab"]);
    });

    it("omits a command the user left unbound", () => {
      renderLanding({ shortcuts: { "conversation.next": { binding: [], enabled: true } } });
      expect(screen.queryByText("下一个对话")).toBeNull();
    });

    it("shows the send key the composer is actually using", () => {
      renderLanding({ sendShortcut: ["Control", "Enter"] });
      expect(keyCaps("发送")).toEqual(["Ctrl", "Enter"]);
    });
  });

  describe("English", () => {
    it("labels the whole page in English under an English preference", () => {
      configureI18n("en-US");
      const { container } = renderLanding({
        recentTasks: [task("t-a", "English task", ago(12 * MINUTE))]
      });
      expect(screen.getByRole("heading", { level: 2, name: "Mew. Work." })).toBeInTheDocument();
      expect(screen.getByRole("region", { name: "Recent tasks" })).toBeInTheDocument();
      expect(screen.getByRole("region", { name: "Keyboard shortcuts" })).toBeInTheDocument();
      expect(keyCaps("Send")).toEqual(["Enter"]);
      expect(keyCaps("Insert context")).toEqual(["Right-click"]);
      expect(keyCaps("Next conversation")).toEqual(["Ctrl", "Tab"]);
      expect(keyCaps("Conversation settings")).toEqual(["Ctrl", "Shift", ","]);
      expect(keyCaps("Built-in browser")).toEqual(["Ctrl", "B"]);
      expect(taskRows()[0].querySelector(".draft-landing__task-age")).toHaveTextContent("12 min ago");
      // The task title above is English, so any Chinese left on the page is a string that was not translated.
      expect(container.textContent).not.toMatch(/[一-鿿]/);
    });
  });
});

describe("taskAgeLabel", () => {
  // Within a day the count is elapsed time, in minutes and then hours.
  it.each([
    ["the same instant", 0, "刚刚"],
    ["under a minute", 59_000, "刚刚"],
    ["one minute", MINUTE, "1 分钟前"],
    ["just under an hour", 59 * MINUTE + 59_000, "59 分钟前"],
    ["one hour", HOUR, "1 小时前"],
    ["several hours", 5 * HOUR, "5 小时前"],
    ["just under a day", 23 * HOUR, "23 小时前"]
  ])("counts elapsed time within a day: %s", (_label, elapsed, expected) => {
    expect(taskAgeLabel(ago(elapsed), NOW, zh, "zh-CN")).toBe(expected);
  });

  // Past a day the count is calendar days, so "yesterday" means the day before, not 24 to 48 hours.
  it.each([
    ["the day before", local(2026, 10, 11, 9), "昨天"],
    ["two calendar days back", local(2026, 10, 10, 12), "2 天前"],
    ["six calendar days back", local(2026, 10, 6, 12), "6 天前"]
  ])("counts calendar days past a day: %s", (_label, updatedAt, expected) => {
    expect(taskAgeLabel(at(updatedAt), NOW, zh, "zh-CN")).toBe(expected);
  });

  it("reads a task under 48 hours old as two days back when it was the night before last", () => {
    // 26 hours before 01:00 is 23:00 two days back: a day before yesterday, though under 48 hours old.
    expect(taskAgeLabel(at(local(2026, 10, 10, 23)), local(2026, 10, 12, 1), zh, "zh-CN")).toBe("2 天前");
    // Within a day the count stays elapsed time, so a task from 23:50 reads in minutes after midnight.
    expect(taskAgeLabel(at(local(2026, 10, 11, 23, 50)), local(2026, 10, 12, 0, 30), zh, "zh-CN"))
      .toBe("40 分钟前");
  });

  it("switches from a count of days to the date at one calendar week", () => {
    expect(taskAgeLabel(at(local(2026, 10, 6, 12)), NOW, zh, "zh-CN")).toBe("6 天前");
    expect(taskAgeLabel(at(local(2026, 10, 5, 12)), NOW, zh, "zh-CN")).not.toMatch(/前/);
  });

  it("reads a task older than a week as its date, with the year only from an earlier year", () => {
    // The date's wording varies by locale, so only its shape is pinned here.
    const sameYear = taskAgeLabel(at(local(2026, 10, 2, 12)), NOW, zh, "zh-CN");
    expect(sameYear).not.toBe("");
    expect(sameYear).not.toMatch(/前|刚刚|昨天/);
    expect(sameYear).not.toContain("2026");

    expect(taskAgeLabel(at(local(2025, 9, 8, 12)), NOW, zh, "zh-CN")).toContain("2025");
  });

  it.each([
    ["the same instant", 0, "Just now"],
    ["one minute", MINUTE, "1 min ago"],
    ["12 minutes", 12 * MINUTE, "12 min ago"],
    ["one hour", HOUR, "1 hour ago"],
    ["five hours", 5 * HOUR, "5 hours ago"]
  ])("reads elapsed time in English: %s", (_label, elapsed, expected) => {
    expect(taskAgeLabel(ago(elapsed), NOW, en, "en-US")).toBe(expected);
  });

  it("reads calendar days in English", () => {
    expect(taskAgeLabel(at(local(2026, 10, 11, 9)), NOW, en, "en-US")).toBe("Yesterday");
    expect(taskAgeLabel(at(local(2026, 10, 9, 12)), NOW, en, "en-US")).toBe("3 days ago");
  });

  it("reads a stamp after now as just now, and an unreadable stamp as nothing", () => {
    // A stamp ahead of the clock is clock drift, not a task from the future.
    expect(taskAgeLabel(ago(-HOUR), NOW, zh, "zh-CN")).toBe("刚刚");
    expect(taskAgeLabel("not a date", NOW, zh, "zh-CN")).toBe("");
  });
});
