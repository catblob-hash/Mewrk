import { describe, expect, it } from "vitest";
import type { ContextItem } from "../types";
import {
  EMPTY_TIMELINE_PATCH,
  TimelineHistory,
  applyTimelinePatch,
  insertionPatch,
  invertTimelinePatch,
  movePatch,
  pastePatch,
  placedContexts,
  timelineClipboardKey,
  timelineClipboardShortcut,
  timelineHistoryKey
} from "./timelineHistory";

const message = (id: string, content = id): ContextItem => ({
  id,
  kind: "user",
  content,
  createdAt: "2026-10-02T00:00:00.000Z"
});

const ids = (contexts: ContextItem[] | null) => contexts?.map((context) => context.id) ?? null;

describe("timeline patches", () => {
  it("takes contexts out and puts them back where they were", () => {
    const list = ["a", "b", "c", "d"].map((id) => message(id));
    const patch = { ...EMPTY_TIMELINE_PATCH, removed: placedContexts(list, new Set(["b", "c"])) };

    const deleted = applyTimelinePatch(list, patch);
    expect(ids(deleted)).toEqual(["a", "d"]);
    expect(ids(applyTimelinePatch(deleted!, invertTimelinePatch(patch)))).toEqual(["a", "b", "c", "d"]);
  });

  it("puts a context back after the neighbour it had, wherever that has moved since", () => {
    const list = ["a", "b", "c"].map((id) => message(id));
    const patch = { ...EMPTY_TIMELINE_PATCH, removed: placedContexts(list, new Set(["c"])) };
    // Two contexts went in above it after it was deleted, so its old index is wrong now.
    const later = [message("x"), message("y"), message("a"), message("b"), message("reply")];

    expect(ids(applyTimelinePatch(later, invertTimelinePatch(patch)))).toEqual(["x", "y", "a", "b", "c", "reply"]);
  });

  it("keeps what a model run appended between an edit and its undo", () => {
    const list = [message("a")];
    const inserted = insertionPatch(list, message("placed"), 1);
    const afterRun = [...applyTimelinePatch(list, inserted)!, message("reply")];

    expect(ids(applyTimelinePatch(afterRun, invertTimelinePatch(inserted)))).toEqual(["a", "reply"]);
  });

  it("rewrites a context in place in both directions", () => {
    const before = message("a", "old");
    const after = message("a", "new");
    const patch = { ...EMPTY_TIMELINE_PATCH, replaced: [{ before, after }] };

    expect(applyTimelinePatch([before, message("b")], patch)?.[0]).toBe(after);
    expect(applyTimelinePatch([after, message("b")], invertTimelinePatch(patch))?.[0]).toBe(before);
  });

  it("lands nowhere once nothing it touches is there to change", () => {
    const patch = { ...EMPTY_TIMELINE_PATCH, removed: placedContexts([message("gone")], new Set(["gone"])) };
    expect(applyTimelinePatch([message("other")], patch)).toBeNull();
  });
});

describe("TimelineHistory", () => {
  const entry = (label: string) => ({ patch: EMPTY_TIMELINE_PATCH, label });

  it("keeps each timeline's edits apart", () => {
    const history = new TimelineHistory();
    history.record("conv_a", entry("a1"));
    history.record("conv_b", entry("b1"));

    expect(history.nextUndo("conv_a")?.label).toBe("a1");
    expect(history.nextUndo("conv_b")?.label).toBe("b1");
    // A fork or a hand-over is a new id, and a new id has nothing on record.
    expect(history.nextUndo("conv_a_fork")).toBeNull();
  });

  it("moves an edit between undo and redo, and a new edit ends the redo", () => {
    const history = new TimelineHistory();
    history.record("conv", entry("first"));
    history.record("conv", entry("second"));

    history.undone("conv");
    expect(history.nextUndo("conv")?.label).toBe("first");
    expect(history.nextRedo("conv")?.label).toBe("second");
    history.redone("conv");
    expect(history.nextUndo("conv")?.label).toBe("second");
    expect(history.nextRedo("conv")).toBeNull();

    history.undone("conv");
    history.record("conv", entry("third"));
    expect(history.nextRedo("conv")).toBeNull();
  });

  it("shares one budget across timelines and lets the oldest edit go first", () => {
    const history = new TimelineHistory(3);
    history.record("conv_a", entry("a1"));
    history.record("conv_b", entry("b1"));
    history.record("conv_a", entry("a2"));
    history.record("conv_b", entry("b2"));

    expect(history.size).toBe(3);
    history.undone("conv_a");
    history.undone("conv_a");
    // "a1" was the oldest of all, so it went; "a2" is still there to undo and redo.
    expect(history.nextUndo("conv_a")).toBeNull();
    expect(history.nextRedo("conv_a")?.label).toBe("a2");
    expect(history.nextUndo("conv_b")?.label).toBe("b2");
  });

  it("counts what is waiting to be redone against the budget too", () => {
    const history = new TimelineHistory(2);
    history.record("conv_a", entry("a1"));
    history.undone("conv_a");
    history.record("conv_b", entry("b1"));
    history.record("conv_b", entry("b2"));

    expect(history.size).toBe(2);
    expect(history.nextRedo("conv_a")).toBeNull();
  });

  it("forgets a timeline outright", () => {
    const history = new TimelineHistory();
    history.record("conv", entry("a"));
    history.forget("conv");
    expect(history.nextUndo("conv")).toBeNull();
    expect(history.size).toBe(0);
  });
});

