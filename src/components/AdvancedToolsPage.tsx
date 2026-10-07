import { useId } from "react";
import { useI18n } from "../i18n";
import type { LockTone } from "../lib/toolLock";
import type { HostMessageContainer } from "../types";
import { LockableSwitchRow, LockMark, lockToneClass, type LockHints } from "./LockTone";
import { WebSearchBehaviorSettings } from "./WebSearchBehaviorSettings";
import type { WebSearchBehaviorSettingsProps } from "./WebSearchBehaviorSettings";

interface AdvancedToolsPageProps {
  /** The search and fetch backends, with the shaping and filtering under them. */
  web: WebSearchBehaviorSettingsProps;
  /**
   * The switch deciding whether this surface reaches the web at all. Omitted
   * where that decision belongs to someone else — a role's caller decides it,
   * and stays the ceiling on whatever the role's own backends say — and the
   * backend rows are then always drawn, since they are all that is left to
   * answer.
   */
  webAccess?: {
    enabled: boolean;
    tone?: LockTone | null;
    onChange: (enabled: boolean) => void;
  };
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
  webAccess,
  memory,
  hostMessages,
  lockHints
}: AdvancedToolsPageProps) {
  const { t } = useI18n();

  return (
    <>
      {/* Web access is one switch, not two tool checkboxes. Upstreams do not
          agree on how many web tools there are — Anthropic exposes search
          and fetch separately, DeepSeek and OpenAI expose search alone and
          keep page retrieval inside it — so the host derives the pair from
          this switch and the resolved backend rather than letting the picker
          promise a shape the upstream may not have. */}
      <section className="conversation-settings__field">
        {webAccess && (
          <LockableSwitchRow
            title={t("启用联网搜索", "Enable web search")}
            description={t(
              "这个对话能不能联网。具体拿到哪几个联网工具，由下面的搜索后端与抓取后端各自决定——两者可以分别指定，也可以分别关掉。",
              "Whether this conversation can reach the web at all. Which web tools it actually gets is decided by the search and fetch backends below: each names its own, and each can be turned off on its own."
            )}
            checked={webAccess.enabled}
            tone={webAccess.tone}
            hints={lockHints}
            onChange={webAccess.onChange}
            label={webAccess.enabled
              ? t("联网搜索已开启", "Web search enabled")
              : t("联网搜索已关闭", "Web search disabled")}
          />
        )}
        {!webAccess || webAccess.enabled ? (
          <div className="web-search-provider-field">
            <WebSearchBehaviorSettings {...web} />
          </div>
        ) : null}
      </section>

      {memory && (
        <section className="conversation-settings__field">
          <LockableSwitchRow
            title={t("启用全局记忆", "Enable global memory")}
            description={t(
              "开启后，~/.mewrk 的 MEWRK.md 常驻指令与 MEMORY.md 记忆索引拼进上下文，读取/创建/编辑全局记忆三个工具随之可用。",
              "When enabled, ~/.mewrk's MEWRK.md instructions and MEMORY.md index join the context, and the read/create/edit global memory tools become available."
            )}
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
            description={t(
              "开启后，当前工作区 .mewrk 的 MEWRK.md 与 MEMORY.md 拼进上下文，读取/创建/编辑项目记忆三个工具随之可用。",
              "When enabled, this workspace's .mewrk MEWRK.md and MEMORY.md join the context, and the read/create/edit project memory tools become available."
            )}
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

      {/* The five race-safe write guards used to be five switches
          here. They are unconditional now — every conversation runs
          with all five — so the section is gone rather than drawn as
          a row of controls nothing can move. */}

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
        "照 Claude Code 的做法，包在 <system-reminder> 里作为一条 user 消息送达。缺点：与你说的话同在 user 角色里，只靠标签和「这不是用户输入」的抬头区分，模型偶尔会把它当成你的话，甚至当成确认；还可能与相邻的用户消息合并成一条。",
        "Claude Code's way: a user message wrapped in <system-reminder>. Downside: it shares the user role with what you say and is told apart only by its tags and a “not user input” preamble, so a model sometimes takes it for you — even for an approval — and it may merge with an adjacent user message."
      )
    },
    {
      value: "box",
      title: t("box 工具结果", "box tool result"),
      description: t(
        "Mewrk 替模型写下一次 box 调用，消息作为它的结果送达。缺点：这是模型从没发起过的伪造调用，模型可能当成自己调的；每次请求都要多声明一个 box 工具，模型偶尔会自己去调它（什么也不做）；每条消息都多占一对调用与结果。",
        "Mewrk writes a box call for the model, and the message arrives as its result. Downside: it is a call the model never made, which it may take for its own; every request declares one more tool, box, which a model sometimes calls itself to no effect; and every message costs a call and a result."
      )
    }
  ];
  return (
    <>
      <div className={`tool-toggle-row${lockToneClass("tool-toggle-row", tone)}`}>
        <span>
          <strong>{title}</strong>
          <small>{t(
            "Mewrk 在两个回合之间交给模型的消息——没等到的后台结果、钩子补充的上下文、中途选中的技能、文件变动，以及模型不能中途追加系统提示词时的指令——装在哪里送达。子代理与工作流跟随本对话；模型支持异步工具调用时，子代理与工作流的结果改为作为启动它们的那次调用的输出。切换后，此前的宿主消息也按新容器重放。",
            "What the messages Mewrk hands the model between rounds arrive in: background results nobody waited for, context a hook added, skills selected later, files that changed, and instructions on a model that takes no system prompt mid-conversation. Subagents and workflows follow this conversation; on a model with asynchronous tool calls, a subagent's or workflow's result is the output of the call that started it instead. After a switch, earlier host messages replay in the new container too."
          )}</small>
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
