import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { configureI18n } from "../i18n";
import type { ContextItem } from "../types";
import { ContextUsageMeter, type ContextUsageMeterProps } from "./ContextUsageMeter";

function contexts(): ContextItem[] {
  return [
    // The system prompt is a timeline card now, so it reaches the meter through
    // `contexts` like every other component of the breakdown.
    { id: "s1", kind: "system", content: "s".repeat(200), createdAt: "2026-08-27T00:00:00Z" },
    { id: "u1", kind: "user", content: "u".repeat(400), createdAt: "2026-08-27T00:00:00Z" },
    { id: "a1", kind: "assistant", content: "a".repeat(800), createdAt: "2026-08-27T00:00:00Z" },
    { id: "r1", kind: "reasoning", content: "r".repeat(200), createdAt: "2026-08-27T00:00:00Z" },
    {
      id: "t1",
      kind: "tool",
      toolName: "read_file",
      input: {},
      result: {
        success: true,
        output: "t".repeat(1200),
        executedAt: "2026-08-27T00:00:00Z",
        durationMs: 1
      },
      createdAt: "2026-08-27T00:00:00Z"
    }
  ];
}

function renderMeter(overrides: Partial<ContextUsageMeterProps> = {}) {
  render(
    <ContextUsageMeter
      contexts={contexts()}
      tokens={180_000}
      estimated={false}
      contextWindow={1_000_000}
      counts={{ tools: 38, mcpServers: 2, skills: 4, agentRoles: 5 }}
      {...overrides}
    />
  );
}

