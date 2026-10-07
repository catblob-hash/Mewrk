import { CheckCircle2, CircleAlert, Copy, RefreshCw, TerminalSquare } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { TranslationFunction } from "../../i18n";
import { useI18n } from "../../i18n";
import { CLAUDE_AGENT_LEGAL_URL } from "../../lib/claudeAgentProvider";
import { claudeAgentLoginStatus, claudeAgentOpenLogin } from "../../lib/runtime";
import type { ApiProvider, ClaudeAgentLoginStatus } from "../../types";
import { useSettingsSession } from "../settingsSession";
import { useWindowRefocus } from "./useWindowRefocus";

function subscriptionLabel(subscriptionType: string | null): string {
  if (!subscriptionType) return "";
  return `${subscriptionType.slice(0, 1).toUpperCase()}${subscriptionType.slice(1)}`;
}

/**
 * Signed-in provenance line: which login the CLI is using. `console` means the
 * CLI is on a Console account and every request is billed per token, which is a
 * materially different deal from a subscription and must not read the same.
 */
function authMethodLabel(t: TranslationFunction, status: ClaudeAgentLoginStatus): string {
  if (status.authMethod === "console") return t("Console（API 计费）", "Console (API billing)");
  return subscriptionLabel(status.subscriptionType);
}

