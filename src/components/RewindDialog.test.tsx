import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { RewindFileEntry, RewindFilesRequest, RewindFilesResponse } from "../types";
import { RewindDialog } from "./RewindDialog";
import type { RewindDialogProps } from "./RewindDialog";

const runtime = vi.hoisted(() => ({ rewindFiles: vi.fn() }));
vi.mock("../lib/runtime", async (importOriginal) => ({
  ...await importOriginal<typeof import("../lib/runtime")>(),
  rewindFiles: runtime.rewindFiles
}));

const file = (displayPath: string, action: RewindFileEntry["action"], skipped: string | null = null): RewindFileEntry => ({
  path: `/work/${displayPath}`,
  displayPath,
  action,
  skipped
});

const FILES = [
  file("src/a.ts", "restore"),
  file("src/b.ts", "restore"),
  file("src/created.ts", "delete"),
  file("src/locked.ts", "restore", "文件在这条消息之后被外部修改过")
];

function renderDialog(overrides: Partial<RewindDialogProps> = {}) {
  const props: RewindDialogProps = {
    conversationId: "conversation-1",
    contextId: "user-2",
    blockedReason: null,
    conversationBlockedReason: null,
    onRewind: vi.fn(),
    onClose: vi.fn(),
    ...overrides
  };
  return { props, ...render(<RewindDialog {...props} />) };
}

const button = (name: string) => screen.getByRole("button", { name });

/** The previews and restores resolve with these; either can be made to fail or hang per test. */
function answerWith(preview: RewindFileEntry[], restored: RewindFileEntry[] = preview.filter((entry) => !entry.skipped)) {
  runtime.rewindFiles.mockImplementation(async (request: RewindFilesRequest): Promise<RewindFilesResponse> => ({
    files: request.dryRun ? preview : restored
  }));
}

