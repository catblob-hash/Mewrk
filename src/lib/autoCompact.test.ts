import { describe, expect, it } from "vitest";
import type { ModelCapability } from "../types";
import {
  autoCompactThresholdTokens,
  compactionMethodInEffect,
  defaultAutoCompactSettings,
  defaultCompactionMethod,
  nativeRetainedBudget,
  normalizeAutoCompactSettings
} from "./autoCompact";

describe("auto-compact settings", () => {
  it("starts on, handing off at 80% and compacting natively at 90%, keeping Codex's 64k", () => {
    expect(defaultAutoCompactSettings()).toEqual({
      enabled: true,
      thresholdPercent: 80,
      native: { thresholdPercent: 90, retainedTokens: 64_000 }
    });
  });

  it("reads a document from before native compaction with its defaults", () => {
    expect(normalizeAutoCompactSettings({ enabled: false, thresholdPercent: 64 }, defaultAutoCompactSettings()))
      .toEqual({
        enabled: false,
        thresholdPercent: 64,
        native: { thresholdPercent: 90, retainedTokens: 64_000 }
      });
  });

  it("clamps each threshold and the budget to the ranges offered", () => {
    const normalized = normalizeAutoCompactSettings(
      { enabled: true, thresholdPercent: 5, native: { thresholdPercent: 120, retainedTokens: 1_000_000 } },
      defaultAutoCompactSettings()
    );
    expect(normalized).toEqual({
      enabled: true,
      thresholdPercent: 20,
      native: { thresholdPercent: 97, retainedTokens: 128_000 }
    });
    expect(normalizeAutoCompactSettings({ native: { retainedTokens: -5 } }, defaultAutoCompactSettings()).native.retainedTokens)
      .toBe(0);
  });

  it("keeps the retained budget the user set, whatever the threshold", () => {
    expect(autoCompactThresholdTokens(272_000, 90)).toBe(244_800);
    expect(nativeRetainedBudget(64_000)).toBe(64_000);
    expect(nativeRetainedBudget(128_000)).toBe(128_000);
    expect(nativeRetainedBudget(500_000)).toBe(128_000);
    expect(nativeRetainedBudget(-5)).toBe(0);
  });
});

describe("compaction method", () => {
  const codex = (capabilities: ModelCapability[]) => ({
    provider: { family: "openai_codex" as const },
    model: { capabilities }
  });
  const both = codex(["tool_append", "native_compaction"]);

  it("is native for a new conversation on a model that compacts natively, the handoff elsewhere", () => {
    expect(defaultCompactionMethod(both)).toBe("native");
    expect(defaultCompactionMethod(codex(["tool_append"]))).toBe("handoff");
    expect(defaultCompactionMethod({ provider: { family: "anthropic" }, model: { capabilities: ["native_compaction"] } }))
      .toBe("handoff");
    expect(defaultCompactionMethod(null)).toBe("handoff");
  });

  it("is the conversation's choice where the model can do it, the other where only that one works", () => {
    expect(compactionMethodInEffect("native", both)).toBe("native");
    expect(compactionMethodInEffect("handoff", both)).toBe("handoff");
    expect(compactionMethodInEffect("native", codex(["tool_append"]))).toBe("handoff");
    expect(compactionMethodInEffect("handoff", codex(["native_compaction"]))).toBe("native");
    expect(compactionMethodInEffect("native", codex([]))).toBeNull();
    expect(compactionMethodInEffect("handoff", null)).toBeNull();
  });
});
