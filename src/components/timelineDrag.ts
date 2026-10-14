/**
 * Dragging what is picked out on the timeline to somewhere else in it.
 *
 * What is picked need not sit together: a drag takes every picked card and row,
 * however far apart, and sets them down side by side. So a drag has three looks.
 *
 * - **Gathered.** The picked pieces leave the timeline, which closes up over
 *   where they were, and ride under the pointer as one tight stack. The
 *   insertion line — the one a right-click draws — marks where they would land.
 * - **Docked.** The pieces are set down in the timeline at the line the drag
 *   began on, side by side and dimmed, the timeline opened out around them:
 *   what letting go there makes of them.
 * - **Apart.** The pieces sit where they were, dimmed, and the timeline is laid
 *   out as it was before the drag. Letting go changes nothing.
 *
 * A drag gathers the moment it begins. Where it began stays marked for as long
 * as it lasts by a line of its own, the home line: the gap the grabbed piece
 * came out of. Bringing the drag back to that line — pointing where a drop
 * would land on it — sets the pieces down there at once.
 *
 * For pieces that sat together, set down at the home line is where they were:
 * they are apart, and that is the end of it. For scattered ones the line means
 * two things, "bring them together right here" and "put them back", and it
 * offers them in that order: the pieces dock, and stay docked for
 * {@link TIMELINE_DRAG_DOCK_MS}, counted down along the line; then they go back
 * apart. Leaving the line and coming back starts the count over.
 *
 * A drag starts on the home line and is held to it until the pointer has
 * really moved off the press: closing the timeline up puts other things under
 * a pointer that has barely moved, and a drag let go there should land where it
 * began, not on whatever slid in.
 *
 * The home line is a place on the timeline, not in the window: it hangs from
 * the last thing above the grabbed piece that is not being dragged, which is on
 * screen in both looks and is held still while the timeline closes up or opens
 * out around it, so the line is where it was whichever look is showing.
 *
 * Holding it still is done by scrolling, and a timeline at its top or its foot,
 * or one too short to scroll at all, has none to give. There the anchor goes
 * with the rest: the timeline closes up over what was lifted out of it all the
 * same, leaving no room standing where a piece was. Only the stretch of screen
 * that points at the home line stays put, so that a pointer brought to the line
 * is still on it once the pieces are set down and the anchor has moved.
 *
 * Everything here is drawn straight onto the page. A drag redraws on every
 * frame, and routing that through the timeline's own render would redraw every
 * card with it; the timeline hears only where the insertion line goes and how
 * the drag ended.
 */

/** How far a press has to travel before it is a drag and not a click. */
const DRAG_THRESHOLD = 5;
/** How far past the home line's reach a drag taken apart has to go to gather again, so its edge does not flicker. */
const HOME_HOLD = 12;
/** How far from where the press was made a drag still counts as not having left the home line. */
const HOME_PIN_RADIUS = 24;
/** How long scattered pieces brought back to the home line stay docked there before they go back apart. */
export const TIMELINE_DRAG_DOCK_MS = 750;
/** How close to the top or bottom edge a drag scrolls the timeline. */
const SCROLL_EDGE = 32;
/** The most a drag scrolls the timeline per frame. */
const SCROLL_STEP = 18;
/** The most of one piece the stack under the pointer shows. */
const GHOST_PIECE_HEIGHT = 132;
/** How many pieces the stack shows; the rest are counted on a badge. */
const GHOST_PIECES = 6;
/** The gap between two pieces of the stack that are not rows of one block. */
const GHOST_GAP = 8;
const MOTION_MS = 220;
const MOTION_EASING = "cubic-bezier(.2, .75, .25, 1)";
/** How far past the visible timeline a reflow still animates what it moves. */
const REFLOW_REACH = 600;

const LIFTED = "data-timeline-lifted";
const DRAG_LOOK = "data-timeline-drag";
const DRAGGING = "data-timeline-dragging";
const UNIT_SELECTOR = ".context-card, .timeline-row, .question-history";
const COPY_STRIPPED = ["id", "data-context-id", "data-context-index", "data-context-end-index", "tabindex", LIFTED];

export type TimelineDragOutcome =
  /** The press never became a drag. */
  | { kind: "click" }
  /** Let go where it began, or called off. */
  | { kind: "cancel" }
  /** Let go with the insertion line at `index`. */
  | { kind: "drop"; index: number };

export interface TimelineDragHost {
  /** The element that scrolls the timeline. */
  scroller: HTMLElement;
  /** The column the cards and rows are laid out in. */
  content: HTMLElement;
  /** One past the last context's index: where a drop below everything lands. */
  endIndex: () => number;
  /** The z-index the stack under the pointer takes, when the timeline sits in an overlay. */
  layer?: number;
  /** The press has become a drag. */
  onStart: () => void;
  /** Where the insertion line goes now, or null while there is nowhere to drop. */
  onTarget: (index: number | null) => void;
  /** The press is over. Called once, before the page is put back in order. */
  onFinish: (outcome: TimelineDragOutcome) => void;
}

