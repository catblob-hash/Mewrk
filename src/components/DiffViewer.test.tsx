import { fireEvent, render } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { parseUnifiedDiff } from "../lib/unifiedDiff";
import { DiffViewer } from "./DiffViewer";
import type { DiffEntry, DiffViewerProps } from "./DiffViewer";

const path = "src/app.ts";

const patch = [
  `diff --git a/${path} b/${path}`,
  "index 1111111..2222222 100644",
  `--- a/${path}`,
  `+++ b/${path}`,
  "@@ -1,4 +1,4 @@",
  " const keep = 1;",
  "-const old = 2;",
  "+const next = 2;",
  "-const gone = 3;",
  " const tail = 4;",
  "+const extra = 5;",
  ""
].join("\n");

const entries: DiffEntry[] = [{ path, status: "modified", additions: 2, deletions: 2 }];
const patches = new Map(parseUnifiedDiff(patch).map((file) => [file.path, file]));

function renderViewer(overrides: Partial<DiffViewerProps> = {}) {
  return render(
    <DiffViewer
      entries={entries}
      patches={patches}
      showTree={false}
      diffStyle="unified"
      wordWrap
      wordDiff={false}
      hideWhitespace={false}
      activePath={null}
      onSelectFile={() => undefined}
      emptyLabel="No changes"
      {...overrides}
    />
  );
}

async function expandFile(container: HTMLElement): Promise<void> {
  const toggle = container.querySelector(".diff-viewer__file-toggle");
  if (!(toggle instanceof HTMLElement)) throw new Error(`no fold toggle for ${path}`);
  await userEvent.click(toggle);
}

/** The text cell of the line that reads `text`: a coloured line is several nodes, not one. */
function lineText(container: HTMLElement, text: string): HTMLElement {
  const cell = Array.from(container.querySelectorAll<HTMLElement>(".diff-viewer__text"))
    .find((candidate) => candidate.textContent === text);
  if (!cell) throw new Error(`no line reads ${text}`);
  return cell;
}

function numberTone(row: Element): "addition" | "deletion" | null {
  const cell = row.querySelector(".diff-viewer__number");
  if (cell?.classList.contains("diff-viewer__number--deletion")) return "deletion";
  if (cell?.classList.contains("diff-viewer__number--addition")) return "addition";
  return null;
}

describe("DiffViewer", () => {
  it("draws each file's type icon in the file column and on its card", () => {
    const { container } = renderViewer({ showTree: true });
    const icon = (selector: string) => container.querySelector(`${selector} img.file-icon`)?.getAttribute("src");

    expect(icon(".diff-viewer__tree-file")).toBe("/file-icons/typescript.svg");
    expect(icon(".diff-viewer__file-toggle")).toBe("/file-icons/typescript.svg");
  });

  it("answers a right click on an added line with its number in the new file", async () => {
    const onLineMenu = vi.fn();
    const { container } = renderViewer({ onLineMenu });
    await expandFile(container);

    fireEvent.contextMenu(lineText(container, "const extra = 5;"), { clientX: 30, clientY: 40 });

    expect(onLineMenu).toHaveBeenCalledWith(path, 4, { x: 30, y: 40 });
  });

  it("answers a right click on a removed line with its number in the old file", async () => {
    const onLineMenu = vi.fn();
    const { container } = renderViewer({ onLineMenu });
    await expandFile(container);

    fireEvent.contextMenu(lineText(container, "const gone = 3;"), { clientX: 5, clientY: 6 });

    expect(onLineMenu).toHaveBeenCalledWith(path, 3, { x: 5, y: 6 });
  });

  it("colours each line as its file's code, and marks a changed word inside the colouring", async () => {
    const { container } = renderViewer({ wordDiff: true });
    await expandFile(container);

    // Unchanged, removed and added lines are all the file's code.
    for (const text of ["const keep = 1;", "const old = 2;", "const next = 2;"]) {
      const keyword = lineText(container, text).querySelector(".code-token--keyword");
      expect(keyword).toHaveTextContent("const");
    }
    // The replaced name is the one word marked, and the line still reads whole.
    const removed = lineText(container, "const old = 2;").querySelector(".diff-viewer__word--deletion");
    const added = lineText(container, "const next = 2;").querySelector(".diff-viewer__word--addition");
    expect(removed).toHaveTextContent("old");
    expect(added).toHaveTextContent("next");
  });

  it("draws a file with no grammar as plain text", async () => {
    const plainPath = "notes.unknownext";
    const plain = parseUnifiedDiff(patch.replaceAll(path, plainPath));
    const { container } = renderViewer({
      entries: [{ path: plainPath, status: "modified", additions: 2, deletions: 2 }],
      patches: new Map(plain.map((file) => [file.path, file]))
    });
    await expandFile(container);

    expect(lineText(container, "const keep = 1;").querySelector(".code-token")).toBeNull();
  });

  it("tones the number column of a split row by the side that changed", async () => {
    const { container } = renderViewer({ diffStyle: "split" });
    await expandFile(container);

    const rows = Array.from(container.querySelectorAll(".diff-viewer__row"));
    expect(rows.map((row) => row.querySelector(".diff-viewer__number")?.textContent)).toEqual(["1", "2", "3", "3", "4"]);
    expect(rows.map(numberTone)).toEqual([null, "deletion", "deletion", null, "addition"]);
  });

  it("marks the code grid once it is scrolled sideways without wrapping", async () => {
    const { container } = renderViewer({ wordWrap: false });
    await expandFile(container);
    const code = container.querySelector(".diff-viewer__code");
    if (!(code instanceof HTMLElement)) throw new Error(`no code grid for ${path}`);

    Object.defineProperty(code, "scrollLeft", { configurable: true, value: 40 });
    fireEvent.scroll(code);
    expect(code).toHaveAttribute("data-scrolled");

    Object.defineProperty(code, "scrollLeft", { configurable: true, value: 0 });
    fireEvent.scroll(code);
    expect(code).not.toHaveAttribute("data-scrolled");
  });
});
