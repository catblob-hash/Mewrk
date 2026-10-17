import { RotateCcw, ShieldCheck } from "lucide-react";
import type { JSX, ReactNode } from "react";
import { useEffect, useState } from "react";
import { useI18n } from "../../i18n";
import {
  DEFAULT_SANDBOX_ALLOWLIST,
  defaultSandboxSettings,
  machineSandboxSupport,
  setupLocalSandbox,
  workspaceSandboxIgnoresCase
} from "../../lib/runtime";
import { runEnvKey } from "../../lib/workspaces";
import type {
  RunTarget,
  SandboxNetworkMode,
  SandboxSettings as SandboxSettingsType,
  SandboxSupport
} from "../../types";
import { Switch } from "../Common";
import { DocsLink } from "../DocsLink";
import "./SandboxSettings.css";

interface SandboxSettingsProps {
  /** The machine the workspace is on; `null` is this computer. */
  machine: RunTarget | null;
  /** How the page names that machine: "This machine", "WSL: Ubuntu", "SSH: devbox". */
  machineName: string;
  /** The workspace's directory on that machine. */
  path: string;
  settings: SandboxSettingsType | undefined;
  onChange: (settings: SandboxSettingsType) => void;
}

/**
 * One rule per line. The textarea is parsed on every keystroke, so it cannot
 * render the parsed value back — a blank line being typed between two rules
 * would vanish — and holds what was typed until the field is left.
 */
function ListField({
  label,
  description,
  value,
  placeholder,
  onChange
}: {
  label: string;
  description: ReactNode;
  value: readonly string[];
  placeholder: string;
  onChange: (value: string[]) => void;
}): JSX.Element {
  const [draft, setDraft] = useState<string | null>(null);
  return (
    <div className="sandbox-settings-page__row sandbox-settings-page__row--vertical">
      <div className="sandbox-settings-page__copy">
        <strong>{label}</strong>
        <small>{description}</small>
      </div>
      <textarea
        className="input sandbox-settings-page__textarea"
        aria-label={label}
        spellCheck={false}
        placeholder={placeholder}
        value={draft ?? value.join("\n")}
        onChange={(event) => {
          setDraft(event.target.value);
          onChange(event.target.value.split("\n").map((line) => line.trim()).filter(Boolean));
        }}
        onBlur={() => setDraft(null)}
      />
    </div>
  );
}

/**
 * The sandbox one workspace's commands run in: sandboxed agent processes on
 * the workspace's machine, one per conversation working there, enforced by the
 * operating system (Seatbelt on macOS, bubblewrap and seccomp on Linux and
 * WSL 2, srt-win's restricted account and firewall rules on Windows). Part of
 * the workspace's settings, beside its variables: whether the code in a
 * directory is trusted is a question about the directory. Off unless switched
 * on; a machine that cannot sandbox refuses the command rather than running it
 * unsandboxed.
 *
 * What the machine can do is asked of the agent on that machine — this
 * computer, a WSL distribution, an SSH machine — not of the one showing the
 * page. Windows needs a one-time setup with administrator rights: this
 * computer's starts here; an SSH machine's has to be run on that machine,
 * which its agent's answer says how to do.
 */