const press = (key: string, modifiers: Partial<Record<"ctrlKey" | "metaKey" | "altKey" | "shiftKey", boolean>> = {}) => ({
  key,
  ctrlKey: false,
  metaKey: false,
  altKey: false,
  shiftKey: false,
  ...modifiers
});

describe("timelineHistoryKey", () => {
  it("reads Ctrl+Z as undo and Ctrl+X as redo, and nothing else", () => {
    expect(timelineHistoryKey(press("z", { ctrlKey: true }))).toBe("undo");
    expect(timelineHistoryKey(press("X", { ctrlKey: true }))).toBe("redo");
    expect(timelineHistoryKey(press("z"))).toBeNull();
    expect(timelineHistoryKey(press("z", { ctrlKey: true, shiftKey: true }))).toBeNull();
    expect(timelineHistoryKey(press("c", { ctrlKey: true }))).toBeNull();
  });
});

describe("timelineClipboardKey", () => {
  // jsdom's navigator.platform is not a Mac, so Ctrl is the primary modifier here.
  it("reads Ctrl+C as copy and Ctrl+V as paste, in either case", () => {
    expect(timelineClipboardKey(press("c", { ctrlKey: true }))).toBe("copy");
    expect(timelineClipboardKey(press("v", { ctrlKey: true }))).toBe("paste");
    expect(timelineClipboardKey(press("C", { ctrlKey: true }))).toBe("copy");
    expect(timelineClipboardKey(press("V", { ctrlKey: true }))).toBe("paste");
  });

  it("ignores a press with Alt or Shift held, without the modifier, or on another letter", () => {
    expect(timelineClipboardKey(press("c", { ctrlKey: true, altKey: true }))).toBeNull();
    expect(timelineClipboardKey(press("v", { ctrlKey: true, altKey: true }))).toBeNull();
    expect(timelineClipboardKey(press("c", { ctrlKey: true, shiftKey: true }))).toBeNull();
    expect(timelineClipboardKey(press("v", { ctrlKey: true, shiftKey: true }))).toBeNull();
    expect(timelineClipboardKey(press("c"))).toBeNull();
    expect(timelineClipboardKey(press("v"))).toBeNull();
    expect(timelineClipboardKey(press("z", { ctrlKey: true }))).toBeNull();
    expect(timelineClipboardKey(press("x", { ctrlKey: true }))).toBeNull();
  });

  it("does not take ⌘ for the modifier off a Mac, nor Ctrl and ⌘ together", () => {
    expect(timelineClipboardKey(press("c", { metaKey: true }))).toBeNull();
    expect(timelineClipboardKey(press("v", { metaKey: true }))).toBeNull();
    expect(timelineClipboardKey(press("c", { ctrlKey: true, metaKey: true }))).toBeNull();
  });

  it("writes the keycaps as this platform does", () => {
    expect(timelineClipboardShortcut("copy")).toBe("Ctrl+C");
    expect(timelineClipboardShortcut("paste")).toBe("Ctrl+V");
  });
});

