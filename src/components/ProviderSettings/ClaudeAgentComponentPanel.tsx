import { ArrowUpCircle, CheckCircle2, CircleAlert, Download, RefreshCw, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import type { TranslationFunction } from "../../i18n";
import { useI18n } from "../../i18n";
import {
  cancelClaudeAgentComponentInstall,
  claudeAgentComponentStatus,
  installClaudeAgentComponent
} from "../../lib/runtime";
import type { ClaudeAgentComponentStatus } from "../../types";
import { IconButton } from "../Common";
import { formatBytes } from "../FilePreview/format";
import { useSettingsSession } from "../settingsSession";
import { useWindowRefocus } from "./useWindowRefocus";

/** Where the settings session keeps the last component status (the components are not per provider). */
export const CLAUDE_AGENT_COMPONENTS_KEY = "claude-agent:components";
/** Set once this settings session's npm check has answered. */
const CLAUDE_AGENT_COMPONENTS_CHECKED_KEY = "claude-agent:components:checked";

/** How often a running install's progress is read. */
const POLL_INTERVAL_MS = 500;

function errorText(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason);
}

/**
 * An answer to a status read that did not ask npm (`latest` and friends come back
 * empty) must not erase what the last check found. While the installed version is
 * the same the check still describes it; once it changed (an install finished) the
 * old verdict no longer applies, and the re-check that follows fills it in.
 */
function keepCheckResult(
  previous: ClaudeAgentComponentStatus | null,
  next: ClaudeAgentComponentStatus
): ClaudeAgentComponentStatus {
  if (!previous || next.latest || next.latestError || next.newerIncompatible) return next;
  if (previous.installed?.sdkVersion !== next.installed?.sdkVersion) return next;
  return {
    ...next,
    latest: previous.latest,
    latestError: previous.latestError,
    newerIncompatible: previous.newerIncompatible,
    updateAvailable: previous.updateAvailable
  };
}

function versionsText(t: TranslationFunction, versions: { sdkVersion: string; claudeCodeVersion: string | null }) {
  return versions.claudeCodeVersion
    ? t("SDK {sdk}（Claude Code {cli}）", "SDK {sdk} (Claude Code {cli})", {
      sdk: versions.sdkVersion,
      cli: versions.claudeCodeVersion
    })
    : t("SDK {sdk}", "SDK {sdk}", { sdk: versions.sdkVersion });
}

function phaseText(t: TranslationFunction, phase: NonNullable<ClaudeAgentComponentStatus["task"]>["phase"]): string {
  switch (phase) {
    case "resolving":
      return t("正在查找版本…", "Looking up the version…");
    case "downloading":
      return t("正在下载…", "Downloading…");
    case "verifying":
      return t("正在校验…", "Verifying…");
    case "installing":
      return t("正在安装…", "Installing…");
  }
}

export interface ClaudeAgentComponentsView {
  /**
   * Moves each time an update of an installed copy ends and the status has been read
   * again. What depends on the installed CLI (the sign-in status) reads again when it
   * does. A first install does not move it: what appears then reads for the first time.
   */
  revision: number;
  /**
   * The components were installed for the first time while this panel was up. The
   * sign-in panel that appears then reads for the first time, and a login it finds
   * enables the provider: installing is how the user asked for it.
   */
  freshInstall: boolean;
}

/**
 * The Claude Agent SDK and the Claude Code CLI Mewrk drives. The installer does not
 * carry them: this panel installs them from npm and keeps them current, and shows
 * `children` (the sign-in panel, which needs the CLI) only once they are there.
 *
 * Refresh rule: the first time the panel is shown in a settings session it asks the
 * host (checking npm for the newest compatible version); showing it again in the same
 * session reuses that answer, and a new session starts over. Coming back to the app
 * window after leaving it asks again, keeping what is on screen until the answer is in.
 * While an install runs, its progress is polled.
 */