export interface TimelineDrag {
  /** Whether the press has become a drag. */
  readonly active: boolean;
  /** Calls the drag off at once, leaving the page as it was. */
  cancel: () => void;
}

/** One thing a drag lifts out of the timeline. */
interface Piece {
  /** What leaves the layout while gathered: a whole slot, or one row of a block that keeps others. */
  hidden: HTMLElement;
  /** What the stack under the pointer shows a copy of. */
  source: HTMLElement;
  /** A row lifted out of its block, which its copy still needs around it to be styled. */
  row: boolean;
  slot: Element | null;
}

interface GhostPiece {
  /** Which of the drag's pieces this is. */
  piece: number;
  element: HTMLElement;
  /** Where it sits in the stack, from the pointer. */
  x: number;
  y: number;
  height: number;
  flight: Animation | null;
}

interface Session {
  /** Everything not being dragged, in timeline order: what a drop can land between. */
  rest: HTMLElement[];
  pieces: Piece[];
  grabbed: number;
  /** The ids of every context being dragged. */
  ids: string[];
  /** Whether the pieces sit apart on the timeline, so that gathering them where they are is a move. */
  scattered: boolean;
  /** The last thing above the grabbed piece that stays put, which the home line hangs from. */
  anchor: HTMLElement | null;
  /** Where in `rest` the home line is: a drop there lands just above `rest[homePosition]`. */
  homePosition: number;
  /**
   * How far below the anchor's foot the first dragged thing under it begins, as the timeline lay
   * before the drag: where the home line sits while the pieces are apart. Kept as a distance and
   * not read off the piece, which may still be on its way back to its place.
   */
  leadBelow: number;
  /** How far above and below the anchor's foot a pointer still points at the home line. */
  reachAbove: number;
  reachBelow: number;
  /** Where the press was made: sideways in the window, and down from the anchor's foot. */
  pressX: number;
  pressBelow: number;
  /** Whether the pointer has yet to move off the press, which holds the drag to the home line. */
  pinned: boolean;
  /**
   * How far setting the pieces down has moved the anchor on screen since they were last gathered:
   * what a timeline with no scroll left could not hold it against. The home line's reach is where
   * it was measured, this far back from the anchor.
   */
  drift: number;
  /** The home line as drawn. */
  mark: HTMLElement;
  /** Where each piece sat, sideways, from the press. */
  offsetsX: number[];
  /** How far into the grabbed piece the press was made. */
  grabY: number;
  partialSlots: Set<Element>;
  look: "apart" | "docked" | "gathered";
  /** Whether the drag is at the home line: it starts there, gathered. */
  home: boolean;
  dockTimer: number | null;
  /** The pieces as set down at the home line: copies of them, in the timeline's own flow. */
  dock: { element: HTMLElement; pieces: HTMLElement[] } | null;
  /** Where a drop would land. */
  target: number | null;
  /** Where the timeline was last told to draw its insertion line: nowhere while the home line stands in for it. */
  drawn: number | null;
  ghost: { layer: HTMLElement; pieces: GhostPiece[] } | null;
  /** What a change of look is still sliding into place, each with its slide. */
  sliding: Map<HTMLElement, Animation>;
}

/**
 * The cards and rows of a timeline, in order. A prompt or a compaction is a
 * card around its one row, so the card is the unit; a context still being
 * written is not on the timeline yet.
 */
function timelineUnits(content: HTMLElement): HTMLElement[] {
  return Array.from(content.querySelectorAll<HTMLElement>(UNIT_SELECTOR)).filter((unit) => (
    unit.dataset.contextIndex !== undefined
    && !(unit.classList.contains("timeline-row") && unit.parentElement?.closest(".context-card"))
    && !unit.closest(".context-slot--inserting")
  ));
}

function unitPicked(unit: HTMLElement): boolean {
  return unit.hasAttribute("data-timeline-selected")
    || unit.querySelector(":scope > .timeline-row[data-timeline-selected]") !== null;
}

/** A copy of a card or row for the stack: looks the same, and is nothing the timeline could mistake for its own. */
function ghostCopy(source: HTMLElement): HTMLElement {
  const copy = source.cloneNode(true) as HTMLElement;
  for (const element of [copy, ...copy.querySelectorAll<HTMLElement>("*")]) {
    const id = element.getAttribute("data-context-id");
    if (id !== null) element.setAttribute("data-ghost-context-id", id);
    for (const name of COPY_STRIPPED) element.removeAttribute(name);
  }
  return copy;
}

function translate(x: number, y: number): string {
  return `translate(${x}px, ${y}px)`;
}

/**
 * Follows a press on something picked out on the timeline until it is let go:
 * a click if it never moves, a drag once it does.
 */
