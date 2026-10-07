import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { placeMenu } from "./menuPlacement";

describe("placeMenu", () => {
  const original = { width: window.innerWidth, height: window.innerHeight };
  const resize = (width: number, height: number) => {
    Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
    Object.defineProperty(window, "innerHeight", { configurable: true, value: height });
  };
  beforeEach(() => resize(1000, 800));
  afterEach(() => resize(original.width, original.height));

  it("hangs down from a point in the window's upper half", () => {
    expect(placeMenu({ x: 200, y: 300 }, 140, 160)).toEqual({ left: 200, top: 300, above: false });
  });

  it("stands on a point in the window's lower half, though there is room below", () => {
    expect(placeMenu({ x: 200, y: 500 }, 140, 160)).toEqual({ left: 200, top: 340, above: true });
  });

  it("holds a menu too tall for its side inside the window", () => {
    expect(placeMenu({ x: 200, y: 500 }, 140, 600)).toEqual({ left: 200, top: 8, above: true });
    expect(placeMenu({ x: 200, y: 300 }, 140, 600)).toEqual({ left: 200, top: 192, above: false });
  });

  it("opens to the left of a point the right has no room past", () => {
    expect(placeMenu({ x: 950, y: 100 }, 140, 160)).toEqual({ left: 810, top: 100, above: false });
  });

  it("keeps a box anchor's edge and gap", () => {
    const rect = { left: 600, top: 600, right: 640, bottom: 620 };
    expect(placeMenu({ rect, align: "end", gap: 4 }, 180, 100)).toEqual({ left: 460, top: 496, above: true });
    expect(placeMenu({ rect: { ...rect, top: 100, bottom: 120 }, gap: 4 }, 180, 100)).toEqual({ left: 600, top: 124, above: false });
  });
});