export function ClaudeAgentComponentPanel({
  desktopRuntime,
  children
}: {
  desktopRuntime: boolean;
  children?: (view: ClaudeAgentComponentsView) => ReactNode;
}) {
  const { t } = useI18n();
  const session = useSettingsSession();
  const [status, setStatus] = useState<ClaudeAgentComponentStatus | null>(
    () => session.peek<ClaudeAgentComponentStatus>(CLAUDE_AGENT_COMPONENTS_KEY) ?? null
  );
  const [readError, setReadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  // A check the user asked for, and the re-read after a task ended (which also hides the
  // install/update buttons, whose verdict is out of date until it answers).
  const [checking, setChecking] = useState(false);
  const [settling, setSettling] = useState(false);
  const [revision, setRevision] = useState(0);
  const [freshInstall, setFreshInstall] = useState(false);
  const mountedRef = useRef(true);
  const statusRef = useRef(status);
  // Answers are applied in the order their requests started: a slow npm check must not
  // overwrite the newer progress a poll has shown meanwhile.
  const issuedRef = useRef(0);
  const appliedRef = useRef(0);
  const firstReadRef = useRef(false);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const show = useCallback((next: ClaudeAgentComponentStatus) => {
    statusRef.current = next;
    setStatus(next);
    session.put(CLAUDE_AGENT_COMPONENTS_KEY, next);
  }, [session]);

  /** Reads the status. `null` when the answer was superseded or the panel is gone; throws when the host fails. */
  const request = useCallback(async (checkLatest: boolean): Promise<ClaudeAgentComponentStatus | null> => {
    issuedRef.current += 1;
    const sequence = issuedRef.current;
    const answer = await claudeAgentComponentStatus(checkLatest);
    if (!mountedRef.current || sequence < appliedRef.current) return null;
    appliedRef.current = sequence;
    const next = checkLatest ? answer : keepCheckResult(statusRef.current, answer);
    show(next);
    setReadError(null);
    return next;
  }, [show]);

  /**
   * A task ended: read the status afresh (npm included). `replaced` says a copy was
   * installed before the task began, so a sign-in panel is already up and reads again
   * (the CLI behind it is another one now); after a first install the panel has only
   * just appeared and its own first read is the current one.
   */
  const settleTask = useCallback(async (replaced: boolean) => {
    setSettling(true);
    try {
      await request(true);
    } catch {
      // What the panel shows stays; Check for updates reports a failure in full.
    } finally {
      if (mountedRef.current) {
        setSettling(false);
        if (replaced) setRevision((current) => current + 1);
        else if (statusRef.current?.installed) setFreshInstall(true);
      }
    }
  }, [request]);

  // The first time the panel is shown in this settings session: what is installed
  // (on disk, at once), then the npm check, which can take as long as the network
  // does and so must not hold the versions back. A page shown again in the session
  // asks nothing; one left before its check answered checks on its next showing.
  useEffect(() => {
    if (!desktopRuntime) return undefined;
    let live = true;
    const checkOnce = () => {
      if (session.peek(CLAUDE_AGENT_COMPONENTS_CHECKED_KEY)) return;
      setChecking(true);
      request(true)
        .then((next) => {
          if (next) session.put(CLAUDE_AGENT_COMPONENTS_CHECKED_KEY, true);
        }, () => {
          // The versions stay; Check for updates reports a failure in full.
        })
        .finally(() => {
          if (mountedRef.current) setChecking(false);
        });
    };
    if (session.peek(CLAUDE_AGENT_COMPONENTS_KEY)) {
      checkOnce();
      return undefined;
    }
    firstReadRef.current = true;
    setReadError(null);
    session.load(CLAUDE_AGENT_COMPONENTS_KEY, () => claudeAgentComponentStatus(false)).then(
      (answer) => {
        firstReadRef.current = false;
        if (!live || !mountedRef.current) return;
        show(keepCheckResult(statusRef.current, answer));
        checkOnce();
      },
      (reason: unknown) => {
        firstReadRef.current = false;
        if (live && mountedRef.current) setReadError(errorText(reason));
      }
    );
    return () => {
      live = false;
    };
  }, [desktopRuntime, session, show, request]);

  // Back in the window after leaving it. Quiet: the answer replaces what is shown only
  // when it arrives, and a failure changes nothing. A running task is already polled.
  useWindowRefocus(desktopRuntime, () => {
    if (firstReadRef.current || statusRef.current?.task) return;
    request(true).catch(() => undefined);
  });

  const running = Boolean(status?.task);
  useEffect(() => {
    if (!desktopRuntime || !running) return undefined;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      const replaced = Boolean(statusRef.current?.installed);
      try {
        const next = await request(false);
        // Applying the answer that ended the task also ends this effect, so the end is
        // looked for before `stopped`.
        if (next && !next.task) {
          void settleTask(replaced);
          return;
        }
      } catch (reason) {
        if (stopped) return;
        setReadError(errorText(reason));
      }
      if (!stopped) timer = setTimeout(poll, POLL_INTERVAL_MS);
    };
    timer = setTimeout(poll, POLL_INTERVAL_MS);
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [desktopRuntime, running, request, settleTask]);

  useEffect(() => {
    if (!running) setCancelling(false);
  }, [running]);

  const check = async () => {
    setChecking(true);
    try {
      await request(true);
    } catch (reason) {
      if (mountedRef.current) setReadError(errorText(reason));
    } finally {
      if (mountedRef.current) setChecking(false);
    }
  };

  const startTask = async () => {
    setStarting(true);
    setActionError(null);
    try {
      // The host picks the newest compatible version itself when it gets none.
      await installClaudeAgentComponent(null);
      // Anything asked before the task began describes a world without it.
      appliedRef.current = issuedRef.current + 1;
      const replaced = Boolean(statusRef.current?.installed);
      const next = await request(false);
      // A task that is already over (it failed at once) is not polled.
      if (next && !next.task) void settleTask(replaced);
    } catch (reason) {
      if (mountedRef.current) setActionError(errorText(reason));
      // Another window's task may be the reason it was refused; its progress is worth showing.
      request(false).catch(() => undefined);
    } finally {
      if (mountedRef.current) setStarting(false);
    }
  };

  const cancelTask = async () => {
    setCancelling(true);
    try {
      await cancelClaudeAgentComponentInstall();
    } catch (reason) {
      if (!mountedRef.current) return;
      setCancelling(false);
      setActionError(errorText(reason));
    }
  };

  const title = t("Claude Agent 组件", "Claude Agent components");

  if (!desktopRuntime) {
    return (
      <>
        <section className="provider-field provider-components">
          <div className="provider-components__summary">
            <CircleAlert size={16} aria-hidden="true" className="provider-components__icon" />
            <div className="provider-components__copy">
              <div className="provider-field__title">{title}</div>
              <p className="provider-field__help">{t(
                "浏览器预览无法管理 Claude Agent 组件。",
                "Browser preview cannot manage the Claude Agent components."
              )}</p>
            </div>
          </div>
        </section>
        {children?.({ revision, freshInstall })}
      </>
    );
  }

  const installed = status?.installed ?? null;
  const task = status?.task ?? null;
  const primary: "install" | "update" | null = !status || task || settling
    ? null
    : !installed
      ? "install"
      : status.updateAvailable || status.lastError || !installed.compatible
        ? "update"
        : null;
  const icon = task || checking || settling || !status
    ? <RefreshCw size={16} aria-hidden="true" className="provider-components__icon spin" />
    : !installed || !installed.compatible
      ? <CircleAlert size={16} aria-hidden="true" className="provider-components__icon provider-components__icon--warning" />
      : status.updateAvailable
        ? <ArrowUpCircle size={16} aria-hidden="true" className="provider-components__icon provider-components__icon--accent" />
        : <CheckCircle2 size={16} aria-hidden="true" className="provider-components__icon provider-components__icon--success" />;

  const totalBytes = task?.totalBytes ?? 0;
  const determinate = task?.phase === "downloading" && totalBytes > 0;
  const percent = determinate && task
    ? Math.min(100, Math.round((task.receivedBytes / totalBytes) * 100))
    : 0;
  const bytesText = task?.phase === "downloading"
    ? totalBytes > 0
      ? `${formatBytes(task.receivedBytes)} / ${formatBytes(totalBytes)} · ${percent}%`
      : formatBytes(task.receivedBytes)
    : "";

  return (
    <>
      <section className="provider-field provider-components">
        <div className="provider-components__summary">
          {icon}
          <div className="provider-components__copy">
            <div className="provider-field__title">
              <span>{title}</span>
              {installed?.source === "development" && (
                <span className="provider-components__badge">{t("开发版本", "Development")}</span>
              )}
            </div>

            {!status && (
              <p className="provider-field__help">{readError
                ? t("读取组件状态失败。", "Could not read the component status.")
                : t("正在读取组件状态…", "Loading component status…")}</p>
            )}

            {status && !installed && (
              <p className="provider-field__help">{t(
                "Claude Agent 需要 Claude Agent SDK 和 Claude Code。Mewrk 的安装包不带它们：点「安装」，从 npm 下载 Anthropic 发布的官方包，装好后再登录。",
                "Claude Agent needs the Claude Agent SDK and Claude Code. Mewrk's installer does not include them: choose Install to download Anthropic's official packages from npm, then sign in."
              )}</p>
            )}

            {installed && (
              <dl className="provider-components__versions">
                <dt>Claude Agent SDK</dt>
                <dd>{installed.sdkVersion}</dd>
                <dt>Claude Code</dt>
                <dd>{installed.claudeCodeVersion ?? "—"}</dd>
              </dl>
            )}
            {status && installed && !installed.compatible && (
              <p className="provider-field__error" role="alert">{t(
                "已安装的版本不在当前 AI SDK 组件支持的范围（{range}）内，Claude Agent 用不了它：请更新。",
                "The installed version is outside what the current AI SDK component supports ({range}), so Claude Agent cannot use it: update it.",
                { range: status.compatible }
              )}</p>
            )}
            {installed?.source === "development" && (
              <p className="provider-field__help">{t(
                "开发版本：用的是源码目录里的 SDK 和 Claude Code，不是下载安装的。",
                "Development build: this uses the SDK and Claude Code from the source tree, not a downloaded install."
              )}</p>
            )}

            {status && !task && (
              checking || settling ? (
                <p className="provider-field__help">{t("正在检查更新…", "Checking for updates…")}</p>
              ) : status.updateAvailable && status.latest ? (
                <p className="provider-field__help provider-components__verdict--accent">{t(
                  "有新版本：{versions}",
                  "A newer version is available: {versions}",
                  { versions: versionsText(t, status.latest) }
                )}</p>
              ) : status.latest ? (
                <p className="provider-field__help">{installed
                  ? t("已是最新的兼容版本。", "You have the newest compatible version.")
                  : t("将安装 {versions}。", "Will install {versions}.", { versions: versionsText(t, status.latest) })}</p>
              ) : status.latestError ? (
                <p className="provider-field__help">{t(
                  "没能检查新版本：{error}",
                  "Could not check for a newer version: {error}",
                  { error: status.latestError }
                )}</p>
              ) : null
            )}
            {status?.newerIncompatible && (
              <p className="provider-field__help">{t(
                "npm 上已有 SDK {version}，但这个版本的 Mewrk 只兼容 {range}；更新 Mewrk 后才能用它。",
                "SDK {version} is on npm, but this version of Mewrk only supports {range}; update Mewrk to use it.",
                { version: status.newerIncompatible, range: status.compatible }
              )}</p>
            )}
          </div>
        </div>

        {task && (
          <div className="provider-components__task">
            <div className="provider-components__task-line">
              <span>{task.sdkVersion
                ? task.action === "update"
                  ? t("正在更新到 SDK {version}", "Updating to SDK {version}", { version: task.sdkVersion })
                  : t("正在安装 SDK {version}", "Installing SDK {version}", { version: task.sdkVersion })
                : task.action === "update"
                  ? t("正在更新 Claude Agent 组件", "Updating the Claude Agent components")
                  : t("正在安装 Claude Agent 组件", "Installing the Claude Agent components")}</span>
              <span className="provider-components__task-phase">{phaseText(t, task.phase)}</span>
            </div>
            <div className="provider-components__progress-row">
              <div
                className={`provider-components__progress${determinate ? "" : " provider-components__progress--indeterminate"}`}
                role="progressbar"
                aria-label={t("安装进度", "Install progress")}
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={determinate ? percent : undefined}
              >
                <span style={determinate ? { width: `${percent}%` } : undefined} />
              </div>
              {bytesText && <span className="provider-components__progress-text">{bytesText}</span>}
              <IconButton
                label={t("取消", "Cancel")}
                disabled={cancelling}
                onClick={() => void cancelTask()}
              >
                <X size={14} />
              </IconButton>
            </div>
          </div>
        )}

        {status?.lastError && !task && (
          <p className="provider-field__error" role="alert">{installed
            ? t("更新失败：{error}", "Update failed: {error}", { error: status.lastError })
            : t("安装失败：{error}", "Install failed: {error}", { error: status.lastError })}</p>
        )}
        {actionError && <p className="provider-field__error" role="alert">{actionError}</p>}
        {readError && <p className="provider-field__error" role="alert">{readError}</p>}

        {!task && (status || readError) && (
          <div className="provider-field__row">
            {primary && (
              <button
                type="button"
                className="button button--primary button--small"
                disabled={starting}
                onClick={() => void startTask()}
              >
                {starting
                  ? <RefreshCw size={12} className="spin" />
                  : primary === "install" ? <Download size={12} /> : <ArrowUpCircle size={12} />}
                {status?.lastError
                  ? t("重试", "Retry")
                  : primary === "install" ? t("安装", "Install") : t("更新", "Update")}
              </button>
            )}
            <button
              type="button"
              className="button button--secondary button--small"
              disabled={checking || settling || starting}
              onClick={() => void check()}
            >
              <RefreshCw size={12} className={checking ? "spin" : undefined} />
              {status ? t("检查更新", "Check for updates") : t("重试", "Retry")}
            </button>
          </div>
        )}
      </section>
      {installed && children?.({ revision, freshInstall })}
    </>
  );
}
