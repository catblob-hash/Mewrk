import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import orchestrationCss from "../styles/orchestration.css?raw";
import { GlassPlate } from "./GlassPlate";

const stylesheets = import.meta.glob<string>("../**/*.css", { query: "?raw", import: "default", eager: true });

function withoutComments(css: string): string {
  return css.replace(/\/\*[\s\S]*?\*\//g, "");
}

describe("GlassPlate", () => {
  it("is a decorative plate holding the window-sized picture", () => {
    const { container } = render(<GlassPlate />);
    const plate = container.querySelector(".glass-plate");
    expect(plate).toHaveAttribute("aria-hidden", "true");
    expect(plate?.querySelector(":scope > .glass-plate__picture")).not.toBeNull();
    expect(plate?.textContent).toBe("");
  });

  it("leaves no background fixed to the window by attachment, which takes scrolling off the compositor", () => {
    const offenders: string[] = [];
    for (const [file, css] of Object.entries(stylesheets)) {
      for (const match of withoutComments(css).matchAll(/(--[\w-]+|background(?:-attachment)?)\s*:([^;{}]*)/g)) {
        if (/\bfixed\b/.test(match[2])) offenders.push(`${file}: ${match[0].trim().slice(0, 120)}`);
      }
    }
    expect(Object.keys(stylesheets).length).toBeGreaterThan(10);
    expect(offenders).toEqual([]);
  });

  it("keeps the chips' row untransformed, so their glass stays fixed to the window", () => {
    const rule = /\.composer-context\s*\{([^}]*)\}/.exec(withoutComments(orchestrationCss));
    expect(rule).not.toBeNull();
    expect(rule![1]).not.toMatch(/\b(?:transform|translate|scale|rotate|filter|contain)\s*:/);
  });
});