export function ClaudeAgentLoginPanel({
  provider,
  desktopRuntime,
  onSignedInChange,
  onBeforeHostCall,
  refreshToken = 0,
  enableWhenSignedIn = false,
}: {
  provider: ApiProvider;
  desktopRuntime: boolean;
  onSignedInChange: (signedIn: boolean) => void;
  /**
   * Reads the status again, keeping what is shown until the answer arrives, each
   * time this changes (not on mount). The components panel moves it when an
   * install or update ends: the CLI that answers is a different one.
   */
  refreshToken?: number;
  /**
   * The CLI was just installed from this page: a first read that finds a login is
   * the moment the provider becomes usable, so it enables the row as a sign-in would.
   */
  enableWhenSignedIn?: boolean;
  /**
   * Flushes pending document edits. The host resolves the login commands against
   * the persisted provider row, so an unsaved row would be rejected as "settings
   * not saved yet" until the debounce fires.
   */
  onBeforeHostCall?: () => Promise<void>;
}) {
  const { t } = useI18n();
  const session = useSettingsSession();
  const sessionKey = `claude-agent:login:${provider.id}`;
  // The first time the page is shown in a settings session it asks the host; showing it
  // again in the same session starts from that answer and asks nothing. Only a window
  // regaining focus, a finished install, or the Re-check button asks again.
  const [status, setStatus] = useState<ClaudeAgentLoginStatus | null>(
    () => session.peek<ClaudeAgentLoginStatus>(sessionKey) ?? null
  );
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const mountedRef = useRef(true);
  const requestTokenRef = useRef(0);
  // Settings edits mint a new provider object on every keystroke; only a change
  // of identity should reset the panel, so effects key on the id and read the
  // latest object through this ref.
  const providerRef = useRef(provider);
  providerRef.current = provider;
  const onSignedInChangeRef = useRef(onSignedInChange);
  onSignedInChangeRef.current = onSignedInChange;
  const onBeforeHostCallRef = useRef(onBeforeHostCall);
  onBeforeHostCallRef.current = onBeforeHostCall;
  const enableWhenSignedInRef = useRef(enableWhenSignedIn);
  enableWhenSignedInRef.current = enableWhenSignedIn;
  // The last status the panel showed. A re-check that turns "signed out" into
  // "signed in" enables the provider; an already-signed-in first read is not a
  // transition and must leave a deliberately disabled row alone.
  const lastStatusRef = useRef<ClaudeAgentLoginStatus | null>(status);
  // False while the first read is still running: it answers for any refresh asked meanwhile.
  const settledRef = useRef(status !== null);
  const showStatus = useCallback((next: ClaudeAgentLoginStatus) => {
    const previous = lastStatusRef.current;
    lastStatusRef.current = next;
    setStatus(next);
    if (next.signedIn && (previous ? !previous.signedIn : enableWhenSignedInRef.current)) {
      onSignedInChangeRef.current(true);
    }
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      requestTokenRef.current += 1;
    };
  }, []);

  /**
   * Asks the host now, whatever the session already holds. `blank` drops the panel to
   * "loading" first (the Re-check and Retry buttons); otherwise it keeps showing what
   * it has until the answer arrives, and a failure changes nothing (the focus and
   * install-finished refreshes: Re-check reports a failure in full).
   */
  const readNow = useCallback(async ({ blank }: { blank: boolean }) => {
    const token = requestTokenRef.current + 1;
    requestTokenRef.current = token;
    const current = () => mountedRef.current && requestTokenRef.current === token;
    if (blank) {
      setStatus(null);
      setPending(false);
      setError(null);
      setCopied(false);
    }
    try {
      await onBeforeHostCallRef.current?.();
      if (!current()) return;
      const next = await claudeAgentLoginStatus(providerRef.current);
      if (!current()) return;
      setError(null);
      session.put(sessionKey, next);
      showStatus(next);
    } catch (reason) {
      if (current() && blank) setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      if (current()) settledRef.current = true;
    }
  }, [session, sessionKey, showStatus]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: the effect reads the live provider through providerRef; only a change of provider identity restarts it.
  useEffect(() => {
    const token = requestTokenRef.current + 1;
    requestTokenRef.current = token;
    setPending(false);
    setError(null);
    setCopied(false);
    // A preview has no CLI to ask; asking would only produce a bridge error.
    if (!desktopRuntime) {
      setStatus(null);
      return;
    }
    const cached = session.peek<ClaudeAgentLoginStatus>(sessionKey);
    if (cached) {
      lastStatusRef.current = cached;
      settledRef.current = true;
      setStatus(cached);
      return;
    }
    setStatus(null);
    settledRef.current = false;

    const current = () => mountedRef.current && requestTokenRef.current === token;
    void (async () => {
      try {
        // Joins a read an earlier visit to this page left running.
        const next = await session.load(sessionKey, async () => {
          await onBeforeHostCallRef.current?.();
          return claudeAgentLoginStatus(providerRef.current);
        });
        if (!current()) return;
        showStatus(next);
      } catch (reason) {
        if (current()) setError(reason instanceof Error ? reason.message : String(reason));
      } finally {
        if (current()) settledRef.current = true;
      }
    })();
  }, [provider.id, desktopRuntime]);

  // Coming back to the window — from the terminal the sign-in ran in, or after
  // long enough away for the login to have lapsed — asks again, keeping what the
  // panel shows until the answer is in rather than blanking it on every focus.
  useWindowRefocus(desktopRuntime, () => {
    // The first read is still running; it answers for this focus too.
    if (!settledRef.current) return;
    void readNow({ blank: false });
  });

  // The components panel moves this when an install or update ends.
  const seenRefreshTokenRef = useRef(refreshToken);
  useEffect(() => {
    if (seenRefreshTokenRef.current === refreshToken) return;
    seenRefreshTokenRef.current = refreshToken;
    if (desktopRuntime && settledRef.current) void readNow({ blank: false });
  }, [refreshToken, desktopRuntime, readNow]);

  const openLogin = async () => {
    const token = requestTokenRef.current + 1;
    requestTokenRef.current = token;
    setPending(true);
    setError(null);
    try {
      await onBeforeHostCallRef.current?.();
      if (!mountedRef.current || requestTokenRef.current !== token) return;
      await claudeAgentOpenLogin(providerRef.current);
    } catch (reason) {
      if (!mountedRef.current || requestTokenRef.current !== token) return;
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      if (mountedRef.current && requestTokenRef.current === token) setPending(false);
    }
  };

  /* The host words the command (`agent.rs::login_command`): Mewrk's own Claude
     Code by absolute path, quoted for this platform's shell, so it runs in any
     terminal whether or not the user ever installed Claude Code. */
  const copyCommand = async (command: string) => {
    setError(null);
    setCopied(false);
    try {
      await navigator.clipboard.writeText(command);
      if (mountedRef.current) setCopied(true);
    } catch (reason) {
      if (!mountedRef.current) return;
      setError(t("复制命令失败：{reason}", "Could not copy the command: {reason}", {
        reason: reason instanceof Error ? reason.message : String(reason),
      }));
    }
  };

  const compliance = (
    <p className="provider-field__help">
      {t(
        "Mewrk 复用你本机 Claude Code 的登录状态，仅供本人使用。",
        "Mewrk reuses the Claude Code login on this machine, for your own use only."
      )}{" "}
      <a href={CLAUDE_AGENT_LEGAL_URL} target="_blank" rel="noreferrer">
        {t("Claude Code 使用条款", "Claude Code terms")}
      </a>
    </p>
  );

  const recheck = (
    <button
      type="button"
      className="button button--secondary button--small"
      onClick={() => void readNow({ blank: true })}
    ><RefreshCw size={12} />{t("重新检查", "Re-check")}</button>
  );

  if (!desktopRuntime) {
    return (
      <section className="provider-field provider-login">
        <div className="provider-login__summary">
          <CircleAlert size={16} aria-hidden="true" />
          <div>
            <div className="provider-field__title">{t("使用本机 Claude Code 的登录", "Uses the Claude Code login on this machine")}</div>
            <p className="provider-field__help">{t(
              "浏览器预览无法读取 Claude Code 登录状态。",
              "Browser preview cannot read the Claude Code sign-in status."
            )}</p>
          </div>
        </div>
        {compliance}
      </section>
    );
  }

  if (!status) {
    return (
      <section className="provider-field provider-login">
        <p className="provider-field__help">{error
          ? t("读取 Claude Code 登录状态失败。", "Could not read the Claude Code sign-in status.")
          : t("正在读取登录状态…", "Loading sign-in status…")}</p>
        {error && <p className="provider-field__error" role="alert">{error}</p>}
        {error && <>
          <p className="provider-field__help">{t(
            "Mewrk 用的是上面安装的 Claude Code；读不到通常是安装不完整（可以重新安装），或者本机 ~/.claude 不可读。",
            "Mewrk uses the Claude Code installed above; a failure here usually means an incomplete install (reinstalling fixes it), or that ~/.claude is unreadable."
          )}</p>
          <div className="provider-field__row">
            <button
              type="button"
              className="button button--secondary button--small"
              onClick={() => void readNow({ blank: true })}
            >{t("重试", "Retry")}</button>
          </div>
        </>}
        {compliance}
      </section>
    );
  }

  if (status.signedIn) {
    return (
      <section className="provider-field provider-login provider-login--signed-in">
        <div className="provider-login__summary">
          <CheckCircle2 size={16} aria-hidden="true" />
          <div>
            <div className="provider-field__title">{t("已登录 Claude Code", "Signed in to Claude Code")}</div>
            <p className="provider-field__help">
              {[status.email, status.orgName, authMethodLabel(t, status)].filter(Boolean).join(" · ")}
            </p>
          </div>
        </div>
        <div className="provider-field__row">
          {recheck}
        </div>
        {error && <p className="provider-field__error" role="alert">{error}</p>}
        {compliance}
      </section>
    );
  }

  return (
    <section className="provider-field provider-login">
      <div className="provider-login__summary">
        <CircleAlert size={16} aria-hidden="true" />
        <div>
          <div className="provider-field__title">{t("使用本机 Claude Code 的登录", "Uses the Claude Code login on this machine")}</div>
          <p className="provider-field__help">{t(
            "Mewrk 用的是上面安装的 Claude Code，不需要另外安装 claude 命令。打开终端登录，或把下面的命令复制到任意终端里运行（claude.ai 订阅或 Console 账号都可以）。已经在用 Claude Code 的话，在那边登录也一样，两者共用同一份登录。Mewrk 不读、不搬、也不转发任何凭据。",
            "Mewrk uses the Claude Code installed above, so there is no separate claude command to install. Open a terminal to sign in, or copy the command below into any terminal (a claude.ai subscription or a Console account both work). If you already use Claude Code, signing in there works too: both share one login. Mewrk never reads, copies, or forwards any credential."
          )}</p>
        </div>
      </div>
      <div className="provider-field__row">
        <button
          type="button"
          className="button button--primary button--small"
          disabled={pending}
          onClick={() => void openLogin()}
        >{pending ? <RefreshCw size={12} className="spin" /> : <TerminalSquare size={12} />}{t("打开终端登录", "Open a terminal to sign in")}</button>
        <button
          type="button"
          className="button button--secondary button--small"
          onClick={() => void copyCommand(status.loginCommand)}
        ><Copy size={12} />{copied
          ? t("已复制", "Copied")
          : t("复制命令", "Copy command")}</button>
        {recheck}
      </div>
      <p className="provider-field__help provider-field__help--code">{status.loginCommand}</p>
      {error && <p className="provider-field__error" role="alert">{error}</p>}
      {compliance}
    </section>
  );
}