export function SandboxSettings({ machine, machineName, path, settings, onChange }: SandboxSettingsProps): JSX.Element {
  const { t } = useI18n();
  const current = settings ?? defaultSandboxSettings();
  const [support, setSupport] = useState<SandboxSupport | null | "loading">("loading");
  const [settingUp, setSettingUp] = useState(false);
  const [setupError, setSetupError] = useState<string | null>(null);
  // The machine by its key, so an equal target rebuilt on each render asks once.
  const machineKey = runEnvKey(machine);
  // biome-ignore lint/correctness/useExhaustiveDependencies: `machineKey` is `machine`'s identity.
  useEffect(() => {
    let live = true;
    setSupport("loading");
    machineSandboxSupport(machine)
      .then((result) => { if (live) setSupport(result); })
      .catch((error: unknown) => {
        if (live) setSupport({ backend: "", available: false, detail: String(error), setup: false });
      });
    return () => { live = false; };
  }, [machineKey]);
  // Only bubblewrap protects a file by its name, so only there does a directory that ignores case
  // weaken the sandbox; Seatbelt and the Windows sandbox do not depend on spelling.
  const [ignoresCase, setIgnoresCase] = useState(false);
  const bubblewrap = support !== "loading" && support !== null && support.available && support.backend === "bubblewrap";
  // biome-ignore lint/correctness/useExhaustiveDependencies: `machineKey` is `machine`'s identity.
  useEffect(() => {
    setIgnoresCase(false);
    if (!bubblewrap) return;
    let live = true;
    workspaceSandboxIgnoresCase(machine, path)
      .then((result) => { if (live) setIgnoresCase(result === true); })
      .catch(() => { /* Nothing to point out when the directory cannot be asked. */ });
    return () => { live = false; };
  }, [bubblewrap, machineKey, path]);
  const setUp = () => {
    setSettingUp(true);
    setSetupError(null);
    setupLocalSandbox()
      .then(setSupport)
      .catch((error: unknown) => setSetupError(String(error)))
      .finally(() => setSettingUp(false));
  };
  const update = (change: Partial<SandboxSettingsType>) => onChange({ ...current, ...change });
  const updateNetwork = (change: Partial<SandboxSettingsType["network"]>) =>
    onChange({ ...current, network: { ...current.network, ...change } });

  const backendName = (backend: string) => backend === "seatbelt"
    ? "Seatbelt"
    : backend === "bubblewrap"
      ? "bubblewrap"
      : backend === "srt-win"
        ? t("Windows 沙箱", "Windows sandbox")
        : backend;
  // Led by the machine's name: the answer is that machine's, whichever computer shows it.
  const status = `${machineName} · ${support === "loading"
    ? t("正在检查……", "Checking…")
    : support === null
      ? t("浏览器预览中无法检查。", "Cannot be checked in the browser preview.")
      : support.available
        ? t("可以使用：{backend}", "Available: {backend}", { backend: backendName(support.backend) })
        : t("不可用：{detail}", "Not available: {detail}", {
          detail: support.detail || backendName(support.backend)
        })}`;
  const unavailable = support !== "loading" && support !== null && !support.available;
  // Only this computer's setup can be started from here: an SSH machine's needs an
  // administrator at that machine, and its agent's answer above names the command.
  const needsSetup = unavailable && support.setup && machine === null;

  return (
    <section className="sandbox-settings-page">
      <section className="settings-card">
        <div className="sandbox-settings-page__row">
          <div className="sandbox-settings-page__copy">
            <strong>{t("在沙箱中运行命令", "Run commands in the sandbox")}</strong>
            <small className={unavailable ? "sandbox-settings-page__status sandbox-settings-page__status--unavailable" : "sandbox-settings-page__status"}>{status}</small>
          </div>
          <Switch
            checked={current.enabled}
            label={t("在沙箱中运行命令", "Run commands in the sandbox")}
            onChange={(enabled) => update({ enabled })}
          />
        </div>
        {ignoresCase && (
          <div className="sandbox-settings-page__row">
            <div className="sandbox-settings-page__copy">
              <strong>{t("这个目录不区分大小写", "This directory ignores case")}</strong>
              <small className="sandbox-settings-page__status--warning" role="note">{t(
                "它所在的文件系统不区分文件名的大小写（WSL 里的 Windows 磁盘就是这样）。Linux 沙箱只能按确切的名字保护文件，所以在这里，.envrc、.mcp.json、.git/config 这类会在沙箱外执行的文件保护不完整，沙箱里的命令可能改到它们；受保护的目录（.mewrk、.vscode、.git/hooks）不受影响，其余限制照常。要完整保护，把工作区放到 Linux 自己的文件系统里，例如 WSL 的主目录下。",
                "Its file system does not tell file names apart by case (Windows drives in WSL work this way). The Linux sandbox can protect a file only by its exact name, so here the files that run outside the sandbox, such as .envrc, .mcp.json and .git/config, are not fully protected: sandboxed commands may change them. Protected directories (.mewrk, .vscode, .git/hooks) are unaffected, and every other limit still applies. For full protection, keep the workspace on a Linux file system, such as your home directory in WSL."
              )}</small>
            </div>
          </div>
        )}
        {needsSetup && (
          <div className="sandbox-settings-page__row">
            <div className="sandbox-settings-page__copy">
              <strong>{t("设置 Windows 沙箱", "Set up the Windows sandbox")}</strong>
              <small>
                {t("一次性设置，需要管理员批准。", "One-time setup; needs administrator approval.")}
                <DocsLink inline page="security" section="sandbox" />
              </small>
              {setupError && (
                <small className="sandbox-settings-page__status sandbox-settings-page__status--unavailable" role="alert">
                  {setupError}
                </small>
              )}
            </div>
            <button type="button" className="button button--primary button--small" disabled={settingUp} onClick={setUp}>
              <ShieldCheck size={12} aria-hidden="true" /> {settingUp ? t("正在设置……", "Setting up…") : t("设置", "Set up")}
            </button>
          </div>
        )}
        <div className="sandbox-settings-page__row sandbox-settings-page__row--vertical">
          <div className="sandbox-settings-page__copy">
            <strong>{t("沙箱里的命令", "What sandboxed commands can do")}</strong>
            <ul className="sandbox-settings-page__list">
              <li>{t(
                "只能写这个工作区、临时目录和包缓存。",
                "Write only to this workspace, temp folders and package caches."
              )}</li>
              <li>{t(
                "读不到 SSH 密钥、令牌等凭据。",
                "Cannot read credentials such as SSH keys and tokens."
              )}</li>
              <li>{t(
                "只能经代理联网，遵循下面的网络规则。",
                "Reach the network only through a proxy, by the rules below."
              )}</li>
              <li>{t(
                "看不到沙箱外的进程。",
                "Cannot see processes outside the sandbox."
              )}</li>
            </ul>
            <small>
              {t(
                "适用于 shell 命令、后台命令和文件工具。",
                "Applies to shell commands, background commands and file tools."
              )}
              <DocsLink inline page="security" section="sandbox" />
            </small>
          </div>
        </div>
      </section>
      <section className="settings-card">
        <div className="sandbox-settings-page__row">
          <div className="sandbox-settings-page__copy">
            <strong>{t("网络", "Network")}</strong>
            <small>{t(
              "放行一个主机，就放行经它的一切。",
              "Allowing a host allows everything done through it."
            )}</small>
          </div>
          <select
            className="input"
            aria-label={t("网络", "Network")}
            value={current.network.mode}
            onChange={(event) => updateNetwork({ mode: event.target.value as SandboxNetworkMode })}
          >
            <option value="allowlist">{t("只允许白名单", "Allowlist only")}</option>
            <option value="open">{t("任意公网主机", "Any public host")}</option>
            <option value="off">{t("不联网", "No network")}</option>
          </select>
        </div>
        {current.network.mode === "allowlist" && (
          <>
            <ListField
              label={t("白名单", "Allowlist")}
              description={<>
                {t(
                  "一行一个主机，可通配子域、可加端口。",
                  "One host per line; subdomain wildcards and ports allowed."
                )}
                <DocsLink inline page="security" section="sandbox" />
              </>}
              value={current.network.allow}
              placeholder={"github.com\n*.npmjs.org\nlocalhost:5432"}
              onChange={(allow) => updateNetwork({ allow })}
            />
            <div className="sandbox-settings-page__actions">
              <button
                type="button"
                className="button button--ghost"
                onClick={() => updateNetwork({ allow: [...DEFAULT_SANDBOX_ALLOWLIST] })}
              >
                <RotateCcw size={12} aria-hidden="true" /> {t("恢复默认白名单", "Restore the default allowlist")}
              </button>
            </div>
          </>
        )}
        {current.network.mode !== "off" && (
          <ListField
            label={t("黑名单", "Blocklist")}
            description={t("任何模式下都不放行，先于白名单判断。", "Never allowed, in any mode; checked before the allowlist.")}
            value={current.network.deny}
            placeholder={"*.example.com"}
            onChange={(deny) => updateNetwork({ deny })}
          />
        )}
      </section>
      <section className="settings-card">
        <ListField
          label={t("额外可写目录", "Further writable directories")}
          description={t(
            "工作区所在机器上的绝对路径或 ~ 路径。",
            "Absolute or ~ paths on the workspace's machine."
          )}
          value={current.writable}
          placeholder={"~/shared-data"}
          onChange={(writable) => update({ writable })}
        />
        <ListField
          label={t("额外禁读路径", "Further unreadable paths")}
          description={t(
            "内置凭据位置之外，沙箱也读不到这些。",
            "Also unreadable, beyond the built-in credential locations."
          )}
          value={current.denyRead}
          placeholder={"~/secrets"}
          onChange={(denyRead) => update({ denyRead })}
        />
      </section>
    </section>
  );
}
