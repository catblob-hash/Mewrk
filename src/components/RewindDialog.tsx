import { useEffect, useState } from "react";
import { useI18n } from "../i18n";
import { errorMessage } from "../lib/errors";
import {
  isRewindable,
  rewindPreviewSummary,
  summarizeRewindFiles
} from "../lib/conversationRewind";
import { rewindFiles } from "../lib/runtime";
import type { RewindFileEntry, RewindFilesResponse } from "../types";
import { Dialog } from "./Common";
import { FileIcon } from "./FileIcon";
import "./RewindDialog.css";

/** Which halves of "back to before this message" the user chose to carry out. */
export type RewindScope = "both" | "conversation" | "files";

export interface RewindDialogProps {
  conversationId: string;
  /** The user message the conversation and its files go back to before. */
  contextId: string;
  /** Why nothing may be rewound right now (a model turn is running); null when it may. */
  blockedReason: string | null;
  /**
   * Why the conversation's own timeline cannot give the messages up (a branch was taken
   * from one of them); null when it can. The files can still be put back.
   */
  conversationBlockedReason: string | null;
  /**
   * Carries out the timeline half and reports the outcome, once the files — when the scope
   * has them — are already back; `files` is what the restore did.
   */
  onRewind: (scope: RewindScope, files: RewindFilesResponse | null) => void | Promise<void>;
  onClose: () => void;
}

type Preview =
  | { status: "loading" }
  | { status: "ready"; files: RewindFileEntry[] }
  | { status: "error"; message: string };

/**
 * The window "rewind to before this message" opens. It is the confirmation too: it lists what
 * the files would go through (a dry run, nothing touched), then the user picks which of the
 * conversation and the files to put back.
 */
export function RewindDialog({
  conversationId,
  contextId,
  blockedReason,
  conversationBlockedReason,
  onRewind,
  onClose
}: RewindDialogProps) {
  const { t } = useI18n();
  const [preview, setPreview] = useState<Preview>({ status: "loading" });
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const blocked = Boolean(blockedReason);

  // A blocked rewind has nothing to preview: the host would refuse the dry run as it refuses the real one.
  useEffect(() => {
    if (blocked) return;
    let cancelled = false;
    setPreview({ status: "loading" });
    rewindFiles({ conversationId, contextId, dryRun: true }).then(
      (response) => {
        if (!cancelled) setPreview({ status: "ready", files: response.files });
      },
      (reason) => {
        if (!cancelled) setPreview({ status: "error", message: errorMessage(reason, "") });
      }
    );
    return () => {
      cancelled = true;
    };
  }, [blocked, conversationId, contextId]);

  const files = preview.status === "ready" ? preview.files : [];
  const summary = summarizeRewindFiles(files);
  const hasFiles = summary.restored + summary.deleted > 0;
  const filesAllowed = !blocked && !busy && preview.status === "ready" && hasFiles;
  const conversationAllowed = !blocked && !busy && !conversationBlockedReason;

  const run = async (scope: RewindScope) => {
    setBusy(true);
    setActionError(null);
    let restored: RewindFilesResponse | null = null;
    try {
      if (scope !== "conversation") {
        restored = await rewindFiles({ conversationId, contextId, dryRun: false });
      }
    } catch (reason) {
      // Nothing of the conversation has moved: the files are the first half, and a failure stops here.
      setActionError(errorMessage(reason, t("文件回退失败", "The files could not be rewound")));
      setBusy(false);
      return;
    }
    try {
      await onRewind(scope, restored);
      onClose();
    } catch (reason) {
      setActionError(errorMessage(reason, t("回退失败", "The rewind failed")));
      setBusy(false);
    }
  };

  const close = () => {
    if (!busy) onClose();
  };

  return (
    <Dialog
      title={t("回退到这条消息之前", "Rewind to before this message")}
      width="560px"
      dismissible={!busy}
      onClose={close}
      className="rewind-dialog"
      footer={(
        <>
          <button type="button" className="button" disabled={busy} onClick={close}>
            {t("取消", "Cancel")}
          </button>
          <button
            type="button"
            className="button button--secondary"
            disabled={!filesAllowed}
            onClick={() => void run("files")}
          >
            {t("仅恢复文件", "Restore files only")}
          </button>
          <button
            type="button"
            className="button button--secondary"
            disabled={!conversationAllowed}
            onClick={() => void run("conversation")}
          >
            {t("仅恢复对话", "Restore conversation only")}
          </button>
          <button
            type="button"
            className="button button--danger"
            disabled={!filesAllowed || !conversationAllowed}
            onClick={() => void run("both")}
          >
            {t("恢复对话和文件", "Restore conversation and files")}
          </button>
        </>
      )}
    >
      {blockedReason && <p className="rewind-dialog__warning" role="status">{blockedReason}</p>}
      {!blocked && preview.status === "loading" && (
        <p className="confirm-copy" role="status">{t("正在检查文件改动…", "Checking file changes…")}</p>
      )}
      {!blocked && preview.status === "error" && (
        <p className="rewind-dialog__error" role="alert">
          {t("无法读取文件改动：{error}", "Could not read the file changes: {error}", {
            error: preview.message || t("未知错误", "Unknown error")
          })}
        </p>
      )}
      {!blocked && preview.status === "ready" && files.length === 0 && (
        <p className="confirm-copy">{t("这条消息之后没有可恢复的文件改动", "There are no file changes to restore after this message")}</p>
      )}
      {!blocked && preview.status === "ready" && files.length > 0 && (
        <>
          <p className="rewind-dialog__summary">{rewindPreviewSummary(summary, t)}</p>
          <ul className="rewind-dialog__files">
            {files.map((entry) => (
              <li
                key={entry.path}
                className="rewind-dialog__file"
                data-action={entry.action}
                data-skipped={isRewindable(entry) ? undefined : ""}
                title={entry.path}
              >
                <span className="rewind-dialog__path">
                  <FileIcon path={entry.path} size={13} inline />
                  {entry.displayPath}
                </span>
                <span className="rewind-dialog__tag">
                  {!isRewindable(entry)
                    ? t("跳过", "Skipped")
                    : entry.action === "delete"
                      ? t("删除", "Delete")
                      : t("恢复", "Restore")}
                </span>
                {entry.skipped && <span className="rewind-dialog__reason">{entry.skipped}</span>}
              </li>
            ))}
          </ul>
        </>
      )}
      <p className="rewind-dialog__scope">
        {t(
          "只包含 write / edit 工具做的改动；shell 命令和远端工作区里的改动不会被恢复。",
          "Only changes made by the write and edit tools are included; changes made by shell commands or in remote workspaces are not restored."
        )}
      </p>
      {conversationBlockedReason && !blocked && (
        <p className="rewind-dialog__warning" role="status">{conversationBlockedReason}</p>
      )}
      {actionError && <p className="rewind-dialog__error" role="alert">{actionError}</p>}
    </Dialog>
  );
}
