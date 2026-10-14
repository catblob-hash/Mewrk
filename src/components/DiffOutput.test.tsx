import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import {
  DiffOutput,
  parseUnifiedDiff
} from "./DiffOutput";

const sampleDiff = [
  "--- a/src/example.ts",
  "+++ b/src/example.ts",
  "@@ -2,3 +2,4 @@",
  " keep();",
  "-oldValue();",
  "+newValue();",
  "+extraValue();",
  " done();",
  ""
].join("\n");

describe("DiffOutput", () => {
  it("parses unified diff line kinds, paths, counts, and line numbers", () => {
    const parsed = parseUnifiedDiff(sampleDiff);

    expect(parsed.path).toBe("src/example.ts");
    expect(parsed.additions).toBe(2);
    expect(parsed.deletions).toBe(1);
    expect(parsed.lines.find((line) => line.kind === "deletion")).toMatchObject({
      text: "oldValue();",
      oldLineNumber: 3,
      newLineNumber: null
    });
    expect(parsed.lines.filter((line) => line.kind === "addition")).toEqual([
      expect.objectContaining({ text: "newValue();", oldLineNumber: null, newLineNumber: 3 }),
      expect.objectContaining({ text: "extraValue();", oldLineNumber: null, newLineNumber: 4 })
    ]);
  });

  it("renders an accessible read-only diff without markers or header rows", () => {
    const { container } = render(<DiffOutput value={sampleDiff} />);
    const region = screen.getByRole("region", { name: "src/example.ts 文件差异" });

    expect(within(region).getByLabelText("新增 2 行，删除 1 行")).toBeInTheDocument();
    expect(container.querySelector(".diff-output__line--addition")).toHaveTextContent("newValue();");
    expect(container.querySelector(".diff-output__line--deletion")).toHaveTextContent("oldValue();");
    expect(container.querySelector(".diff-output__marker")).toBeNull();
    expect(within(region).queryByText("--- a/src/example.ts")).not.toBeInTheDocument();
    expect(within(region).queryByText("+++ b/src/example.ts")).not.toBeInTheDocument();
    expect(within(region).queryByText("@@ -2,3 +2,4 @@")).not.toBeInTheDocument();
    expect(within(region).queryByRole("button")).not.toBeInTheDocument();
  });

  it("carries one line number per row and sizes the gutter to its widest number", () => {
    const { container } = render(
      <DiffOutput value={["--- a/wide.ts", "+++ b/wide.ts", "@@ -999,3 +999,3 @@", " keep();", "-old();", "+new();", " tail();", ""].join("\n")} />
    );
    const region = container.querySelector<HTMLElement>(".diff-output")!;
    const rows = container.querySelectorAll(".diff-output__line");

    expect(region.style.getPropertyValue("--diff-gutter")).toBe("4ch");
    for (const row of rows) {
      expect(row.querySelectorAll(".diff-output__line-number")).toHaveLength(1);
    }
    // A deletion is addressed in the old file, everything else in the new one.
    expect(container.querySelector(".diff-output__line--deletion .diff-output__line-number")).toHaveTextContent("1000");
    expect(container.querySelector(".diff-output__line--addition .diff-output__line-number")).toHaveTextContent("1000");
  });

  it("groups each run of changes with its deletions above its additions", () => {
    const { container } = render(
      <DiffOutput
        value={["--- a/x.ts", "+++ b/x.ts", "@@ -1,4 +1,4 @@", "+added-first", "-removed-after", " keep();", "+tail-add", "-tail-remove", ""].join("\n")}
      />
    );
    const changed = [...container.querySelectorAll(".diff-output__line--addition, .diff-output__line--deletion")]
      .map((row) => row.querySelector(".diff-output__content")?.textContent);

    expect(changed).toEqual(["removed-after", "added-first", "tail-remove", "tail-add"]);
  });

  it("does not confuse changed content beginning with diff header markers", () => {
    const parsed = parseUnifiedDiff("--- markers.txt\n+++ markers.txt\n@@ -1 +1 @@\n----\n++++\n");

    expect(parsed.deletions).toBe(1);
    expect(parsed.additions).toBe(1);
    expect(parsed.lines.find((line) => line.kind === "deletion")?.text).toBe("---");
    expect(parsed.lines.find((line) => line.kind === "addition")?.text).toBe("+++");
  });

  it("drops file and hunk rows and keeps meta and generated omission rows", () => {
    const contextLines = Array.from({ length: 810 }, (_, index) => ` line-${index}`);
    const largeDiff = [
      "--- a/src/large.ts",
      "+++ b/src/large.ts",
      "@@ -1,810 +1,810 @@",
      ...contextLines,
      "\\ No newline at end of file"
    ].join("\n");
    render(<DiffOutput value={largeDiff} />);

    expect(screen.queryByText("--- a/src/large.ts")).not.toBeInTheDocument();
    expect(screen.queryByText("@@ -1,810 +1,810 @@")).not.toBeInTheDocument();
    const omissionRow = screen.getByText(/行差异未显示/).closest(".diff-output__line");
    const metaRow = screen.getByText("\\ No newline at end of file").closest(".diff-output__line");

    expect(omissionRow).not.toBeNull();
    expect(metaRow).not.toBeNull();
  });

  describe("code colour", () => {
    const typedDiff = [
      "--- a/src/a.ts",
      "+++ b/src/a.ts",
      "@@ -1,2 +1,3 @@",
      " const kept = 1;",
      "+const added = 2;",
      " done();",
      ""
    ].join("\n");
    const replacedDiff = [
      "--- a/src/a.ts",
      "+++ b/src/a.ts",
      "@@ -1,3 +1,3 @@",
      " keep();",
      "-const old = 2;",
      "+const next = 2;",
      " done();",
      ""
    ].join("\n");

    it("colours a context or added line by the language of its file", () => {
      const { container } = render(<DiffOutput value={typedDiff} path="src/a.ts" />);

      expect(container.querySelector(".diff-output__line--context .code-token--keyword")?.textContent).toBe("const");
      expect(container.querySelector(".diff-output__line--addition .code-token--keyword")?.textContent).toBe("const");
    });

    it("draws no syntax colour without a path, or with one whose extension has no grammar", () => {
      const withoutPath = render(<DiffOutput value={"@@ -1,2 +1,3 @@\n const kept = 1;\n+const added = 2;\n done();\n"} />);
      expect(withoutPath.container.querySelector(".code-token")).toBeNull();
      withoutPath.unmount();

      const unknownExtension = render(<DiffOutput value={typedDiff} path="notes.xyz" />);
      expect(unknownExtension.container.querySelector(".code-token")).toBeNull();
    });

    it("marks only the words that changed in a replaced line, on each side", () => {
      const { container } = render(<DiffOutput value={replacedDiff} path="src/a.ts" />);

      expect(container.querySelector(".diff-output__word--deletion")?.textContent).toBe("old");
      expect(container.querySelector(".diff-output__word--addition")?.textContent).toBe("next");
      expect(container.querySelectorAll(".diff-output__word")).toHaveLength(2);
    });

    it("marks no words in an added line that replaces nothing", () => {
      const { container } = render(
        <DiffOutput value={["--- a/x.ts", "+++ b/x.ts", "@@ -1 +1,2 @@", " keep();", "+added();", ""].join("\n")} />
      );

      expect(container.querySelector(".diff-output__line--addition")).toHaveTextContent("added();");
      expect(container.querySelector(".diff-output__word")).toBeNull();
    });

    it("keeps each line's text whole once it is coloured and marked", () => {
      const { container } = render(<DiffOutput value={replacedDiff} path="src/a.ts" />);
      const texts = [...container.querySelectorAll(".diff-output__content")].map((content) => content.textContent);

      expect(texts).toEqual(["keep();", "const old = 2;", "const next = 2;", "done();"]);
    });
  });
});
