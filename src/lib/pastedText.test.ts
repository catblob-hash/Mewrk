import { describe, expect, it } from "vitest";
import {
  adoptDraftClipboard,
  draftClipboard,
  expandPastedTexts,
  foldsPastedText,
  nextPastedTextId,
  normalizePastedText,
  parseDraftClipboard,
  pastedTextExtraLines,
  pastedTextIdMark,
  pastedTextRangeAround,
  pastedTextRanges,
  type PastedText,
  type PastedTextLabeler
} from "./pastedText";

const labelFor: PastedTextLabeler = (id, extraLines) => (
  extraLines ? `[Pasted text #${id} +${extraLines} lines]` : `[Pasted text #${id}]`
);
const paste = (id: number, content: string): PastedText => ({
  id,
  label: labelFor(id, pastedTextExtraLines(content)),
  content
});

describe("pasted text", () => {
  it("folds only a paste longer than 800 characters, as Claude Code does", () => {
    expect(foldsPastedText("a".repeat(800))).toBe(false);
    expect(foldsPastedText("a".repeat(801))).toBe(true);
    // Line count alone does not fold: short lines stay in the box.
    expect(foldsPastedText(Array.from({ length: 200 }, () => "x").join("\n"))).toBe(false);
  });

  it("counts the lines after the first, and keeps only LF", () => {
    expect(pastedTextExtraLines("one")).toBe(0);
    expect(pastedTextExtraLines("one\ntwo\nthree")).toBe(2);
    expect(normalizePastedText("a\r\nb\rc\n")).toBe("a\nb\nc\n");
  });

  it("numbers pastes past the highest the draft ever held", () => {
    expect(nextPastedTextId([])).toBe(1);
    expect(nextPastedTextId([{ id: 1 }, { id: 4 }])).toBe(5);
  });

  it("spells a paste's number in invisible marks, so tags that read alike stay apart", () => {
    const ids = [1, 2, 3, 4, 5, 8, 16, 255];
    const marks = ids.map(pastedTextIdMark);
    for (const mark of marks) expect(mark).toMatch(/^[\u2060\u200d]+$/u);
    expect(new Set(marks).size).toBe(ids.length);
    // The id in binary: a 1 is the zero-width joiner and a 0 the word joiner.
    expect(pastedTextIdMark(1)).toBe("\u200d");
    expect(pastedTextIdMark(2)).toBe("\u200d\u2060");
    expect(pastedTextIdMark(3)).toBe("\u200d\u200d");

    // Two pastes whose labels read the same on screen: one name, different marks.
    const unmarked = (text: string) => text.replace(/[\u2060\u200d]/gu, "");
    const label = (id: number) => `\u00a0粘贴文本\u00a0+1\u00a0行${pastedTextIdMark(id)}\u00a0`;
    const first: PastedText = { id: 1, label: label(1), content: "one\ntwo" };
    const second: PastedText = { id: 2, label: label(2), content: "three\nfour" };
    expect(first.label).not.toBe(second.label);
    expect(unmarked(first.label)).toBe(unmarked(second.label));
    expect(unmarked(first.label)).not.toContain("#");

    const value = `a ${first.label} b ${second.label} c ${first.label}`;
    const ranges = pastedTextRanges(value, [first, second]);
    expect(ranges.map((range) => range.paste.id)).toEqual([1, 2, 1]);
    expect(ranges.map((range) => value.slice(range.start, range.end))).toEqual([first.label, second.label, first.label]);
    expect(expandPastedTexts(value, [first, second])).toBe("a one\ntwo b three\nfour c one\ntwo");
    // Take the marks away and the label is only the words around it.
    const edited = `x ${unmarked(first.label)} y`;
    expect(pastedTextRanges(edited, [first, second])).toEqual([]);
    expect(expandPastedTexts(edited, [first, second])).toBe(edited);
  });

  it("finds each label where it stands, and none a hand edit broke", () => {
    const first = paste(1, "A\nB");
    const second = paste(2, "C");
    const value = `x ${first.label} y ${second.label}${first.label} [Pasted text #2`;
    expect(pastedTextRanges(value, [first, second]).map((range) => [range.start, range.paste.id]))
      .toEqual([[2, 1], [2 + first.label.length + 3, 2], [2 + first.label.length + 3 + second.label.length, 1]]);
    expect(pastedTextRanges(value, [])).toEqual([]);
    const [range] = pastedTextRanges(value, [first]);
    expect(pastedTextRangeAround([range], range.start)).toBeUndefined();
    expect(pastedTextRangeAround([range], range.start + 1)).toBe(range);
    expect(pastedTextRangeAround([range], range.end)).toBeUndefined();
  });

  it("sends every label as the text it stands for", () => {
    const first = paste(1, "first\npaste");
    const second = paste(2, "second");
    expect(expandPastedTexts(`see ${first.label} and ${second.label}.`, [first, second]))
      .toBe("see first\npaste and second.");
    // A label for a paste this draft does not hold is just text.
    expect(expandPastedTexts("[Pasted text #9]", [first])).toBe("[Pasted text #9]");
  });

  it("copies a span as the text it reads as, and as the box held it", () => {
    const first = paste(1, "the long text");
    const other = paste(2, "not copied");
    const value = `before ${first.label} after ${other.label}`;
    const end = value.indexOf(" after") + " after".length;
    const copied = draftClipboard(value, 0, end, [first, other]);
    expect(copied.plain).toBe("before the long text after");
    expect(copied.draft).toEqual({
      text: `before ${first.label} after`,
      pastes: [{ label: first.label, content: "the long text" }]
    });
    expect(parseDraftClipboard(JSON.stringify(copied.draft))).toEqual(copied.draft);
    expect(parseDraftClipboard("not json")).toBeNull();
    expect(parseDraftClipboard(JSON.stringify({ text: 1, pastes: [] }))).toBeNull();
    expect(parseDraftClipboard(JSON.stringify({ text: "t", pastes: [{ label: "", content: "c" }] })))
      .toEqual({ text: "t", pastes: [] });
  });

  it("renumbers copied tags into a draft, keeping a tag whose text the draft already holds", () => {
    const held = paste(1, "held text");
    const clip = {
      text: `${labelFor(1, 0)} and ${labelFor(3, 1)} again ${labelFor(1, 0)}`,
      pastes: [
        { label: labelFor(1, 0), content: "from elsewhere" },
        { label: labelFor(3, 1), content: "held text" }
      ]
    };
    const { text, added } = adoptDraftClipboard(clip, [held], labelFor);
    expect(added).toEqual([{ id: 2, label: labelFor(2, 0), content: "from elsewhere" }]);
    expect(text).toBe(`${labelFor(2, 0)} and ${held.label} again ${labelFor(2, 0)}`);
    expect(expandPastedTexts(text, [held, ...added])).toBe("from elsewhere and held text again from elsewhere");
    // Plain text copied out of a box comes back as it was, however long.
    const long = "z".repeat(2_000);
    expect(adoptDraftClipboard({ text: long, pastes: [] }, [], labelFor)).toEqual({ text: long, added: [] });
  });
});
