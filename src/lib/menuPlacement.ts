/** How close a menu may come to the window's edge. */
const VIEWPORT_MARGIN = 8;

/**
 * What a menu opens against: the point of a right click, or a box — the control that opened it,
 * lined up with its start or end edge and kept `gap` pixels off it.
 */
export type MenuAnchor =
  | { x: number; y: number }
  | {
    rect: { left: number; top: number; right: number; bottom: number };
    align?: "start" | "end";
    gap?: number;
  };

export interface MenuPlacement {
  left: number;
  top: number;
  /** The menu stands on its anchor instead of hanging from it; its submenus grow up from their rows to match. */
  above: boolean;
}

/**
 * Where a menu goes, given where it was opened and its measured size.
 *
 * Where it was opened decides the side. In the window's upper half the menu hangs down from the
 * spot; in the lower half it stands on it, so a right click just above the composer opens over
 * the timeline rather than down into the composer. That half is always the roomier side, so there
 * is no better one to try: a menu too tall for it is held inside the window.
 *
 * Across, a point opens to its right, or to its left when only the left has room — as a desktop's
 * own context menus do. A box lines the menu up with its start or end edge.
 */
export function placeMenu(anchor: MenuAnchor, width: number, height: number): MenuPlacement {
  const viewportWidth = window.innerWidth || width;
  const viewportHeight = window.innerHeight || height;
  const box = "rect" in anchor
    ? anchor.rect
    : { left: anchor.x, top: anchor.y, right: anchor.x, bottom: anchor.y };
  const gap = "rect" in anchor ? anchor.gap ?? 0 : 0;

  const above = (box.top + box.bottom) / 2 > viewportHeight / 2;
  const preferredTop = above ? box.top - gap - height : box.bottom + gap;
  const top = Math.min(
    Math.max(preferredTop, VIEWPORT_MARGIN),
    Math.max(VIEWPORT_MARGIN, viewportHeight - height - VIEWPORT_MARGIN)
  );

  let preferredLeft: number;
  if ("rect" in anchor) {
    preferredLeft = anchor.align === "end" ? box.right - width : box.left;
  } else {
    const roomRight = anchor.x + width <= viewportWidth - VIEWPORT_MARGIN;
    preferredLeft = roomRight || anchor.x - width < VIEWPORT_MARGIN ? anchor.x : anchor.x - width;
  }
  const left = Math.min(
    Math.max(preferredLeft, VIEWPORT_MARGIN),
    Math.max(VIEWPORT_MARGIN, viewportWidth - width - VIEWPORT_MARGIN)
  );

  return { left, top, above };
}
