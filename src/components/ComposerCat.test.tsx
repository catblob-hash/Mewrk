import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import orchestrationCss from "../styles/orchestration.css?raw";
import { CAT_COMPOSER_VIEWBOX, CAT_LEDGE_Y, CAT_ON_LEDGE, CAT_SCENE_VIEWBOX, CAT_TAIL_RAISED_ROOT } from "./catArt";
import { ComposerCat } from "./ComposerCat";

/** The swing either way `catArt.test.ts` checks the raised tail's geometry across. */
const TAIL_SWING_CHECKED = 12;

/**
 * The declaration block of a rule, read from the stylesheet's source: jsdom never lays the
 * stylesheet out, so the source is the only place these numbers can be seen.
 */
function block(selector: string): string {
  const escaped = selector.replace(/[.]/g, "\\.");
  const match = orchestrationCss.match(new RegExp(`(?:^|\\n)${escaped}\\s*\\{([^}]*)\\}`));
  expect(match, `no rule for ${selector}`).not.toBeNull();
  return match![1]!;
}

function px(declarations: string, property: string): number {
  const match = declarations.match(new RegExp(`(?:^|[;\\s])${property}:\\s*(-?[\\d.]+)px`));
  expect(match, `no ${property}`).not.toBeNull();
  return Number(match![1]);
}

const [, top, width, height] = CAT_COMPOSER_VIEWBOX.split(" ").map(Number) as [number, number, number, number];

describe("ComposerCat", () => {
  it("draws the raised tail inside the group that lowers the cat onto the ledge", () => {
    // The tail is in the drawing's own coordinates, like the body it grows from; outside the
    // group it would float 62 units above its rump.
    const { container } = render(<ComposerCat />);
    expect(container.querySelector(".composer-cat")?.getAttribute("viewBox")).toBe(CAT_COMPOSER_VIEWBOX);
    expect(container.querySelector(".composer-cat__tail")?.parentElement?.getAttribute("transform")).toBe(CAT_ON_LEDGE);
  });

  it("keeps the cat its size and lays the ledge on the composer's border", () => {
    const rule = block(".composer-cat");
    const boxWidth = px(rule, "width");
    const boxHeight = px(rule, "height");
    // Off the scene's aspect, `xMidYMid meet` letterboxes the drawing and the ledge slides.
    expect(boxWidth / boxHeight).toBeCloseTo(width / height, 2);
    // The scale the cat had in the streaming scene's box at 45px tall.
    const sceneHeight = Number(CAT_SCENE_VIEWBOX.split(" ")[3]);
    expect(boxHeight / height).toBeCloseTo(45 / sceneHeight, 4);
    // `top` is from the padding box, 1px inside the border's outer edge; half-way down the
    // border is half a pixel above that.
    const ledge = px(rule, "top") + (boxHeight * (CAT_LEDGE_Y - top)) / height;
    expect(ledge).toBeCloseTo(-0.5, 1);
  });

  it("swings the tail about its root, no further than its geometry is checked", () => {
    const rule = block(".composer-cat__tail");
    const origin = rule.match(/transform-origin:\s*([\d.]+)px\s+([\d.]+)px/);
    expect(origin).not.toBeNull();
    expect(Number(origin![1])).toBe(CAT_TAIL_RAISED_ROOT.x);
    expect(Number(origin![2])).toBe(CAT_TAIL_RAISED_ROOT.y);
    const name = rule.match(/animation:\s*([\w-]+)/)?.[1];
    const keyframes = orchestrationCss.match(new RegExp(`@keyframes ${name}\\s*\\{(.*)\\}`))?.[1] ?? "";
    const angles = [...keyframes.matchAll(/rotate\((-?[\d.]+)deg\)/g)].map((match) => Number(match[1]));
    expect(angles.length).toBeGreaterThan(1);
    for (const angle of angles) expect(Math.abs(angle)).toBeLessThanOrEqual(TAIL_SWING_CHECKED);
  });
});