export function beginTimelineDrag(
  host: TimelineDragHost,
  press: { pointerId: number; x: number; y: number; target: Element }
): TimelineDrag {
  const { scroller, content } = host;
  const stillMotion = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;
  const pointer = { x: press.x, y: press.y };
  let session: Session | null = null;
  let frame: number | null = null;
  let over = false;

  const animate = (element: HTMLElement, keyframes: Keyframe[], options?: KeyframeAnimationOptions): Animation | null => (
    stillMotion || typeof element.animate !== "function"
      ? null
      : element.animate(keyframes, { duration: MOTION_MS, easing: MOTION_EASING, ...options })
  );

  /**
   * How far a slide still has `unit` from where the layout puts it. A drop lands
   * where things are going, not where they are passing through, so what a drag
   * measures the timeline by is read past the slides it started itself.
   */
  const slideOffset = (drag: Session, unit: HTMLElement): number => {
    if (!drag.sliding.size) return 0;
    const mover = drag.sliding.has(unit) ? unit : unit.closest<HTMLElement>(".context-slot");
    if (!mover || !drag.sliding.has(mover)) return 0;
    const transform = window.getComputedStyle(mover).transform;
    return transform && transform !== "none" ? new DOMMatrixReadOnly(transform).m42 : 0;
  };

  /** Where the layout puts a card or row, top and bottom, whatever is still sliding. */
  const settledSpan = (drag: Session, unit: HTMLElement): { top: number; bottom: number } => {
    const box = unit.getBoundingClientRect();
    const offset = slideOffset(drag, unit);
    return { top: box.top - offset, bottom: box.bottom - offset };
  };

  /** The edge the home line hangs from: the anchor's foot, or the top of the column. */
  const anchorEdge = (drag: Session): number => (
    drag.anchor ? settledSpan(drag, drag.anchor).bottom : content.getBoundingClientRect().top
  );

  const start = (): Session | null => {
    const units = timelineUnits(content);
    const picked = new Set(units.filter(unitPicked));
    if (!picked.size) return null;
    const pieces: Piece[] = [];
    const pieceUnits: HTMLElement[][] = [];
    const partialSlots = new Set<Element>();
    for (let from = 0; from < units.length;) {
      const slot = units[from].closest(".context-slot");
      let to = from + 1;
      while (slot && to < units.length && units[to].closest(".context-slot") === slot) to += 1;
      const group = units.slice(from, to);
      const lifted = group.filter((unit) => picked.has(unit));
      if (lifted.length === group.length && slot instanceof HTMLElement) {
        pieces.push({
          hidden: slot,
          source: slot.querySelector<HTMLElement>(":scope > .timeline-block") ?? group[0],
          row: false,
          slot
        });
        pieceUnits.push(group);
      } else {
        if (lifted.length && slot) partialSlots.add(slot);
        for (const unit of lifted) {
          pieces.push({ hidden: unit, source: unit, row: unit.classList.contains("timeline-row"), slot });
          pieceUnits.push([unit]);
        }
      }
      from = to;
    }
    const grabbed = Math.max(0, pieces.findIndex((piece) => piece.source.contains(press.target)));
    let anchorAt = units.indexOf(pieceUnits[grabbed][0]) - 1;
    while (anchorAt >= 0 && picked.has(units[anchorAt])) anchorAt -= 1;
    const anchor = anchorAt >= 0 ? units[anchorAt] : null;
    const rest = units.filter((unit) => !picked.has(unit));
    const positions = units.flatMap((unit, index) => (picked.has(unit) ? [index] : []));
    const grabbedBox = pieces[grabbed].source.getBoundingClientRect();
    const mark = document.createElement("div");
    mark.className = "timeline-drag-home";
    mark.setAttribute("aria-hidden", "true");
    mark.style.setProperty("--timeline-drag-wait", `${TIMELINE_DRAG_DOCK_MS}ms`);
    const drag: Session = {
      rest,
      pieces,
      grabbed,
      ids: [...picked].flatMap((unit) => (unit.dataset.contextId ? [unit.dataset.contextId] : [])),
      scattered: positions[positions.length - 1] - positions[0] + 1 !== positions.length,
      anchor,
      homePosition: anchor ? rest.indexOf(anchor) + 1 : 0,
      leadBelow: 0,
      reachAbove: Number.POSITIVE_INFINITY,
      reachBelow: Number.POSITIVE_INFINITY,
      pressX: press.x,
      pressBelow: 0,
      pinned: true,
      drift: 0,
      mark,
      offsetsX: pieces.map((piece) => piece.source.getBoundingClientRect().left - press.x),
      grabY: press.y - grabbedBox.top,
      partialSlots,
      look: "apart",
      home: true,
      dockTimer: null,
      dock: null,
      target: null,
      drawn: null,
      ghost: null,
      sliding: new Map()
    };
    drag.pressBelow = press.y - anchorEdge(drag);
    // Everything between the anchor and the grabbed piece is being dragged too.
    drag.leadBelow = units[anchorAt + 1].getBoundingClientRect().top - anchorEdge(drag);
    for (const piece of pieces) piece.hidden.setAttribute(LIFTED, "");
    content.setAttribute(DRAG_LOOK, "apart");
    scroller.setAttribute(DRAGGING, "");
    content.appendChild(mark);
    return drag;
  };

  /** The visible things a change of look moves, with where each is now. */
  const reflowSnapshot = (drag: Session): Map<HTMLElement, number> => {
    const view = scroller.getBoundingClientRect();
    const tops = new Map<HTMLElement, number>();
    for (const child of content.children) {
      if (!(child instanceof HTMLElement) || child === drag.mark || child === drag.dock?.element) continue;
      // A block losing some of its rows moves the rest one by one; anything else moves whole.
      const movers = drag.partialSlots.has(child)
        ? Array.from(child.querySelectorAll<HTMLElement>(".timeline-row"))
        : [child];
      for (const mover of movers) {
        // The copies docked among a block's rows are not the block's own to move.
        if (mover.hasAttribute(LIFTED) || mover.closest(".timeline-drag-dock")) continue;
        const box = mover.getBoundingClientRect();
        if (!box.width && !box.height) continue;
        if (box.bottom < view.top - REFLOW_REACH || box.top > view.bottom + REFLOW_REACH) continue;
        tops.set(mover, box.top);
      }
    }
    return tops;
  };

  /** Slides everything in `tops` from where it was to where the layout has it now. */
  const reflowFrom = (drag: Session, tops: Map<HTMLElement, number>) => {
    for (const [mover, top] of tops) {
      if (!mover.isConnected) continue;
      const shift = top - mover.getBoundingClientRect().top;
      if (Math.abs(shift) < 0.5) continue;
      const slide = animate(mover, [{ transform: `translateY(${shift}px)` }, { transform: "translateY(0)" }]);
      if (!slide) continue;
      drag.sliding.set(mover, slide);
      const done = () => {
        if (drag.sliding.get(mover) === slide) drag.sliding.delete(mover);
      };
      slide.addEventListener("finish", done);
      slide.addEventListener("cancel", done);
    }
  };

  const stopSlides = (drag: Session) => {
    const slides = [...drag.sliding.values()];
    drag.sliding.clear();
    for (const slide of slides) slide.cancel();
  };

  /**
   * Changes the layout under a slide: what is on screen is measured where it is
   * drawn now, slides that were still running included, and moves from there.
   */
  const reflow = (drag: Session, change: () => void) => {
    const tops = reflowSnapshot(drag);
    stopSlides(drag);
    change();
    reflowFrom(drag, tops);
  };

  /** Where each piece is drawn on the timeline now: its copy at the home line while docked, itself otherwise. */
  const shownBoxes = (drag: Session): DOMRect[] => (
    drag.look === "docked" && drag.dock
      ? drag.dock.pieces.map((copy) => copy.getBoundingClientRect())
      : drag.pieces.map((piece) => piece.source.getBoundingClientRect())
  );

  /**
   * Sets copies of the pieces into the timeline at the home line, in order and
   * side by side. Rows that end up next to each other share a block, as they
   * will once really moved. The copies wait there out of sight; the look shows
   * them.
   */
  const buildDock = (drag: Session): NonNullable<Session["dock"]> => {
    const element = document.createElement("div");
    element.className = "timeline-drag-dock";
    element.setAttribute("aria-hidden", "true");
    element.setAttribute("inert", "");
    let rows: HTMLElement | null = null;
    const pieces = drag.pieces.map((piece) => {
      const copy = ghostCopy(piece.source);
      if (piece.row) {
        if (!rows) {
          rows = document.createElement("div");
          rows.className = "timeline-block";
          element.appendChild(rows);
        }
        rows.appendChild(copy);
      } else {
        rows = null;
        const slot = document.createElement("div");
        slot.className = "timeline-drag-dock__slot";
        slot.appendChild(copy);
        element.appendChild(slot);
      }
      return copy;
    });
    const below = drag.rest[drag.homePosition];
    const anchorSlot = drag.anchor?.closest(".context-slot") ?? null;
    const belowSlot = below?.closest(".context-slot") ?? null;
    // Between two rows of one block the home line is inside that block; anywhere else it is between slots.
    if (below && belowSlot && belowSlot === anchorSlot) below.before(element);
    else if (below) (belowSlot ?? below).before(element);
    else if (anchorSlot) anchorSlot.after(element);
    else content.prepend(element);
    return { element, pieces };
  };

  /** Sends the pieces from where their copies sat at the home line back to their own places. */
  const scatter = (drag: Session, from: DOMRect[]) => {
    drag.pieces.forEach((piece, index) => {
      const to = piece.source.getBoundingClientRect();
      if (!to.width && !to.height) return;
      animate(piece.source, [
        { transform: translate(from[index].left - to.left, from[index].top - to.top) },
        { transform: "translate(0, 0)" }
      ]);
    });
  };

  const buildGhost = (drag: Session, boxes: DOMRect[]): NonNullable<Session["ghost"]> => {
    const layer = document.createElement("div");
    layer.className = "timeline-drag-ghost";
    layer.setAttribute("aria-hidden", "true");
    layer.setAttribute("inert", "");
    if (host.layer !== undefined) layer.style.zIndex = String(host.layer);
    const first = Math.max(0, Math.min(drag.grabbed - 2, drag.pieces.length - GHOST_PIECES));
    const shown = drag.pieces.slice(first, first + GHOST_PIECES).map((piece, offset): GhostPiece => {
      const index = first + offset;
      const element = document.createElement("div");
      element.className = "timeline-drag-ghost__piece";
      const copy = ghostCopy(piece.source);
      if (piece.row) {
        const block = document.createElement("div");
        block.className = "timeline-block timeline-drag-ghost__block";
        block.appendChild(copy);
        element.appendChild(block);
      } else {
        element.appendChild(copy);
      }
      const height = Math.min(boxes[index].height, GHOST_PIECE_HEIGHT);
      if (height < boxes[index].height) element.setAttribute("data-clipped", "");
      element.style.width = `${boxes[index].width}px`;
      layer.appendChild(element);
      return { piece: index, element, x: drag.offsetsX[index], y: 0, height, flight: null };
    });
    // The stack hangs from the grabbed piece, which keeps the pointer where the press took hold of it.
    const held = shown.findIndex((entry) => entry.piece === drag.grabbed);
    const gapBetween = (upper: GhostPiece, lower: GhostPiece) => {
      const above = drag.pieces[upper.piece];
      const below = drag.pieces[lower.piece];
      return above.row && below.row && above.slot === below.slot ? 0 : GHOST_GAP;
    };
    shown[held].y = -Math.max(0, Math.min(drag.grabY, shown[held].height - 12));
    for (let index = held + 1; index < shown.length; index += 1) {
      shown[index].y = shown[index - 1].y + shown[index - 1].height + gapBetween(shown[index - 1], shown[index]);
    }
    for (let index = held - 1; index >= 0; index -= 1) {
      shown[index].y = shown[index + 1].y - gapBetween(shown[index], shown[index + 1]) - shown[index].height;
    }
    if (drag.pieces.length > shown.length) {
      const last = shown[shown.length - 1];
      const more = document.createElement("div");
      more.className = "timeline-drag-ghost__more";
      more.textContent = `+${drag.pieces.length - shown.length}`;
      more.style.transform = translate(last.x, last.y + last.height + 6);
      layer.appendChild(more);
    }
    document.body.appendChild(layer);
    return { layer, pieces: shown };
  };

  const placeGhost = (drag: Session) => {
    if (drag.ghost) drag.ghost.layer.style.transform = translate(pointer.x, pointer.y);
  };

  /** Brings the stack out from where the pieces sit on the timeline. */
  const gatherGhost = (drag: Session, boxes: DOMRect[]) => {
    const fresh = !drag.ghost;
    const ghost = drag.ghost ?? buildGhost(drag, boxes);
    drag.ghost = ghost;
    placeGhost(drag);
    for (const entry of ghost.pieces) {
      // A piece still on its way home turns round from wherever it has got to.
      const from = fresh || !entry.flight ? boxes[entry.piece] : entry.element.getBoundingClientRect();
      entry.flight?.cancel();
      entry.element.style.transform = translate(entry.x, entry.y);
      entry.element.style.height = `${entry.height}px`;
      entry.flight = animate(entry.element, [
        { transform: translate(from.left - pointer.x, from.top - pointer.y), height: `${from.height}px` },
        { transform: translate(entry.x, entry.y), height: `${entry.height}px` }
      ]);
    }
    ghost.layer.style.visibility = "visible";
  };

  /** Sends the stack onto the pieces, which are on the timeline again under it: themselves, or their copies at the home line. */
  const partGhost = (drag: Session, boxes: DOMRect[]) => {
    const ghost = drag.ghost;
    if (!ghost) return;
    let last: Animation | null = null;
    for (const entry of ghost.pieces) {
      const from = entry.element.getBoundingClientRect();
      const to = boxes[entry.piece];
      entry.flight?.cancel();
      entry.flight = animate(entry.element, [
        { transform: translate(from.left - pointer.x, from.top - pointer.y), height: `${from.height}px`, opacity: 1 },
        { transform: translate(to.left - pointer.x, to.top - pointer.y), height: `${to.height}px`, opacity: 0 }
      ], { fill: "forwards" });
      last = entry.flight ?? last;
    }
    const hide = () => {
      if (session === drag && drag.look !== "gathered") ghost.layer.style.visibility = "hidden";
    };
    if (last) last.addEventListener("finish", hide);
    else hide();
  };

  /**
   * Changes the layout with the anchor, and the home line with it, held where
   * it is on screen: whatever closes up or opens out above it is scrolled away
   * again. A timeline at its top or its foot has nothing left to scroll, and
   * there the anchor moves with everything else — the column is given no room
   * of its own to stand in for what left it. How far the anchor moved is handed
   * back.
   */
  const holdAnchor = (drag: Session, change: () => void): number => {
    const before = anchorEdge(drag);
    change();
    const shift = anchorEdge(drag) - before;
    if (shift) scroller.scrollTop += shift;
    const moved = anchorEdge(drag) - before;
    return Math.abs(moved) < 1 ? 0 : moved;
  };

  const setLook = (drag: Session, look: Session["look"]) => {
    const from = drag.look;
    if (from === look) return;
    // Wherever the pieces are drawn now is where what stands in for them next sets out from.
    const leaving = shownBoxes(drag);
    if (look === "docked") drag.dock ??= buildDock(drag);
    let moved = 0;
    reflow(drag, () => {
      moved = holdAnchor(drag, () => content.setAttribute(DRAG_LOOK, look));
    });
    // Gathered, the home line's reach is measured afresh; set down, it stays where it was measured.
    drag.drift = look === "gathered" ? 0 : drag.drift + moved;
    // The press was made on the screen, not on an anchor that has since moved under it.
    if (drag.pinned) drag.pressBelow -= moved;
    drag.look = look;
    if (look === "gathered") gatherGhost(drag, leaving);
    else if (from === "gathered") partGhost(drag, shownBoxes(drag));
    else if (look === "apart") scatter(drag, leaving);
  };

  /** Notes where a drop would land, and has the timeline draw its insertion line at `drawn`. */
  const setTarget = (drag: Session, index: number | null, drawn: number | null = index) => {
    drag.target = index;
    if (drag.drawn === drawn) return;
    drag.drawn = drawn;
    host.onTarget(drawn);
  };

  /** The first thing staying put whose middle is below `y`: a drop lands just above it. */
  const restBelow = (drag: Session, y: number): number => {
    let low = 0;
    let high = drag.rest.length;
    while (low < high) {
      const middle = (low + high) >> 1;
      const span = settledSpan(drag, drag.rest[middle]);
      if ((span.top + span.bottom) / 2 > y) high = middle;
      else low = middle + 1;
    }
    return low;
  };

  /**
   * Where a drop at `y` lands: above the first thing staying put whose middle is
   * below the pointer. Over a card too tall for both its ends to be on screen,
   * that can put the line out of sight, so the end that can be seen is taken
   * instead: the line is what says where a drop goes. The strips a drag scrolls
   * from do not count as in sight — the foot of the timeline lies under the
   * composer's fade, and holding the pointer there brings more into view anyway.
   */
  /** A drop above `rest[position]`, as the index the timeline knows it by. */
  const dropAbove = (drag: Session, position: number): { index: number; position: number } => {
    const index = position < drag.rest.length ? Number(drag.rest[position].dataset.contextIndex) : Number.NaN;
    return { index: Number.isInteger(index) ? index : host.endIndex(), position };
  };

  /** A drop on the home line. */
  const homeDrop = (drag: Session) => dropAbove(drag, drag.homePosition);

  const targetAt = (drag: Session, y: number): { index: number; position: number } => {
    const { rest } = drag;
    const view = scroller.getBoundingClientRect();
    const lineAt = (position: number): number => (
      position < rest.length
        ? settledSpan(drag, rest[position]).top
        : rest.length ? settledSpan(drag, rest[rest.length - 1]).bottom : view.top
    );
    const sightTop = view.top + SCROLL_EDGE;
    const sightBottom = view.bottom - SCROLL_EDGE;
    const inSight = (position: number): boolean => {
      if (position < 0 || position > rest.length) return false;
      const line = lineAt(position);
      return line >= sightTop && line <= sightBottom;
    };
    let below = restBelow(drag, y);
    if (!inSight(below)) {
      const otherEnd = lineAt(below) > sightBottom ? below - 1 : below + 1;
      if (inSight(otherEnd)) below = otherEnd;
    }
    return dropAbove(drag, below);
  };

  /**
   * Whether the pointer points at the home line: whether a drop here would land
   * on it, as the timeline lies while gathered. That reach is measured whenever
   * the pieces are gathered and kept as distances from the anchor, so it is the
   * same stretch of screen once they are set down and the timeline around it
   * has opened out again — even where the anchor could not be held and went
   * with it: a reach that followed the anchor there would leave the pointer
   * behind, gather the pieces, and be brought straight back under it.
   */
  const pointsHome = (drag: Session): boolean => {
    const edge = anchorEdge(drag) - (drag.look === "gathered" ? 0 : drag.drift);
    if (drag.look === "gathered") {
      const middle = (unit: HTMLElement) => {
        const span = settledSpan(drag, unit);
        return (span.top + span.bottom) / 2;
      };
      const below = drag.rest[drag.homePosition];
      drag.reachAbove = drag.anchor ? edge - middle(drag.anchor) : Number.POSITIVE_INFINITY;
      drag.reachBelow = below ? middle(below) - edge : Number.POSITIVE_INFINITY;
    }
    const hold = drag.look === "gathered" ? 0 : HOME_HOLD;
    return pointer.y >= edge - drag.reachAbove - hold && pointer.y < edge + drag.reachBelow + hold;
  };

  /**
   * Draws the home line where the drag began: above whatever follows the anchor
   * as the timeline lies now — the first of the pieces while they are apart,
   * their copies while they are docked, the next thing staying put while they
   * are gathered.
   */
  const placeMark = (drag: Session) => {
    const column = content.getBoundingClientRect();
    const below = drag.rest[drag.homePosition];
    const line = drag.look === "apart"
      ? anchorEdge(drag) + drag.leadBelow
      : drag.look === "docked" && drag.dock
        ? drag.dock.element.getBoundingClientRect().top
        : below
          ? settledSpan(drag, below).top
          : drag.anchor ? settledSpan(drag, drag.anchor).bottom + 2 : column.top + 2;
    drag.mark.style.top = `${Math.round(line - column.top) - 2}px`;
  };

  const stopDockWait = (drag: Session) => {
    if (drag.dockTimer !== null) window.clearTimeout(drag.dockTimer);
    drag.dockTimer = null;
  };

  /**
   * Brings what is drawn up to the look and the pointer: the stack and the
   * insertion line while gathered, and the home line always — counting down
   * while the pieces are docked on it, lit while a drop would land on it or the
   * pieces are back apart at it, and faint as a mark of where the drag began
   * otherwise. A drop on the home line is shown by that line alone: the
   * timeline draws none of its own there.
   */
  const present = (drag: Session) => {
    if (drag.look === "gathered") {
      placeGhost(drag);
      const drop = drag.pinned ? homeDrop(drag) : targetAt(drag, pointer.y);
      const onHome = drop.position === drag.homePosition;
      setTarget(drag, drop.index, onHome ? null : drop.index);
      drag.mark.setAttribute("data-state", onHome ? "at" : "away");
    } else if (drag.look === "docked") {
      // Let go now, the pieces stay as they are shown: together, at the home line.
      setTarget(drag, homeDrop(drag).index, null);
      drag.mark.setAttribute("data-state", "counting");
    } else {
      setTarget(drag, null);
      drag.mark.setAttribute("data-state", "at");
    }
    placeMark(drag);
  };

  /** Starts the count after which pieces docked at the home line go back apart. */
  const startDockWait = (drag: Session) => {
    stopDockWait(drag);
    drag.dockTimer = window.setTimeout(() => {
      drag.dockTimer = null;
      if (over || session !== drag || drag.look !== "docked") return;
      setLook(drag, "apart");
      present(drag);
    }, TIMELINE_DRAG_DOCK_MS);
  };

  const schedule = () => {
    if (frame === null && !over) frame = window.requestAnimationFrame(() => step());
  };

  /**
   * Brings the drag up to where the pointer is: scrolls the timeline on when
   * held against its top or bottom edge, sets the pieces down when the pointer
   * comes to the home line and gathers them when it leaves, and moves the stack
   * and the lines.
   */
  const step = (final = false) => {
    frame = null;
    const drag = session;
    if (!drag || over) return;
    let scrolled = false;
    if (!final) {
      const bounds = scroller.getBoundingClientRect();
      const stride = pointer.y < bounds.top + SCROLL_EDGE
        ? -Math.min(SCROLL_STEP, bounds.top + SCROLL_EDGE - pointer.y)
        : pointer.y > bounds.bottom - SCROLL_EDGE
          ? Math.min(SCROLL_STEP, pointer.y - (bounds.bottom - SCROLL_EDGE))
          : 0;
      if (stride) {
        const before = scroller.scrollTop;
        scroller.scrollTop = before + stride;
        scrolled = scroller.scrollTop !== before;
      }
    }
    if (drag.pinned) {
      drag.pinned = Math.hypot(pointer.x - drag.pressX, pointer.y - (anchorEdge(drag) + drag.pressBelow)) <= HOME_PIN_RADIUS;
    }
    // Measured even while the pin answers for it: the reach is what the line has once the pieces are set down.
    const home = pointsHome(drag) || drag.pinned;
    if (home !== drag.home) {
      drag.home = home;
      stopDockWait(drag);
      if (!home) {
        setLook(drag, "gathered");
      } else if (drag.scattered) {
        setLook(drag, "docked");
        startDockWait(drag);
      } else {
        // Pieces that sat together are, set down at the home line, simply where they were.
        setLook(drag, "apart");
      }
    }
    present(drag);
    if (scrolled) schedule();
  };

  /**
   * Puts the page back in order once the timeline has redrawn for however the
   * drag ended: what stayed slides to where it is now, and what was dragged
   * lands from where its copy was, in the stack or at the home line.
   */
  const settle = (drag: Session, tops: Map<HTMLElement, number>, held: Map<string, DOMRect>, steady: HTMLElement | null) => {
    stopSlides(drag);
    const steadyBefore = steady?.isConnected ? steady.getBoundingClientRect().bottom : null;
    for (const piece of drag.pieces) piece.hidden.removeAttribute(LIFTED);
    content.removeAttribute(DRAG_LOOK);
    drag.mark.remove();
    drag.dock?.element.remove();
    if (steady?.isConnected && steadyBefore !== null) {
      const shift = steady.getBoundingClientRect().bottom - steadyBefore;
      if (shift) scroller.scrollTop += shift;
    }
    scroller.removeAttribute(DRAGGING);
    drag.ghost?.layer.remove();
    reflowFrom(drag, tops);
    if (!held.size) return;
    const landing = new Set(held.keys());
    for (const element of content.querySelectorAll<HTMLElement>("[data-context-id]")) {
      const id = element.dataset.contextId;
      if (!id || !landing.delete(id)) continue;
      const from = held.get(id)!;
      const to = element.getBoundingClientRect();
      animate(element, [
        { transform: translate(from.left - to.left, from.top - to.top), opacity: 0.9 },
        { transform: "translate(0, 0)", opacity: 1 }
      ]);
    }
  };

  const listeners: Array<() => void> = [];
  const listen = <Name extends keyof WindowEventMap>(
    target: Window | HTMLElement,
    name: Name,
    handler: (event: WindowEventMap[Name]) => void,
    options?: AddEventListenerOptions | boolean
  ) => {
    target.addEventListener(name, handler as EventListener, options);
    listeners.push(() => target.removeEventListener(name, handler as EventListener, options));
  };

  const end = (outcome: TimelineDragOutcome, immediately = false) => {
    if (over) return;
    over = true;
    if (frame !== null) window.cancelAnimationFrame(frame);
    frame = null;
    for (const remove of listeners.splice(0)) remove();
    const drag = session;
    if (!drag) {
      host.onFinish(outcome);
      return;
    }
    stopDockWait(drag);
    // The click a drag ends in comes right after the release, in the same turn; nothing later is its.
    const swallow = (event: Event) => {
      event.preventDefault();
      event.stopPropagation();
    };
    window.addEventListener("click", swallow, true);
    window.setTimeout(() => window.removeEventListener("click", swallow, true), 0);
    const tops = drag.look === "apart" ? new Map<HTMLElement, number>() : reflowSnapshot(drag);
    // What stands in for the pieces while they are out of their places: the stack, or their copies at the home line.
    const stand = drag.look === "gathered" ? drag.ghost?.layer : drag.look === "docked" ? drag.dock?.element : null;
    const held = new Map<string, DOMRect>();
    for (const copy of stand?.querySelectorAll<HTMLElement>("[data-ghost-context-id]") ?? []) {
      const id = copy.getAttribute("data-ghost-context-id")!;
      if (!held.has(id)) held.set(id, copy.getBoundingClientRect());
    }
    // Whatever sits just above the drop stays where it is while the rest falls into place.
    const steady = drag.look === "gathered"
      ? drag.rest[restBelow(drag, pointer.y) - 1] ?? null
      : drag.look === "docked" ? drag.anchor : null;
    host.onFinish(outcome);
    if (immediately) settle(drag, new Map(), new Map(), null);
    else window.requestAnimationFrame(() => settle(drag, tops, held, steady));
  };

  listen(window, "pointermove", (event) => {
    if (event.pointerId !== press.pointerId) return;
    pointer.x = event.clientX;
    pointer.y = event.clientY;
    if (!session) {
      if (Math.hypot(pointer.x - press.x, pointer.y - press.y) < DRAG_THRESHOLD) return;
      session = start();
      if (!session) {
        end({ kind: "click" });
        return;
      }
      host.onStart();
      // A drag gathers as it begins: it is at the home line, with nothing left to count down.
      setLook(session, "gathered");
      present(session);
    }
    schedule();
  }, true);
  const release = (event: PointerEvent) => {
    if (event.pointerId !== press.pointerId) return;
    pointer.x = event.clientX;
    pointer.y = event.clientY;
    if (!session) {
      end({ kind: event.type === "pointerup" ? "click" : "cancel" });
      return;
    }
    if (event.type !== "pointerup") {
      end({ kind: "cancel" });
      return;
    }
    step(true);
    end(session.look !== "apart" && session.target !== null
      ? { kind: "drop", index: session.target }
      : { kind: "cancel" });
  };
  listen(window, "pointerup", release, true);
  listen(window, "pointercancel", release, true);
  listen(window, "keydown", (event) => {
    if (event.key !== "Escape" || !session) return;
    event.preventDefault();
    event.stopPropagation();
    end({ kind: "cancel" });
  }, true);
  // Wheeling the timeline under a still pointer moves what the pointer is over.
  listen(scroller, "scroll", () => {
    if (session) schedule();
  }, { passive: true });
  const prevent = (event: Event) => event.preventDefault();
  listen(window, "selectstart", prevent, true);
  listen(window, "dragstart", prevent, true);

  return {
    get active() {
      return session !== null && !over;
    },
    cancel: () => end({ kind: "cancel" }, true)
  };
}
