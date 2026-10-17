import { useId } from "react";
import { useI18n } from "../i18n";
import type { LockTone } from "../lib/toolLock";
import type { HostMessageContainer } from "../types";
import { DocsLink } from "./DocsLink";
import { LockableSwitchRow, LockMark, lockToneClass, type LockHints } from "./LockTone";
import { WebSearchBehaviorSettings } from "./WebSearchBehaviorSettings";
import type { WebSearchBehaviorSettingsProps } from "./WebSearchBehaviorSettings";

interface AdvancedToolsPageProps {
  /**
   * The web search and web fetch switches, each with its backend and the
   * shaping and filtering under it. Where the surface decides web access at all
   * it rides in here too (`webAccess`), written by the two switches.
   */
  web: WebSearchBehaviorSettingsProps;
  /** The two memory tiers. Omitted where memory is not a switch of this surface. */
  memory?: {
    global: boolean;
    project: boolean;
    globalTone?: LockTone | null;
    projectTone?: LockTone | null;
    onChangeGlobal: (enabled: boolean) => void;
    onChangeProject: (enabled: boolean) => void;
  };
  /**
   * The file write guards, one switch for all of them. Omitted where the
   * surface does not decide it: a role's runs follow the conversation that
   * spawns them.
   */
  fileWriteGuards?: {
    enabled: boolean;
    tone?: LockTone | null;
    onChange: (enabled: boolean) => void;
  };
  /**
   * Subagent communication, one switch for both directions (`send_message` for
   * the main agent, `subagent_update` for its subagents). Omitted where the
   * surface does not decide it: a role's runs follow the conversation that
   * spawns them.
   */
  subagentMessaging?: {
    enabled: boolean;
    tone?: LockTone | null;
    onChange: (enabled: boolean) => void;
  };
  /**
   * Whether the system prompt opens with the environment section. Omitted where
   * the surface does not decide it: a role's runs follow the conversation that
   * spawns them.
   */
  environmentPrompt?: {
    enabled: boolean;
    tone?: LockTone | null;
    onChange: (enabled: boolean) => void;
  };
  /**
   * What the host's messages to the model come in. Omitted where the surface
   * does not decide it: a role's runs follow the conversation that spawns them.
   */
  hostMessages?: {
    value: HostMessageContainer;
    tone?: LockTone | null;
    onChange: (value: HostMessageContainer) => void;
  };
  /** What a row the conversation's lock tones says about itself. */
  lockHints?: LockHints;
}

/**
 * The advanced tools page: what a model is handed through a switch rather than
 * picked row by row from the tool list, which has a page of its own.
 *
 * One page for every surface that asks these questions — a conversation, a
 * preset opened in its window, and a subagent role — so a row added or
 * reworded here lands on all of them at once. A surface that cannot answer a
 * section leaves that section's prop out, and the section is not drawn: that
 * is the only way the three differ.
 */
