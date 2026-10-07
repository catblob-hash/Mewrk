import { KeyRound, ShieldQuestion } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useI18n } from "../i18n";
import { errorMessage } from "../lib/errors";
import type { SshPrompt } from "../lib/sshPrompts";
import { Dialog } from "./Common";
import "./SshPromptDialog.css";

/**
 * What an SSH connection is waiting on the user for, asked in the app rather than on a terminal
 * nobody sees: whether to trust a host key met for the first time, or a password, passphrase or PIN.
 *
 * Turning a question down tells the host not to ask again for that machine until it is probed from
 * its settings, so a connection that keeps retrying does not keep putting the dialog back.
 */
export function SshPromptDialog({
  prompt,
  onAnswer
}: {
  prompt: SshPrompt;
  /** The answer: the secret typed, any value to accept or allow, or `null` to turn it down. */
  onAnswer: (answer: string | null) => Promise<void>;
}) {
  const { t } = useI18n();
  const [secret, setSecret] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const secretField = useRef<HTMLInputElement>(null);
  // After the window's own effect has focused its panel: this dialog exists to take the one answer.
  useEffect(() => {
    secretField.current?.focus();
  }, []);

  const send = async (answer: string | null) => {
    setSending(true);
    setError(null);
    try {
      await onAnswer(answer);
    } catch (reason) {
      setError(errorMessage(reason, t("无法回答这个询问", "Could not answer this question")));
      setSending(false);
    }
  };

  const machine = prompt.machine;
  if (prompt.kind === "hostKey") {
    const key = prompt.hostKey;
    return (
      <Dialog
        title={t("确认 {machine} 的主机密钥", "Confirm the host key of {machine}", { machine })}
        width="480px"
        dismissible={false}
        onClose={() => void send(null)}
        footer={<>
          <button type="button" className="button button--secondary" disabled={sending} onClick={() => void send(null)}>
            {t("拒绝", "Reject")}
          </button>
          <button type="button" className="button button--primary" disabled={sending} onClick={() => void send("yes")}>
            {t("接受", "Accept")}
          </button>
        </>}
      >
        <p className="confirm-copy">
          {t(
            "Mewrk 第一次连接这台机器，它出示了下面这个主机密钥。只有确认这是那台机器自己的密钥时才接受，例如与它的管理员给你的指纹一致。",
            "Mewrk is connecting to this machine for the first time, and it presented the host key below. Accept it only if you know it is that machine's own key, for example because it matches the fingerprint its administrator gave you."
          )}
        </p>
        {key
          ? (
            <dl className="ssh-prompt__key">
              <dt>{t("主机", "Host")}</dt>
              <dd>{key.host}</dd>
              <dt>{t("密钥类型", "Key type")}</dt>
              <dd>{key.keyType}</dd>
              <dt>{t("指纹", "Fingerprint")}</dt>
              <dd><code>{key.fingerprint}</code></dd>
            </dl>
          )
          : <pre className="ssh-prompt__raw">{prompt.prompt}</pre>}
        <div className="safe-boundary-note">
          <ShieldQuestion size={15} />
          <span>
            {t(
              "接受的密钥会加入 ~/.ssh/known_hosts。以后这台机器出示的密钥若与它不同，Mewrk 一律拒绝连接。",
              "An accepted key is added to ~/.ssh/known_hosts. If the machine ever presents a different key, Mewrk refuses to connect."
            )}
          </span>
        </div>
        {error && <p className="field__hint field__hint--error" role="alert">{error}</p>}
      </Dialog>
    );
  }

  if (prompt.kind === "secret") {
    return (
      <Dialog
        title={t("登录 {machine}", "Sign in to {machine}", { machine })}
        width="440px"
        dismissible={false}
        onClose={() => void send(null)}
        footer={<>
          <button type="button" className="button button--secondary" disabled={sending} onClick={() => void send(null)}>
            {t("取消", "Cancel")}
          </button>
          <button type="submit" form={`ssh-prompt-${prompt.id}`} className="button button--primary" disabled={sending}>
            {t("登录", "Sign in")}
          </button>
        </>}
      >
        <form
          id={`ssh-prompt-${prompt.id}`}
          className="ssh-prompt__form"
          onSubmit={(event) => {
            event.preventDefault();
            void send(secret);
          }}
        >
          <label className="field">
            <span className="field__label">{prompt.prompt}</span>
            <input
              ref={secretField}
              className="input"
              type="password"
              autoComplete="off"
              spellCheck={false}
              value={secret}
              onChange={(event) => setSecret(event.target.value)}
            />
            {prompt.retry && (
              <span className="field__hint field__hint--error">
                {t("上次输入的没有被接受，请重新输入。", "That was not accepted; try again.")}
              </span>
            )}
          </label>
        </form>
        <div className="safe-boundary-note">
          <KeyRound size={15} />
          <span>
            {t(
              "Mewrk 只把它保存在内存里，直到退出，用于再次连接这台机器；不会写入磁盘。",
              "Mewrk keeps it in memory only, until it quits, to connect to this machine again; it is never written to disk."
            )}
          </span>
        </div>
        {error && <p className="field__hint field__hint--error" role="alert">{error}</p>}
      </Dialog>
    );
  }

  const confirm = prompt.kind === "confirm";
  return (
    <Dialog
      title={confirm
        ? t("{machine} 请求确认", "{machine} asks for confirmation", { machine })
        : t("{machine} 在等待", "{machine} is waiting", { machine })}
      width="440px"
      dismissible={!confirm}
      onClose={() => void send(null)}
      footer={confirm
        ? <>
          <button type="button" className="button button--secondary" disabled={sending} onClick={() => void send(null)}>
            {t("拒绝", "Deny")}
          </button>
          <button type="button" className="button button--primary" disabled={sending} onClick={() => void send("yes")}>
            {t("允许", "Allow")}
          </button>
        </>
        : (
          <button type="button" className="button button--secondary" disabled={sending} onClick={() => void send(null)}>
            {t("关闭", "Close")}
          </button>
        )}
    >
      <pre className="ssh-prompt__raw">{prompt.prompt}</pre>
      {error && <p className="field__hint field__hint--error" role="alert">{error}</p>}
    </Dialog>
  );
}
