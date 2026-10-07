import {
  ChevronRight,
  GitCompareArrows
} from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { useI18n } from "../i18n";
import type { GitWorkspaceSnapshot } from "../lib/git";
import { sidePaneDomId } from "../lib/sidePanes";

/** The review pane these rows drive. */
const reviewPageDomId = sidePaneDomId("review");
import "./GitStatusCard.css";

export interface GitStatusCardProps {
  /**
   * The workspace's repository. The card exists only to report it, so the
   * caller renders nothing at all when the workspace is not a Git working
   * directory rather than passing null.
   */
  git: GitWorkspaceSnapshot;
  gitOpen?: boolean;
  onOpenGitReview?: () => void;
}

export function GitStatusCard({
  git,
  gitOpen = false,
  onOpenGitReview = () => undefined
}: GitStatusCardProps) {
  const { t } = useI18n();
  const cardBodyId = useId();
  const cardRef = useRef<HTMLElement>(null);
  const [expanded, setExpanded] = useState(false);

  useEffect(() => {
    if (!expanded) return;
    const closeOnOutsidePress = (event: MouseEvent) => {
      if (!cardRef.current?.contains(event.target as Node)) setExpanded(false);
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.defaultPrevented) setExpanded(false);
    };
    document.addEventListener("mousedown", closeOnOutsidePress);
    document.addEventListener("keydown", closeOnEscape);
    return () => {
      document.removeEventListener("mousedown", closeOnOutsidePress);
      document.removeEventListener("keydown", closeOnEscape);
    };
  }, [expanded]);

  const changeCount = git.changedFiles
    ?? (git.files.length || git.staged + git.unstaged + git.untracked + git.conflicted);
  /* The repository's next step, said as a state rather than a button's verb:
     Mewrk itself commits, pushes and pulls nothing — the model or a terminal
     does — so the row only opens the Review pane where the step is seen. */
  const operationStep = git.operation === "merge"
    ? t("合并待继续", "A merge to continue")
    : git.operation === "rebase"
      ? t("变基待继续", "A rebase to continue")
      : git.operation === "cherryPick"
        ? t("拣选提交待继续", "A cherry-pick to continue")
        : git.operation === "revert"
          ? t("还原提交待继续", "A revert to continue")
          : git.operation === "bisect"
            ? t("二分查找进行中", "A bisect in progress")
            : null;
  const nextStep = git.conflicted
    ? t("{count} 个冲突待解决", "{count} conflicts to resolve", { count: git.conflicted })
    : operationStep
      ? operationStep
      : changeCount > 0
        ? t("有更改待提交", "Changes to commit")
        : git.ahead > 0
          ? t("{count} 个提交待推送", "{count} commits to push", { count: git.ahead })
          : git.behind > 0
            ? t("{count} 个提交待拉取", "{count} commits to pull", { count: git.behind })
            : t("没有待提交或推送的内容", "Nothing to commit or push");
  return (
    <aside
      ref={cardRef}
      className={`git-status-card${expanded ? " git-status-card--expanded" : ""}`}
      aria-label={t("Git 状态", "Git status")}
    >
      <button
        type="button"
        className="git-status-card__toggle composer-chip"
        aria-label={expanded
          ? t("收起 Git 状态卡片", "Collapse Git status card")
          : t("展开 Git 状态卡片", "Expand Git status card")}
        aria-expanded={expanded}
        aria-controls={cardBodyId}
        onClick={() => setExpanded((current) => !current)}
      >
        <GitCompareArrows size={13} aria-hidden="true" />
        <span className="composer-chip__label">{t("Git 状态", "Git status")}</span>
      </button>
      <div id={cardBodyId} className="git-status-card__body">
        <div className="git-status-card__heading">{t("Git 状态", "Git status")}</div>
        <button
          type="button"
          className={`git-status-card__row${gitOpen ? " git-status-card__row--open" : ""}`}
          aria-current={gitOpen ? "page" : undefined}
          aria-controls={reviewPageDomId}
          onClick={() => onOpenGitReview()}
        >
          <strong>{t("变更", "Changes")}</strong>
          <span className="git-status-card__diff-stat" aria-label={t(
            "新增 {additions} 行，删除 {deletions} 行",
            "{additions} lines added, {deletions} lines deleted",
            { additions: git.additions, deletions: git.deletions }
          )}>
            <b>+{git.additions.toLocaleString()}</b>
            <em>−{git.deletions.toLocaleString()}</em>
          </span>
        </button>
        <div className="git-status-card__row git-status-card__row--static">
          <strong>{t("本地", "Local")}</strong>
          <small>{git.remote?.name ?? t("无远程仓库", "No remote")}</small>
        </div>
        <div className="git-status-card__row git-status-card__row--static">
          <strong title={git.branch ?? git.head ?? ""}>
            {git.branch ?? (git.head
              ? t("分离头指针 {head}", "Detached at {head}", { head: git.head })
              : t("尚无提交", "No commits yet"))}
          </strong>
          {(git.ahead > 0 || git.behind > 0) && (
            <small>{git.ahead > 0 ? `↑${git.ahead}` : ""}{git.ahead > 0 && git.behind > 0 ? " " : ""}{git.behind > 0 ? `↓${git.behind}` : ""}</small>
          )}
        </div>
        <button
          type="button"
          className={`git-status-card__row${gitOpen ? " git-status-card__row--open" : ""}`}
          aria-current={gitOpen ? "page" : undefined}
          aria-controls={reviewPageDomId}
          onClick={() => onOpenGitReview()}
        >
          <strong>{nextStep}</strong>
          <ChevronRight size={14} aria-hidden="true" />
        </button>
      </div>
    </aside>
  );
}