describe("RewindDialog", () => {
  beforeEach(() => {
    runtime.rewindFiles.mockReset();
  });

  it("previews with a dry run, and while that loads only the conversation can be restored", async () => {
    runtime.rewindFiles.mockReturnValue(new Promise(() => {}));
    renderDialog();

    expect(runtime.rewindFiles).toHaveBeenCalledTimes(1);
    expect(runtime.rewindFiles).toHaveBeenCalledWith({ conversationId: "conversation-1", contextId: "user-2", dryRun: true });
    expect(screen.getByRole("dialog", { name: "回退到这条消息之前" })).toBeInTheDocument();
    expect(screen.getByText("正在检查文件改动…")).toBeInTheDocument();
    expect(button("仅恢复文件")).toBeDisabled();
    expect(button("恢复对话和文件")).toBeDisabled();
    expect(button("仅恢复对话")).toBeEnabled();
    expect(button("取消")).toBeEnabled();
  });

  it("says so when no file changed after the message, and leaves the file buttons off", async () => {
    answerWith([]);
    renderDialog();

    expect(await screen.findByText("这条消息之后没有可恢复的文件改动")).toBeInTheDocument();
    expect(screen.queryByRole("list")).not.toBeInTheDocument();
    expect(screen.getByText("只包含 write / edit 工具做的改动；shell 命令和远端工作区里的改动不会被恢复。")).toBeInTheDocument();
    expect(button("仅恢复文件")).toBeDisabled();
    expect(button("恢复对话和文件")).toBeDisabled();
    expect(button("仅恢复对话")).toBeEnabled();
  });

  it("lists the files with a summary, marks deletions, and greys a skipped file with its reason", async () => {
    answerWith(FILES);
    renderDialog();

    expect(await screen.findByText("将恢复 2 个文件，删除 1 个之后新建的文件；1 个会被跳过")).toBeInTheDocument();
    const items = within(screen.getByRole("list")).getAllByRole("listitem");
    expect(items.map((item) => item.querySelector(".rewind-dialog__path")?.textContent)).toEqual([
      "src/a.ts",
      "src/b.ts",
      "src/created.ts",
      "src/locked.ts"
    ]);
    expect(items[0]).not.toHaveAttribute("data-skipped");
    expect(items[2]).toHaveAttribute("data-action", "delete");
    expect(within(items[2]).getByText("删除")).toBeInTheDocument();
    expect(within(items[0]).getByText("恢复")).toBeInTheDocument();
    expect(items[3]).toHaveAttribute("data-skipped");
    expect(within(items[3]).getByText("跳过")).toBeInTheDocument();
    expect(within(items[3]).getByText("文件在这条消息之后被外部修改过")).toBeInTheDocument();
    expect(button("仅恢复文件")).toBeEnabled();
    expect(button("仅恢复对话")).toBeEnabled();
    expect(button("恢复对话和文件")).toBeEnabled();
  });

  it("offers no file restore when every file would be skipped", async () => {
    answerWith([file("src/locked.ts", "restore", "文件被占用")]);
    renderDialog();

    expect(await screen.findByText("没有文件会被恢复；1 个会被跳过")).toBeInTheDocument();
    expect(screen.getByText("文件被占用")).toBeInTheDocument();
    expect(button("仅恢复文件")).toBeDisabled();
    expect(button("恢复对话和文件")).toBeDisabled();
    expect(button("仅恢复对话")).toBeEnabled();
  });

  it("shows why the preview failed and keeps the file buttons off", async () => {
    runtime.rewindFiles.mockRejectedValue("当前对话正在运行，不能回退");
    renderDialog();

    expect(await screen.findByRole("alert")).toHaveTextContent("无法读取文件改动：当前对话正在运行，不能回退");
    expect(button("仅恢复文件")).toBeDisabled();
    expect(button("恢复对话和文件")).toBeDisabled();
    expect(button("仅恢复对话")).toBeEnabled();
  });

  it("disables every rewind and skips the preview while a model turn is running", async () => {
    renderDialog({ blockedReason: "模型回合进行中，完成或停止后才能回退" });

    expect(screen.getByText("模型回合进行中，完成或停止后才能回退")).toBeInTheDocument();
    expect(runtime.rewindFiles).not.toHaveBeenCalled();
    expect(button("仅恢复文件")).toBeDisabled();
    expect(button("仅恢复对话")).toBeDisabled();
    expect(button("恢复对话和文件")).toBeDisabled();
    expect(button("取消")).toBeEnabled();
  });

  it("keeps the files restorable when a branch anchors the messages, and says why the conversation is not", async () => {
    answerWith(FILES);
    renderDialog({ conversationBlockedReason: "有分支从这条消息或其之后的消息开始，不能回退对话" });

    expect(await screen.findByText("有分支从这条消息或其之后的消息开始，不能回退对话")).toBeInTheDocument();
    await screen.findByRole("list");
    expect(button("仅恢复文件")).toBeEnabled();
    expect(button("仅恢复对话")).toBeDisabled();
    expect(button("恢复对话和文件")).toBeDisabled();
  });

  it("restores only the files with a real run, then reports them and closes", async () => {
    answerWith(FILES);
    const { props } = renderDialog();
    await screen.findByRole("list");

    fireEvent.click(button("仅恢复文件"));

    await waitFor(() => expect(props.onClose).toHaveBeenCalledTimes(1));
    expect(runtime.rewindFiles).toHaveBeenCalledTimes(2);
    expect(runtime.rewindFiles).toHaveBeenLastCalledWith({ conversationId: "conversation-1", contextId: "user-2", dryRun: false });
    expect(props.onRewind).toHaveBeenCalledTimes(1);
    expect(props.onRewind).toHaveBeenCalledWith("files", { files: FILES.filter((entry) => !entry.skipped) });
  });

  it("restores only the conversation without touching any file", async () => {
    answerWith(FILES);
    const { props } = renderDialog();
    await screen.findByRole("list");

    fireEvent.click(button("仅恢复对话"));

    await waitFor(() => expect(props.onClose).toHaveBeenCalledTimes(1));
    // The one call is the preview: nothing was written.
    expect(runtime.rewindFiles).toHaveBeenCalledTimes(1);
    expect(props.onRewind).toHaveBeenCalledWith("conversation", null);
  });

  it("restores the files first and the conversation after them", async () => {
    answerWith(FILES);
    const { props } = renderDialog();
    await screen.findByRole("list");

    fireEvent.click(button("恢复对话和文件"));

    await waitFor(() => expect(props.onClose).toHaveBeenCalledTimes(1));
    expect(runtime.rewindFiles).toHaveBeenLastCalledWith({ conversationId: "conversation-1", contextId: "user-2", dryRun: false });
    expect(props.onRewind).toHaveBeenCalledWith("both", { files: FILES.filter((entry) => !entry.skipped) });
    const realRestore = runtime.rewindFiles.mock.invocationCallOrder[1];
    expect(realRestore).toBeLessThan(vi.mocked(props.onRewind).mock.invocationCallOrder[0]);
  });

  it("stops at a failed file restore: the error stays in the dialog and the conversation is not touched", async () => {
    runtime.rewindFiles.mockImplementation(async (request: RewindFilesRequest) => {
      if (request.dryRun) return { files: FILES };
      throw "写入 src/a.ts 失败：拒绝访问";
    });
    const { props } = renderDialog();
    await screen.findByRole("list");

    fireEvent.click(button("恢复对话和文件"));

    expect(await screen.findByRole("alert")).toHaveTextContent("写入 src/a.ts 失败：拒绝访问");
    expect(props.onRewind).not.toHaveBeenCalled();
    expect(props.onClose).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(button("恢复对话和文件")).toBeEnabled();
  });

  it("disables every button while an action is running", async () => {
    let finish: (response: RewindFilesResponse) => void = () => {};
    runtime.rewindFiles.mockImplementation((request: RewindFilesRequest) => (
      request.dryRun
        ? Promise.resolve({ files: FILES })
        : new Promise<RewindFilesResponse>((resolve) => { finish = resolve; })
    ));
    const { props } = renderDialog();
    await screen.findByRole("list");

    fireEvent.click(button("仅恢复文件"));

    await waitFor(() => expect(button("取消")).toBeDisabled());
    expect(button("仅恢复文件")).toBeDisabled();
    expect(button("仅恢复对话")).toBeDisabled();
    expect(button("恢复对话和文件")).toBeDisabled();
    // Neither Escape nor the backdrop closes it mid-way.
    fireEvent.keyDown(document, { key: "Escape" });
    expect(props.onClose).not.toHaveBeenCalled();

    finish({ files: FILES });
    await waitFor(() => expect(props.onClose).toHaveBeenCalledTimes(1));
  });

  it("closes without doing anything on Cancel", async () => {
    answerWith(FILES);
    const { props } = renderDialog();
    await screen.findByRole("list");

    fireEvent.click(button("取消"));

    expect(props.onClose).toHaveBeenCalledTimes(1);
    expect(props.onRewind).not.toHaveBeenCalled();
    expect(runtime.rewindFiles).toHaveBeenCalledTimes(1);
  });
});