export function AdvancedToolsPage({
  web,
  memory,
  fileWriteGuards,
  subagentMessaging,
  environmentPrompt,
  hostMessages,
  lockHints
}: AdvancedToolsPageProps) {
  const { t } = useI18n();

  return (
    <>
      {/* The two web tools are a switch each, not tool checkboxes. Upstreams do
          not agree on how many web tools there are — Anthropic exposes search
          and fetch separately, DeepSeek and OpenAI expose search alone and
          keep page retrieval inside it — so the host derives the pair from
          these switches and the backend under each rather than letting the
          tool list promise a shape the upstream may not have. */}
      <section className="conversation-settings__field">
        <WebSearchBehaviorSettings lockHints={lockHints} {...web} />
      </section>

      {memory && (
        <section className="conversation-settings__field">
          <LockableSwitchRow
            title={t("启用全局记忆", "Enable global memory")}
            description={<>
              {t("接入全局记忆及其工具。", "Brings in global memory and its tools.")}
              <DocsLink inline page="memory" section="memory-switches" />
            </>}
            checked={memory.global}
            tone={memory.globalTone}
            hints={lockHints}
            onChange={memory.onChangeGlobal}
            label={memory.global
              ? t("全局记忆已开启", "Global memory enabled")
              : t("全局记忆已关闭", "Global memory disabled")}
          />
          <LockableSwitchRow
            title={t("启用项目记忆", "Enable project memory")}
            description={<>
              {t("接入项目记忆及其工具。", "Brings in project memory and its tools.")}
              <DocsLink inline page="memory" section="memory-switches" />
            </>}
            checked={memory.project}
            tone={memory.projectTone}
            hints={lockHints}
            onChange={memory.onChangeProject}
            label={memory.project
              ? t("项目记忆已开启", "Project memory enabled")
              : t("项目记忆已关闭", "Project memory disabled")}
          />
        </section>
      )}

      {/* The five race-safe write guards — read-before-write, the stale-write
          refusal, external-change notices, hook re-sync and the formatter
          hint — are one switch: they stand or fall together with the rules
          the edit and write descriptions carry for them. */}
      {fileWriteGuards && (
        <section className="conversation-settings__field">
          <LockableSwitchRow
            title={t("启用文件防误写保护", "Enable file write guards")}
            description={<>
              {t(
                "写入前须先读过，并拦下过期写入。",
                "Requires a read before a write and refuses stale writes."
              )}
              <DocsLink inline page="working" section="advanced-tools" />
            </>}
            checked={fileWriteGuards.enabled}
            tone={fileWriteGuards.tone}
            hints={lockHints}
            onChange={fileWriteGuards.onChange}
            label={fileWriteGuards.enabled
              ? t("文件防误写保护已开启", "File write guards enabled")
              : t("文件防误写保护已关闭", "File write guards disabled")}
          />
        </section>
      )}

      {/* One switch for both ends of the channel: the main agent's send_message
          and the subagents' subagent_update exist together or not at all, and
          only while the agent_spawn tool is on. */}
      {subagentMessaging && (
        <section className="conversation-settings__field">
          <LockableSwitchRow
            title={t("子代理通信", "Subagent communication")}
            description={<>
              {t(
                "让主代理与子代理互相发消息。",
                "Lets the main agent and subagents message each other."
              )}
              <DocsLink inline page="agents" section="subagents" />
            </>}
            checked={subagentMessaging.enabled}
            tone={subagentMessaging.tone}
            hints={lockHints}
            onChange={subagentMessaging.onChange}
            label={subagentMessaging.enabled
              ? t("子代理通信已开启", "Subagent communication enabled")
              : t("子代理通信已关闭", "Subagent communication disabled")}
          />
        </section>
      )}

      {/* The environment section heads the system prompt: facts a model would
          otherwise spend a tool call on. Off drops the whole section, the
          numbered workspace list included. */}
      {environmentPrompt && (
        <section className="conversation-settings__field">
          <LockableSwitchRow
            title={t("拼接环境信息", "Include environment info")}
            description={<>
              {t(
                "提示词开头附上工作目录、系统与日期。",
                "Adds the working directory, OS and date to the prompt."
              )}
              <DocsLink inline page="memory" section="injected" />
            </>}
            checked={environmentPrompt.enabled}
            tone={environmentPrompt.tone}
            hints={lockHints}
            onChange={environmentPrompt.onChange}
            label={environmentPrompt.enabled
              ? t("环境信息已拼接", "Environment info included")
              : t("环境信息未拼接", "Environment info left out")}
          />
        </section>
      )}

      {hostMessages && (
        <section className="conversation-settings__field">
          <HostMessageContainerRows {...hostMessages} hints={lockHints} />
        </section>
      )}
    </>
  );
}

/**
 * The host-message container: a heading row that carries the lock, then one
 * row per container, each saying what it costs. One of two, so a radio group
 * rather than a switch: neither is the "off" of the other.
 */
function HostMessageContainerRows({
  value,
  tone = null,
  onChange,
  hints
}: NonNullable<AdvancedToolsPageProps["hostMessages"]> & { hints?: LockHints }) {
  const { t } = useI18n();
  const name = useId();
  const note = tone && hints?.[tone];
  const title = t("宿主消息容器", "Host message container");
  const options: Array<{ value: HostMessageContainer; title: string; description: string }> = [
    {
      value: "user",
      title: t("user 消息", "User message"),
      description: t(
        "Claude Code 的做法；可能被当成你说的话。",
        "Claude Code's way; may be mistaken for something you said."
      )
    },
    {
      value: "box",
      title: t("box 工具结果", "box tool result"),
      description: t(
        "作为 box 调用的结果；模型可能当成自己调的。",
        "Arrives as a box call result; the model may think it called it."
      )
    }
  ];
  return (
    <>
      <div className={`tool-toggle-row${lockToneClass("tool-toggle-row", tone)}`}>
        <span>
          <strong>{title}</strong>
          <small>
            {t("Mewrk 交给模型的消息用什么送达。", "What Mewrk's messages to the model arrive in.")}
            <DocsLink inline page="context" section="host-messages" />
          </small>
          {note && <small className={`lock-note${lockToneClass("lock-note", tone)}`}>{note}</small>}
        </span>
        <LockMark tone={tone} />
      </div>
      <div role="radiogroup" aria-label={title}>
        {options.map((option) => (
          <label key={option.value} className="tool-toggle-row choice-row">
            <span>
              <strong>{option.title}</strong>
              <small>{option.description}</small>
            </span>
            <input
              type="radio"
              name={name}
              value={option.value}
              checked={value === option.value}
              onChange={() => onChange(option.value)}
            />
          </label>
        ))}
      </div>
    </>
  );
}
