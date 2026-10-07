import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { FileAttachmentPreview, PdfReadingContext } from "./FileAttachmentPreview";
import { readsPdfDocuments } from "../lib/modelCapabilities";
import type { FileAttachment } from "../types";

vi.mock("../lib/runtime", async (importOriginal) => ({
  ...await importOriginal<typeof import("../lib/runtime")>(),
  fileAttachmentData: vi.fn(() => new Promise(() => undefined))
}));

const pdf = (pages: number): FileAttachment => ({
  id: `pdf-${pages}`,
  name: "spec.pdf",
  format: "pdf",
  bytes: 1024,
  tokens: 300,
  pages
});

function preview(file: FileAttachment, readsDocuments: boolean) {
  return render(
    <PdfReadingContext.Provider value={readsDocuments}>
      <FileAttachmentPreview file={file} onClose={() => undefined} />
    </PdfReadingContext.Provider>
  );
}

describe("PDF attachment preview", () => {
  it("says the model reads the PDF itself where it does", () => {
    preview(pdf(3), true);
    expect(screen.getByText(/模型读取的是这份 PDF 本身/)).toBeInTheDocument();
  });

  it("says the model reads the extracted text past the page limit or without document support", () => {
    const { unmount } = preview(pdf(30), true);
    expect(screen.getByText("模型读取的是上传时提取出的文字")).toBeInTheDocument();
    unmount();
    preview(pdf(3), false);
    expect(screen.getByText("模型读取的是上传时提取出的文字")).toBeInTheDocument();
  });

  it("mirrors which families read a PDF as a document", () => {
    expect(readsPdfDocuments("anthropic", true)).toBe(true);
    expect(readsPdfDocuments("openai_chat", true)).toBe(true);
    expect(readsPdfDocuments("anthropic", false)).toBe(false);
    expect(readsPdfDocuments("openai_compatible", true)).toBe(false);
    expect(readsPdfDocuments("bedrock", true)).toBe(false);
  });
});