describe("moving and pasting", () => {
  const abcde = () => ["a", "b", "c", "d", "e"].map((id) => message(id));
  /** The order a move leaves, or null when it is no change at all. */
  const moved = (list: ContextItem[], picked: string[], index: number) => {
    const patch = movePatch(list, new Set(picked), index);
    return patch ? ids(applyTimelinePatch(list, patch)) : null;
  };

  describe("movePatch", () => {
    it("gathers scattered contexts together, in list order, at the insertion index", () => {
      expect(moved(abcde(), ["a", "d"], 3)).toEqual(["b", "c", "a", "d", "e"]);
      expect(moved(abcde(), ["b", "e"], 0)).toEqual(["b", "e", "a", "c", "d"]);
      expect(moved(abcde(), ["a"], 5)).toEqual(["b", "c", "d", "e", "a"]);
    });

    it("keeps list order among the moved contexts whatever order the ids were picked in", () => {
      expect(moved(abcde(), ["d", "a"], 3)).toEqual(["b", "c", "a", "d", "e"]);
    });

    it("reads the index as an insertion line in the list as it was before the move", () => {
      // The line before "e": "a" and "b" sit above it and leave, so it ends up past "c" and "d".
      expect(moved(abcde(), ["a", "b"], 4)).toEqual(["c", "d", "a", "b", "e"]);
      expect(moved(abcde(), ["a"], 1)).toBeNull();
      expect(moved(abcde(), ["a"], 2)).toEqual(["b", "a", "c", "d", "e"]);
      expect(moved(abcde(), ["e"], 0)).toEqual(["e", "a", "b", "c", "d"]);
    });

    it("clamps an index outside the list to its ends", () => {
      expect(moved(abcde(), ["b"], 99)).toEqual(["a", "c", "d", "e", "b"]);
      expect(moved(abcde(), ["d"], -3)).toEqual(["d", "a", "b", "c", "e"]);
    });

    it("is null when nothing would change", () => {
      // "b" and "c" are already together, and these indexes sit at or inside their run.
      for (const index of [1, 2, 3]) expect(movePatch(abcde(), new Set(["b", "c"]), index)).toBeNull();
      expect(movePatch(abcde(), new Set(), 2)).toBeNull();
      expect(movePatch(abcde(), new Set(["nope", "gone"]), 2)).toBeNull();
      expect(movePatch([], new Set(["a"]), 0)).toBeNull();
    });

    it("is not null when scattered contexts are moved to where one of them already sits", () => {
      // Gathering "a" and "c" is a change even though "a" does not move.
      expect(moved(abcde(), ["a", "c"], 0)).toEqual(["a", "c", "b", "d", "e"]);
    });

    it("ignores ids that are not in the list and moves the rest", () => {
      expect(moved(abcde(), ["a", "nope"], 5)).toEqual(["b", "c", "d", "e", "a"]);
    });

    it("keeps the very same context objects", () => {
      const list = abcde();
      const patch = movePatch(list, new Set(["a", "d"]), 3)!;
      const after = applyTimelinePatch(list, patch)!;

      for (const context of after) expect(context).toBe(list.find((original) => original.id === context.id));
    });

    it("is undone by the patch read the other way, and redone by applying it again", () => {
      for (const [picked, index, order] of [
        [["a", "d"], 3, ["b", "c", "a", "d", "e"]],
        [["b", "e"], 0, ["b", "e", "a", "c", "d"]],
        [["a", "c"], 0, ["a", "c", "b", "d", "e"]],
        [["a"], 5, ["b", "c", "d", "e", "a"]],
        [["a", "b"], 4, ["c", "d", "a", "b", "e"]]
      ] as const) {
        const list = abcde();
        const patch = movePatch(list, new Set(picked), index)!;
        const after = applyTimelinePatch(list, patch)!;
        expect(ids(after)).toEqual(order);

        const restored = applyTimelinePatch(after, invertTimelinePatch(patch))!;
        expect(restored.map((context) => context.id)).toEqual(["a", "b", "c", "d", "e"]);
        for (const [position, context] of restored.entries()) expect(context).toBe(list[position]);

        expect(ids(applyTimelinePatch(restored, patch))).toEqual(order);
      }
    });

    it("keeps what a model run appended between a move and its undo", () => {
      const list = abcde();
      const patch = movePatch(list, new Set(["a", "d"]), 3)!;
      const afterRun = [...applyTimelinePatch(list, patch)!, message("f")];

      const undone = applyTimelinePatch(afterRun, invertTimelinePatch(patch))!;
      expect(ids(undone)).toEqual(["a", "b", "c", "d", "e", "f"]);
      expect(ids(applyTimelinePatch(undone, patch))).toEqual(["b", "c", "a", "d", "e", "f"]);
    });
  });

  describe("pastePatch", () => {
    const abc = () => ["a", "b", "c"].map((id) => message(id));
    const pasted = () => [message("x"), message("y")];
    const pastedOrder = (list: ContextItem[], put: ContextItem[], index: number, replaced?: string[]) => {
      const patch = pastePatch(list, put, index, replaced ? new Set(replaced) : undefined);
      return patch ? ids(applyTimelinePatch(list, patch)) : null;
    };

    it("puts the pasted contexts where the index points", () => {
      expect(pastedOrder(abc(), pasted(), 2)).toEqual(["a", "b", "x", "y", "c"]);
      expect(pastedOrder(abc(), pasted(), 0)).toEqual(["x", "y", "a", "b", "c"]);
      expect(pastedOrder(abc(), pasted(), 3)).toEqual(["a", "b", "c", "x", "y"]);
    });

    it("clamps an index outside the list to its ends", () => {
      expect(pastedOrder(abc(), pasted(), 99)).toEqual(["a", "b", "c", "x", "y"]);
      expect(pastedOrder(abc(), pasted(), -4)).toEqual(["x", "y", "a", "b", "c"]);
      expect(pastedOrder([], pasted(), 5)).toEqual(["x", "y"]);
    });

    it("is null when nothing was pasted", () => {
      expect(pastePatch(abc(), [], 1)).toBeNull();
      expect(pastePatch(abc(), [], 1, new Set(["b"]))).toBeNull();
    });

    it("puts in exactly the pasted objects and takes nothing out", () => {
      const list = abc();
      const put = pasted();
      const patch = pastePatch(list, put, 1)!;

      expect(patch.removed).toEqual([]);
      expect(patch.replaced).toEqual([]);
      expect(patch.inserted.map((entry) => entry.context)).toEqual(put);
      expect(patch.inserted[0].context).toBe(put[0]);
      expect(patch.inserted[1].context).toBe(put[1]);
    });

    it("is undone by the patch read the other way, and redone by applying it again", () => {
      const list = abc();
      const patch = pastePatch(list, pasted(), 1)!;
      const after = applyTimelinePatch(list, patch)!;
      expect(ids(after)).toEqual(["a", "x", "y", "b", "c"]);

      const restored = applyTimelinePatch(after, invertTimelinePatch(patch))!;
      expect(ids(restored)).toEqual(["a", "b", "c"]);
      expect(ids(applyTimelinePatch(restored, patch))).toEqual(["a", "x", "y", "b", "c"]);
    });

    describe("replacing contexts", () => {
      const abcd = () => ["a", "b", "c", "d"].map((id) => message(id));

      it("puts a contiguous run's replacement where the run sat, whatever the index", () => {
        for (const index of [0, 1, 2, 3, 4, 99]) {
          expect(pastedOrder(abcd(), pasted(), index, ["b", "c"])).toEqual(["a", "x", "y", "d"]);
        }
      });

      it("puts scattered contexts' replacement where the first of them sat, and just removes the rest", () => {
        for (const index of [0, 2, 4]) {
          expect(pastedOrder(abcd(), pasted(), index, ["a", "c"])).toEqual(["x", "y", "b", "d"]);
        }
        expect(pastedOrder(abcd(), pasted(), 0, ["b", "d"])).toEqual(["a", "x", "y", "c"]);
      });

      it("uses the index when none of the ids to replace is in the list", () => {
        expect(pastedOrder(abcd(), pasted(), 2, ["gone"])).toEqual(["a", "b", "x", "y", "c", "d"]);
      });

      it("takes out the contexts it replaces as they sat", () => {
        const list = abcd();
        const patch = pastePatch(list, pasted(), 0, new Set(["a", "c"]))!;

        expect(patch.removed.map((entry) => entry.context.id)).toEqual(["a", "c"]);
        expect(patch.removed[0].context).toBe(list[0]);
        expect(patch.removed[1].context).toBe(list[2]);
      });

      it("is undone to the exact original list, and redone by applying it again", () => {
        for (const replaced of [["b", "c"], ["a", "c"]]) {
          const list = abcd();
          const patch = pastePatch(list, pasted(), 3, new Set(replaced))!;
          const after = applyTimelinePatch(list, patch)!;

          const restored = applyTimelinePatch(after, invertTimelinePatch(patch))!;
          expect(ids(restored)).toEqual(["a", "b", "c", "d"]);
          for (const [position, context] of restored.entries()) expect(context).toBe(list[position]);

          expect(ids(applyTimelinePatch(restored, patch))).toEqual(ids(after));
        }
      });

      it("replaces everything when every context is picked", () => {
        expect(pastedOrder(abcd(), pasted(), 2, ["a", "b", "c", "d"])).toEqual(["x", "y"]);
      });
    });
  });
});
