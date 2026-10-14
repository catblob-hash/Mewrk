import { describe, expect, it } from "vitest";
import { toolCatalog } from "../seed";
import { localizeToolDescriptor } from "./toolDefaults";

describe("tool defaults", () => {
  it("does not mutate Chinese defaults or unknown third-party tools", () => {
    const tool = toolCatalog[0];
    expect(localizeToolDescriptor(tool, "zh-CN")).toBe(tool);

    const unknown = { ...tool, name: "third_party" };
    expect(localizeToolDescriptor(unknown, "en-US")).toBe(unknown);
  });

  it("localizes the label and parameters but never introduces a tool description", () => {
    // Seed descriptions remain empty; localization must neither create nor display them.
    const tool = toolCatalog[0];
    const localized = localizeToolDescriptor(tool, "en-US");
    expect(localized.label).toBe("List files");
    expect(localized.description).toBe("");
  });

  it("describes grep's modes, its context and its glob, with the defaults the tool runs with", () => {
    const grep = toolCatalog.find((candidate) => candidate.name === "grep");
    expect(grep).toBeDefined();
    if (!grep) return;
    expect(grep.parameters.map((parameter) => parameter.name))
      .toEqual(["pattern", "path", "glob", "case_sensitive", "output_mode", "context", "before", "after", "limit", "offset"]);
    // Case-sensitive and a file list unless the call says otherwise; context has no default.
    expect(Object.fromEntries(
      grep.parameters.filter((parameter) => parameter.defaultValue !== undefined).map((parameter) => [parameter.name, parameter.defaultValue])
    )).toEqual({ path: ".", case_sensitive: true, output_mode: "files_with_matches", limit: 250, offset: 0 });
    expect(grep.parameters.find((parameter) => parameter.name === "glob")?.placeholder).toBe("*.rs");

    const english = localizeToolDescriptor(grep, "en-US");
    expect(english.parameters.map((parameter) => parameter.label)).toEqual([
      "Search pattern", "Path", "File glob", "Case sensitive", "Output mode", "Context", "Lines before", "Lines after", "Limit", "Offset"
    ]);
    const help = Object.fromEntries(english.parameters.map((parameter) => [parameter.name, parameter.help]));
    expect(help.output_mode).toMatch(/^files_with_matches \(default\)/);
    expect(help.limit).toBe("Most results of the chosen mode to return: 250 by default, at most 1,000.");
    expect(help.offset).toBe("Results of the chosen mode to skip first, to fetch the next page.");
  });

  it("leaves no Chinese in any built-in tool under the English UI", () => {
    // The renderer's half of catalog.rs's `english_tool_catalog_localizes_every_visible_default…`.
    const han = /[㐀-䶿一-鿿豈-﫿]/;
    const leftovers: string[] = [];
    for (const tool of toolCatalog) {
      const localized = localizeToolDescriptor(tool, "en-US");
      if (han.test(localized.label)) leftovers.push(`${tool.name}: ${localized.label}`);
      for (const parameter of localized.parameters) {
        const where = `${tool.name}.${parameter.name}`;
        if (!parameter.label || han.test(parameter.label)) leftovers.push(`${where} label: ${parameter.label}`);
        if (parameter.help && han.test(parameter.help)) leftovers.push(`${where} help: ${parameter.help}`);
        if (parameter.placeholder && han.test(parameter.placeholder)) {
          leftovers.push(`${where} placeholder: ${parameter.placeholder}`);
        }
      }
    }
    expect(leftovers).toEqual([]);
  });
});