describe("ContextUsageMeter", () => {
  beforeEach(() => configureI18n("zh-CN"));

  it("puts the numbers on the ring's accessible name instead of into the composer row", async () => {
    const user = userEvent.setup();
    renderMeter();

    const trigger = screen.getByRole("button", { name: "上下文用量：180k / 1m（18%）" });
    // The ring contains no text so the bottom row width remains stable as values change each turn.
    expect(trigger).toHaveTextContent("");

    await user.click(trigger);
    const panel = await screen.findByRole("dialog", { name: "上下文窗口用量" });
    expect(within(panel).getByText("180k / 1m（18%）")).toBeInTheDocument();
  });

  it("lists the composition, free space and the per-conversation counts", async () => {
    const user = userEvent.setup();
    renderMeter();

    await user.click(screen.getByRole("button", { name: /^上下文用量：/ }));
    const panel = await screen.findByRole("dialog", { name: "上下文窗口用量" });

    for (const label of ["系统提示词", "用户消息", "助手回复", "思考", "工具调用", "其他（工具定义等）", "剩余空间"]) {
      expect(within(panel).getByText(label)).toBeInTheDocument();
    }
    // Counts belong to the conversation's four lists, not global assets.
    const counts = panel.querySelector(".context-usage-panel__counts");
    expect(counts).toHaveTextContent("工具38");
    expect(counts).toHaveTextContent("MCP2");
    expect(counts).toHaveTextContent("技能4");
    expect(counts).toHaveTextContent("角色5");
  });

  it("ends at the counts row, with no settings action or estimate note", async () => {
    const user = userEvent.setup();
    renderMeter();

    const trigger = screen.getByRole("button", { name: /^上下文用量：/ });
    expect(trigger).not.toBeDisabled();
    await user.click(trigger);
    const panel = await screen.findByRole("dialog", { name: "上下文窗口用量" });
    expect(within(panel).queryByRole("button", { name: "打开上下文管理设置" })).not.toBeInTheDocument();
    expect(panel).not.toHaveTextContent("成分按本地估算拆分");
    expect(panel.lastElementChild).toHaveClass("context-usage-panel__counts");
  });

  it("marks an estimated total and drops the percentage when no window is declared", async () => {
    const user = userEvent.setup();
    renderMeter({ estimated: true, contextWindow: null, tokens: 12_345 });

    await user.click(screen.getByRole("button", { name: "上下文用量：~12.3k" }));
    const panel = await screen.findByRole("dialog", { name: "上下文窗口用量" });
    expect(within(panel).queryByText("剩余空间")).not.toBeInTheDocument();
  });

  it("says a share is too small to print instead of rounding it to zero", async () => {
    const user = userEvent.setup();
    // At 45 tokens in a 1m window, one-decimal formatting would render every row as "0.0%".
    renderMeter({ tokens: 45 });

    await user.click(screen.getByRole("button", { name: /^上下文用量：/ }));
    const panel = await screen.findByRole("dialog", { name: "上下文窗口用量" });
    expect(within(panel).getAllByText("<0.1%").length).toBeGreaterThan(0);
    expect(within(panel).queryByText("0.0%")).not.toBeInTheDocument();
    expect(within(panel).getByText("100.0%")).toBeInTheDocument();
  });

  it("says so when the model cannot project usage at all", async () => {
    const user = userEvent.setup();
    renderMeter({ unprojectable: true });

    await user.click(screen.getByRole("button", { name: "上下文用量：不可投影" }));
    const panel = await screen.findByRole("dialog", { name: "上下文窗口用量" });
    expect(within(panel).getByText("不可投影")).toBeInTheDocument();
  });

  describe("auto-compact", () => {
    const native = { thresholdPercent: 90, retainedTokens: 64_000 };
    const settings = (enabled: boolean, thresholdPercent: number, nativeSettings = native) => ({
      enabled,
      thresholdPercent,
      native: nativeSettings
    });

    async function openAutoCompact(overrides: Partial<ContextUsageMeterProps> = {}) {
      const user = userEvent.setup();
      const onAutoCompactChange = vi.fn();
      const onCompactionMethodChange = vi.fn();
      renderMeter({
        autoCompact: settings(true, 80),
        onAutoCompactChange,
        onCompactionMethodChange,
        ...overrides
      });
      await user.click(screen.getByRole("button", { name: /^上下文用量：/ }));
      const panel = await screen.findByRole("dialog", { name: "上下文窗口用量" });
      return { user, panel, onAutoCompactChange, onCompactionMethodChange };
    }

    it("opens its submenu from a row at the foot of the panel", async () => {
      const { user, panel } = await openAutoCompact();
      expect(panel.lastElementChild).toHaveClass("context-usage-compact");
      const row = within(panel).getByRole("button", { name: /自动压缩/ });
      expect(row).toHaveTextContent("80%");
      expect(row).toHaveAttribute("aria-expanded", "false");
      expect(screen.queryByRole("group", { name: "自动压缩" })).not.toBeInTheDocument();

      await user.click(row);
      expect(row).toHaveAttribute("aria-expanded", "true");
      const menu = screen.getByRole("group", { name: "自动压缩" });
      // The switch heads the submenu; the two methods follow it, at the left.
      const tabs = within(menu).getByRole("tablist", { name: "压缩方式" });
      const enable = within(menu).getByRole("switch", { name: "启用自动压缩" });
      expect(menu.firstElementChild).toContainElement(enable);
      expect(menu.firstElementChild?.nextElementSibling).toBe(tabs);
      expect(enable).toHaveAttribute("aria-checked", "true");
      expect(within(tabs).getByRole("tab", { name: "交接" })).toHaveAttribute("aria-selected", "true");
      expect(within(menu).getByRole("spinbutton", { name: "交接阈值（百分比）" })).toHaveValue(80);
      const slider = within(menu).getByRole("slider", { name: "交接阈值" });
      expect(slider).toHaveValue("80");
      expect(slider).toHaveAttribute("min", "20");
      expect(slider).toHaveAttribute("max", "97");
      // 80% of the 1m window.
      expect(menu).toHaveTextContent("上下文达到 800k tokens 时，模型写好交接文档，在新的交接会话中继续");
      // A model that does not compact natively is offered only the handoff.
      expect(within(tabs).getByRole("tab", { name: "原生压缩" })).toBeDisabled();
      expect(within(menu).queryByRole("button", { name: "立即压缩" })).not.toBeInTheDocument();

      await user.click(row);
      expect(screen.queryByRole("group", { name: "自动压缩" })).not.toBeInTheDocument();
    });

    it("marks the threshold on the usage bar", async () => {
      const { panel } = await openAutoCompact({ autoCompact: settings(true, 64) });
      const marker = panel.querySelector<HTMLElement>(".context-usage-panel__threshold");
      expect(marker?.style.left).toBe("64%");
    });

    it("switches auto-compact off without touching the threshold", async () => {
      const { user, panel, onAutoCompactChange } = await openAutoCompact();
      await user.click(within(panel).getByRole("button", { name: /自动压缩/ }));
      await user.click(screen.getByRole("switch", { name: "启用自动压缩" }));
      expect(onAutoCompactChange).toHaveBeenCalledWith(settings(false, 80));
    });

    it("commits a typed threshold on Enter, clamped to 20–97", async () => {
      const { user, panel, onAutoCompactChange } = await openAutoCompact();
      await user.click(within(panel).getByRole("button", { name: /自动压缩/ }));
      const field = screen.getByRole("spinbutton", { name: "交接阈值（百分比）" });

      await user.clear(field);
      await user.type(field, "8");
      // "8" is on its way to something; nothing is saved while typing.
      expect(onAutoCompactChange).not.toHaveBeenCalled();
      await user.type(field, "5{Enter}");
      expect(onAutoCompactChange).toHaveBeenLastCalledWith(settings(true, 85));

      await user.clear(field);
      await user.type(field, "150{Enter}");
      expect(onAutoCompactChange).toHaveBeenLastCalledWith(settings(true, 97));

      await user.clear(field);
      await user.type(field, "5");
      await user.tab();
      expect(onAutoCompactChange).toHaveBeenLastCalledWith(settings(true, 20));
    });

    it("commits the slider when it is released, not at every step", async () => {
      const { user, panel, onAutoCompactChange } = await openAutoCompact();
      await user.click(within(panel).getByRole("button", { name: /自动压缩/ }));
      const slider = screen.getByRole("slider", { name: "交接阈值" });

      fireEvent.input(slider, { target: { value: "60" } });
      expect(onAutoCompactChange).not.toHaveBeenCalled();
      // The number field follows the drag as it happens.
      expect(screen.getByRole("spinbutton", { name: "交接阈值（百分比）" })).toHaveValue(60);
      fireEvent.pointerUp(slider);
      expect(onAutoCompactChange).toHaveBeenCalledTimes(1);
      expect(onAutoCompactChange).toHaveBeenCalledWith(settings(true, 60));
    });

    it("rounds the token threshold down", async () => {
      const { user, panel } = await openAutoCompact({
        contextWindow: 999,
        tokens: 100,
        autoCompact: settings(true, 97)
      });
      await user.click(within(panel).getByRole("button", { name: /自动压缩/ }));
      // 999 × 97% = 969.03.
      expect(screen.getByRole("group", { name: "自动压缩" })).toHaveTextContent("上下文达到 969 tokens 时");
    });

    it("disables the threshold while it is off, and says when there is no window to measure", async () => {
      const { user, panel } = await openAutoCompact({
        contextWindow: null,
        autoCompact: settings(false, 80)
      });
      const row = within(panel).getByRole("button", { name: /自动压缩/ });
      expect(row).toHaveTextContent("已关闭");
      expect(panel.querySelector(".context-usage-panel__threshold")).toBeNull();
      await user.click(row);
      const menu = screen.getByRole("group", { name: "自动压缩" });
      expect(within(menu).getByRole("switch", { name: "启用自动压缩" })).toHaveAttribute("aria-checked", "false");
      expect(within(menu).getByRole("slider", { name: "交接阈值" })).toBeDisabled();
      expect(within(menu).getByRole("spinbutton", { name: "交接阈值（百分比）" })).toBeDisabled();
      expect(menu).toHaveTextContent("当前模型没有设置上下文窗口，无法自动压缩");
    });

    describe("native compaction", () => {
      it("is one of the two methods, a page with a threshold and a budget of its own", async () => {
        const { user, panel } = await openAutoCompact({
          nativeCompactionAvailable: true,
          compactionMethod: "native"
        });
        const row = within(panel).getByRole("button", { name: /自动压缩/ });
        expect(row).toHaveTextContent("原生 90%");
        // One mark, the method's own: dashed for native compaction.
        const markers = panel.querySelectorAll<HTMLElement>(".context-usage-panel__threshold");
        expect(markers).toHaveLength(1);
        expect(markers[0]).toHaveClass("context-usage-panel__threshold--native");
        expect(markers[0].style.left).toBe("90%");

        await user.click(row);
        const menu = screen.getByRole("group", { name: "自动压缩" });
        expect(within(menu).getByRole("tab", { name: "原生压缩" })).toHaveAttribute("aria-selected", "true");
        expect(within(menu).getByRole("tab", { name: "交接" })).toHaveAttribute("aria-selected", "false");
        expect(within(menu).getByRole("spinbutton", { name: "原生压缩阈值（百分比）" })).toHaveValue(90);
        expect(within(menu).getByRole("spinbutton", { name: "保留最近的用户消息（token）" })).toHaveValue(64_000);
        expect(menu.querySelector(".context-usage-compact__retained")).toHaveTextContent("保留最近的token 用户消息");
        // 90% of the 1m window.
        expect(menu).toHaveTextContent(
          "上下文达到 900k tokens 时，模型把上下文原生压缩成一个压缩项，在新会话中继续"
        );
        // The other method's page is not shown.
        expect(within(menu).queryByRole("spinbutton", { name: "交接阈值（百分比）" })).not.toBeInTheDocument();
        expect(menu).toHaveTextContent("方式按对话各自记");
      });

      it("records the conversation's choice, and the numbers globally", async () => {
        const { user, panel, onAutoCompactChange, onCompactionMethodChange } = await openAutoCompact({
          nativeCompactionAvailable: true,
          compactionMethod: "handoff"
        });
        await user.click(within(panel).getByRole("button", { name: /自动压缩/ }));
        expect(screen.queryByRole("spinbutton", { name: "原生压缩阈值（百分比）" })).not.toBeInTheDocument();
        await user.click(screen.getByRole("tab", { name: "原生压缩" }));
        expect(onCompactionMethodChange).toHaveBeenCalledWith("native");
        expect(onAutoCompactChange).not.toHaveBeenCalled();
      });

      it("changes its own threshold and budget, never the handoff's", async () => {
        const { user, panel, onAutoCompactChange } = await openAutoCompact({
          nativeCompactionAvailable: true,
          compactionMethod: "native"
        });
        await user.click(within(panel).getByRole("button", { name: /自动压缩/ }));
        const field = screen.getByRole("spinbutton", { name: "原生压缩阈值（百分比）" });
        await user.clear(field);
        await user.type(field, "55{Enter}");
        expect(onAutoCompactChange).toHaveBeenLastCalledWith(settings(true, 80, { ...native, thresholdPercent: 55 }));

        const retained = screen.getByRole("spinbutton", { name: "保留最近的用户消息（token）" });
        await user.clear(retained);
        await user.type(retained, "20000{Enter}");
        expect(onAutoCompactChange).toHaveBeenLastCalledWith(settings(true, 80, { ...native, retainedTokens: 20_000 }));
        await user.clear(retained);
        await user.type(retained, "900000{Enter}");
        expect(onAutoCompactChange).toHaveBeenLastCalledWith(settings(true, 80, { ...native, retainedTokens: 128_000 }));
      });

      it("keeps its budget as set, whatever its threshold, and with auto-compact off", async () => {
        const { user, panel, onAutoCompactChange } = await openAutoCompact({
          contextWindow: 100_000,
          tokens: 1_000,
          nativeCompactionAvailable: true,
          compactionMethod: "native",
          autoCompact: settings(false, 80, { thresholdPercent: 50, retainedTokens: 64_000 })
        });
        await user.click(within(panel).getByRole("button", { name: /自动压缩/ }));
        const retained = screen.getByRole("spinbutton", { name: "保留最近的用户消息（token）" });
        // Compacting now spends it too, so it stays in reach with the switch off.
        expect(retained).toBeEnabled();
        expect(retained).toHaveValue(64_000);
        await user.clear(retained);
        await user.type(retained, "60000{Enter}");
        expect(onAutoCompactChange).toHaveBeenLastCalledWith(
          settings(false, 80, { thresholdPercent: 50, retainedTokens: 60_000 })
        );
      });

      it("compacts at once from its page, with auto-compact off too", async () => {
        const onCompactNow = vi.fn();
        const { user, panel } = await openAutoCompact({
          nativeCompactionAvailable: true,
          compactionMethod: "native",
          autoCompact: settings(false, 80),
          onCompactNow
        });
        await user.click(within(panel).getByRole("button", { name: /自动压缩/ }));
        await user.click(screen.getByRole("button", { name: "立即压缩" }));
        expect(onCompactNow).toHaveBeenCalledTimes(1);
        expect(screen.queryByRole("dialog", { name: "上下文窗口用量" })).not.toBeInTheDocument();
      });

      it("holds back compacting at once while it is out of reach", async () => {
        const onCompactNow = vi.fn();
        const { user, panel } = await openAutoCompact({
          nativeCompactionAvailable: true,
          compactionMethod: "native",
          onCompactNow,
          compactNowBlocked: "对话正在运行，停下后才能压缩"
        });
        await user.click(within(panel).getByRole("button", { name: /自动压缩/ }));
        const button = screen.getByRole("button", { name: "立即压缩" });
        expect(button).toBeDisabled();
        expect(button).toHaveAttribute("title", "对话正在运行，停下后才能压缩");
      });

      it("stays reachable on a model that cannot hand off", async () => {
        const { user, panel } = await openAutoCompact({
          autoCompactUnavailable: true,
          nativeCompactionAvailable: true,
          compactionMethod: "native"
        });
        const row = within(panel).getByRole("button", { name: /自动压缩/ });
        expect(row).not.toBeDisabled();
        expect(row).toHaveTextContent("原生 90%");
        await user.click(row);
        const menu = screen.getByRole("group", { name: "自动压缩" });
        const handoff = within(menu).getByRole("tab", { name: "交接" });
        expect(handoff).toBeDisabled();
        expect(handoff.getAttribute("title")).toContain("当前模型不支持中途追加工具");
        expect(within(menu).queryByRole("slider", { name: "交接阈值" })).not.toBeInTheDocument();
        expect(within(menu).getByRole("tab", { name: "原生压缩" })).toBeEnabled();
      });

      it("is out of reach only when neither method applies", async () => {
        const { panel } = await openAutoCompact({ autoCompactUnavailable: true, compactionMethod: null });
        const row = within(panel).getByRole("button", { name: /自动压缩/ });
        expect(row).toBeDisabled();
        expect(row).toHaveTextContent("当前模型不支持");
        expect(panel.querySelector(".context-usage-panel__threshold")).toBeNull();
      });

      it("counts a compaction's item as compacted history", async () => {
        const user = userEvent.setup();
        renderMeter({
          contexts: contexts().slice(0, 2),
          compaction: {
            providerId: "codex",
            model: "gpt-6-astra",
            parts: [],
            retained: [{ role: "user", sourceId: "u0", content: "u".repeat(400) }],
            tokensBefore: 900_000,
            tokensAfter: 5_100
          },
          tokens: 200_000
        });
        await user.click(screen.getByRole("button", { name: /^上下文用量：/ }));
        const panel = await screen.findByRole("dialog", { name: "上下文窗口用量" });
        expect(within(panel).getByText("压缩的历史")).toBeInTheDocument();
        expect(panel.querySelector("[data-segment='compacted']")).not.toBeNull();
      });
    });
  });

  it("renders in English when the app language is English", async () => {
    configureI18n("en-US");
    const user = userEvent.setup();
    renderMeter();

    await user.click(screen.getByRole("button", { name: /^Context usage:/ }));
    const panel = await screen.findByRole("dialog", { name: "Context window usage" });
    expect(within(panel).getByText("Context window")).toBeInTheDocument();
    expect(within(panel).getByText("Free space")).toBeInTheDocument();
  });
});
