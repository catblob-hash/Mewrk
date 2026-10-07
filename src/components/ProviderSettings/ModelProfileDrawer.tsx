import { useState } from "react";
import { useI18n } from "../../i18n";
import {
  asyncToolsTakeEffect,
  knownAsyncTools,
  knownNativeCompaction,
  nativeCompactionTakesEffect,
  knownProtocolCapabilities,
  knownSystemAppend,
  knownToolAppend,
  normalizeCapabilities,
  promptCacheTakesEffect,
  reasoningContentTakesEffect,
  REASONING_CONTENTS,
  systemAppendTakesEffect,
  toolAppendTakesEffect
} from "../../lib/modelCapabilities";
import type { ModelCapability, ProviderFamily, ModelProfile, ReasoningContent } from "../../types";
import { DEFAULT_CACHE_TTL_MINUTES } from "../../lib/toolLock";
import { Field, Switch } from "../Common";
import { capabilityIcon, capabilityLabel } from "./capabilityMeta";
import { Drawer } from "./Drawer";

function optionalPositiveInteger(value: string): number | undefined {
  if (!value.trim()) return undefined;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? Math.max(1, Math.floor(parsed)) : undefined;
}

/** The capabilities Mewrk declares itself where it knows the model (`knownProtocolCapabilities`). */
const PROTOCOL_CAPABILITIES: readonly ModelCapability[] = [
  "tool_append",
  "system_append",
  "async_tools",
  "native_compaction"
];

function withCapability(capabilities: readonly ModelCapability[], capability: ModelCapability, on: boolean) {
  return normalizeCapabilities(on
    ? [...capabilities, capability]
    : capabilities.filter((entry) => entry !== capability));
}

export function ModelProfileDrawer({
  providerName,
  family,
  baseUrl,
  mode,
  draft,
  idError,
  onChange,
  onClose,
  onSave
}: {
  providerName: string;
  family: ProviderFamily;
  /** The provider's address: what Mewrk knows about appending depends on it. */
  baseUrl: string;
  mode: "create" | "edit";
  draft: ModelProfile;
  idError: string | null;
  onChange: (draft: ModelProfile) => void;
  onClose: () => void;
  onSave: () => void;
}) {
  const { t } = useI18n();
  /** The form matters only on the three Responses-shaped families, and even there it never
   * changes what is asked for: the request always includes `reasoning.encrypted_content`
   * (`aisdk/step.rs`) so thinking replays across tool calls. It only decides how the
   * thinking that streams back is read and shown. */
  const reasoningHint = reasoningContentTakesEffect(family)
    ? draft.reasoningContent === "plaintext"
      ? t(
          "Mewrk 总会向上游请求加密思考字段，让思考能在工具调用之间回放；这一项只决定思考怎样读取和显示。明文：端点流式返回可读的思考文字时（许多中转站和 DeepSeek 都是这样），Mewrk 把这些文字显示为思考。",
          "Mewrk always asks for the encrypted reasoning field so thinking replays across tool calls; this setting only decides how thinking is read and shown. Plaintext: for endpoints that stream readable reasoning text (many relays, and DeepSeek), Mewrk shows that text as thinking."
        )
      : t(
          "Mewrk 总会向上游请求加密思考字段，让思考能在工具调用之间回放；这一项只决定思考怎样读取和显示。端点流式返回可读的思考文字时（许多中转站和 DeepSeek 都是这样），选明文才能把这些文字显示为思考。",
          "Mewrk always asks for the encrypted reasoning field so thinking replays across tool calls; this setting only decides how thinking is read and shown. Choose Plaintext for endpoints that stream readable reasoning text (many relays, and DeepSeek) to see that text as thinking."
        )
    : t(
        "这一项只对 OpenAI Responses、OpenAI Codex 和 Azure OpenAI 有效。别的协议里思考怎么回来由上游单方面决定，客户端没有可发的开关。",
        "This setting only takes effect on OpenAI Responses, OpenAI Codex and Azure OpenAI. On the others the upstream alone decides how thinking comes back; there is no switch to send."
      );

  const reasoningContentLabel = (value: ReasoningContent) => {
    switch (value) {
      case "plaintext":
        return t("明文思考字段", "Plaintext reasoning field");
      case "encrypted":
        return t("加密思考字段", "Encrypted reasoning field");
    }
  };

  const visionLabel = capabilityLabel(t, "image_recognition");
  /* A model being added takes Mewrk's protocol declarations as its ID is
     typed, the way a fetched one is declared, until any of their switches is moved. */
  const [appendTouched, setAppendTouched] = useState(mode === "edit");
  const changeId = (id: string) => {
    if (appendTouched) {
      onChange({ ...draft, id });
      return;
    }
    const kept = draft.capabilities.filter((capability) => !PROTOCOL_CAPABILITIES.includes(capability));
    onChange({
      ...draft,
      id,
      capabilities: normalizeCapabilities([...kept, ...knownProtocolCapabilities({ family, baseUrl }, id.trim())])
    });
  };
  const toggle = (capability: ModelCapability, on: boolean) => {
    if (PROTOCOL_CAPABILITIES.includes(capability)) setAppendTouched(true);
    onChange({ ...draft, capabilities: withCapability(draft.capabilities, capability, on) });
  };
  /** Whether Mewrk knows the answer for this model here, and what it is; otherwise the user's to declare. */
  const knownHint = (known: boolean | null) => known === null
    ? t(
        "Mewrk 不认得这个模型或这个端点（比如中转站），请按它的实际能力勾选。",
        "Mewrk doesn't know this model or this endpoint (a relay, say); tick it according to what the endpoint actually takes."
      )
    : known
      ? t("Mewrk 认得这个模型：支持，获取模型时会自动勾上。", "Mewrk knows this model: it takes it, and fetching the model ticks this.")
      : t("Mewrk 认得这个模型：不支持。", "Mewrk knows this model: it does not take it.");
  const modelId = draft.id.trim();
  /* Sentences run on without a space in Chinese and with one in English. */
  const sentenceGap = t("", " ");
  const toolAppendLabel = capabilityLabel(t, "tool_append");
  const toolAppendHint = toolAppendTakesEffect(family)
    ? `${t(
        "对话中途加入的工具经协议自己的追加接口交给模型，不改写开头的工具列表，提示词缓存保得住。没有这项能力时，工具面在首次请求后固定，自动压缩与工具发现也不可用。",
        "Tools that join mid-conversation reach the model through the protocol's own append interface instead of rewriting the tool list at the head of the prompt, so the prompt cache survives. Without it the tool surface is fixed after the first request, and auto-compact and tool discovery are unavailable."
      )}${modelId ? `${sentenceGap}${knownHint(knownToolAppend(family, baseUrl, modelId))}` : ""}`
    : t(
        "这个协议没有对话中途追加工具的接口，这一项不起作用。",
        "This protocol has no interface for adding a tool mid-conversation, so this setting has no effect."
      );
  const systemAppendLabel = capabilityLabel(t, "system_append");
  const systemAppendHint = systemAppendTakesEffect(family)
    ? `${t(
        "计划模式这类中途开始生效的指令，以会话中的 system 消息追加在那个位置，不改开头的系统提示词。没有这项能力时改用 user 消息里的 <system-reminder> 送达。",
        "Instructions that start to apply mid-conversation, such as plan mode, are appended as a system message at that point instead of rewriting the system prompt. Without it they arrive as a <system-reminder> in a user message."
      )}${modelId ? `${sentenceGap}${knownHint(knownSystemAppend(family, baseUrl, modelId))}` : ""}`
    : t(
        "这个协议不能在对话中途带 system 消息，这一项不起作用。",
        "This protocol cannot carry a system message mid-conversation, so this setting has no effect."
      );
  const asyncToolsLabel = capabilityLabel(t, "async_tools");
  const asyncToolsHint = asyncToolsTakeEffect(family)
    ? `${t(
        "子代理和工作流以异步工具声明：调用不必等结果，模型可以接着干别的，任务的结果之后作为这次调用的输出送达。没有这项能力时，结果按 Claude Code 的做法以 user 消息里的 <task-notification> 送达。",
        "Subagents and workflows are declared as asynchronous tools: the call does not wait for its result, the model goes on with other work, and the task's result later arrives as that call's output. Without it, results arrive the way Claude Code delivers them: a <task-notification> in a user message."
      )}${modelId ? `${sentenceGap}${knownHint(knownAsyncTools(family, baseUrl, modelId))}` : ""}`
    : t(
        "这个协议没有异步工具调用，这一项不起作用。",
        "This protocol has no asynchronous tool calls, so this setting has no effect."
      );
  const nativeCompactionLabel = capabilityLabel(t, "native_compaction");
  const nativeCompactionHint = nativeCompactionTakesEffect(family)
    ? `${t(
        "上下文达到原生压缩阈值时，由模型把上下文压缩成一个不透明的压缩项，保留最近的用户消息，在当前对话里继续（Codex 的做法）。没有这项能力时，自动压缩只有交接一种。",
        "Past the native-compaction threshold, the model compacts the context into one opaque item, keeps the latest user messages, and carries on in the same conversation (the way Codex does). Without it, auto-compact only hands off."
      )}${modelId ? `${sentenceGap}${knownHint(knownNativeCompaction(family, baseUrl, modelId))}` : ""}`
    : t(
        "这个协议没有原生压缩，这一项不起作用。",
        "This protocol has no native compaction, so this setting has no effect."
      );
  const promptCacheLabel = t("提示词缓存", "Prompt caching");
  /** Only the Messages protocol takes client-placed cache breakpoints; elsewhere the value is stored but idle. */
  const promptCacheHint = promptCacheTakesEffect(family)
    ? t(
        "按 Claude Code 的规则在请求里打缓存断点：系统提示词的稳定前缀与本步尾巴各一处，最后一条可打标的消息一处。Claude 只缓存客户端标记过的前缀，关掉就完全没有缓存。",
        "Places cache breakpoints the way Claude Code does: one on the stable system-prompt prefix, one on its per-step tail, and one on the last markable message. Claude only caches what the client marks, so turning this off means no caching at all."
      )
    : t(
        "这一档今天只对 Anthropic Messages 协议有效；别的协议由上游自行决定缓存，客户端没有可发的标记。",
        "This setting only takes effect on the Anthropic Messages protocol today; on the others the upstream decides caching on its own and there is no marker to send."
      );

  return (
    <Drawer
      title={mode === "create" ? t("添加模型", "Add model") : t("模型属性", "Model properties")}
      subtitle={t("{provider} · 修改只会在保存后应用", "{provider} · Changes apply only after saving", { provider: providerName })}
      labelledBy="model-editor-title"
      onClose={onClose}
      footer={
        <>
          <button type="button" className="button button--ghost" onClick={onClose}>{t("取消", "Cancel")}</button>
          <button type="button" className="button button--primary" disabled={Boolean(idError)} onClick={onSave}>{t("保存", "Save")}</button>
        </>
      }
    >
      <Field label={t("模型 ID", "Model ID")} hint={idError ?? t("请求 API 时发送的 model 值", "The model value sent in API requests")}>
        <input
          className={`input input--code${idError ? " input--error" : ""}`}
          aria-label={t("模型 ID", "Model ID")}
          aria-invalid={Boolean(idError)}
          value={draft.id}
          onChange={(event) => changeId(event.target.value)}
          placeholder={t("例如 gpt-5", "For example, gpt-5")}
        />
      </Field>
      <div className="drawer-field-grid">
        <Field label={t("显示名", "Display name")} hint={t("留空则显示模型 ID", "Falls back to the model ID when blank")}>
          <input className="input" aria-label={t("显示名", "Display name")} value={draft.name} onChange={(event) => onChange({ ...draft, name: event.target.value })} />
        </Field>
        <Field label={t("分组", "Group")} hint={t("模型列表里的折叠分组；留空则由 ID 推断", "The collapsible group in the model list; inferred from the ID when blank")}>
          <input className="input" aria-label={t("分组", "Group")} value={draft.group} onChange={(event) => onChange({ ...draft, group: event.target.value })} />
        </Field>
        <Field label={t("上下文窗口", "Context window")}>
          <input className="input" aria-label={t("上下文窗口", "Context window")} type="number" min={1} step={1} value={draft.contextWindow ?? ""} onChange={(event) => onChange({ ...draft, contextWindow: optionalPositiveInteger(event.target.value) })} placeholder={t("留空表示未知", "Leave blank if unknown")} />
        </Field>
        <Field label={t("单次最大输出", "Maximum output")}>
          <input className="input" aria-label={t("最大输出 Token", "Maximum output tokens")} type="number" min={1} step={1} value={draft.maxOutputTokens ?? ""} onChange={(event) => onChange({ ...draft, maxOutputTokens: optionalPositiveInteger(event.target.value) })} placeholder={t("留空表示未知", "Leave blank if unknown")} />
        </Field>
        <Field
          label={t("缓存失效时间（分钟）", "Cache lifetime (minutes)")}
          hint={t(
            "上次请求后这么久内，对话设置里会让这个模型的提示词缓存失效的改动显示为橘色并先提醒。",
            "For this long after a request, changes in the conversation settings that would invalidate this model's prompt cache are shown in orange and ask first."
          )}
        >
          <input
            className="input"
            aria-label={t("缓存失效时间（分钟）", "Cache lifetime (minutes)")}
            type="number"
            min={1}
            step={1}
            value={draft.cacheTtlMinutes ?? ""}
            onChange={(event) => onChange({ ...draft, cacheTtlMinutes: optionalPositiveInteger(event.target.value) })}
            placeholder={t("默认 {minutes}", "Default {minutes}", { minutes: DEFAULT_CACHE_TTL_MINUTES })}
          />
        </Field>
      </div>

      <div className="drawer-switch-row">
        <span>
          <strong>{capabilityIcon("image_recognition", 12)} {visionLabel}</strong>
          <small>{t("允许把图片作为输入发给这个模型。", "Lets images be sent to this model as input.")}</small>
        </span>
        <Switch
          checked={draft.capabilities.includes("image_recognition")}
          onChange={(on) => toggle("image_recognition", on)}
          label={t("{name} {capability}", "{name} {capability}", {
            name: draft.id || t("新模型", "New model"),
            capability: visionLabel
          })}
        />
      </div>

      <div className="drawer-switch-row">
        <span>
          <strong>{capabilityIcon("tool_append", 12)} {toolAppendLabel}</strong>
          <small>{toolAppendHint}</small>
        </span>
        <Switch
          checked={draft.capabilities.includes("tool_append")}
          onChange={(on) => toggle("tool_append", on)}
          label={t("{name} {capability}", "{name} {capability}", {
            name: draft.id || t("新模型", "New model"),
            capability: toolAppendLabel
          })}
        />
      </div>

      <div className="drawer-switch-row">
        <span>
          <strong>{capabilityIcon("system_append", 12)} {systemAppendLabel}</strong>
          <small>{systemAppendHint}</small>
        </span>
        <Switch
          checked={draft.capabilities.includes("system_append")}
          onChange={(on) => toggle("system_append", on)}
          label={t("{name} {capability}", "{name} {capability}", {
            name: draft.id || t("新模型", "New model"),
            capability: systemAppendLabel
          })}
        />
      </div>

      <div className="drawer-switch-row">
        <span>
          <strong>{capabilityIcon("async_tools", 12)} {asyncToolsLabel}</strong>
          <small>{asyncToolsHint}</small>
        </span>
        <Switch
          checked={draft.capabilities.includes("async_tools")}
          onChange={(on) => toggle("async_tools", on)}
          label={t("{name} {capability}", "{name} {capability}", {
            name: draft.id || t("新模型", "New model"),
            capability: asyncToolsLabel
          })}
        />
      </div>

      <div className="drawer-switch-row">
        <span>
          <strong>{capabilityIcon("native_compaction", 12)} {nativeCompactionLabel}</strong>
          <small>{nativeCompactionHint}</small>
        </span>
        <Switch
          checked={draft.capabilities.includes("native_compaction")}
          onChange={(on) => toggle("native_compaction", on)}
          label={t("{name} {capability}", "{name} {capability}", {
            name: draft.id || t("新模型", "New model"),
            capability: nativeCompactionLabel
          })}
        />
      </div>

      <div className="drawer-switch-row">
        <span>
          <strong>{promptCacheLabel}</strong>
          <small>{promptCacheHint}</small>
        </span>
        <Switch
          checked={draft.promptCache}
          onChange={(on) => onChange({ ...draft, promptCache: on })}
          label={t("{name} {capability}", "{name} {capability}", {
            name: draft.id || t("新模型", "New model"),
            capability: promptCacheLabel
          })}
        />
      </div>

      <Field label={t("思考字段形态", "Reasoning field form")} hint={reasoningHint}>
        <select
          className="input"
          aria-label={t("思考字段形态", "Reasoning field form")}
          value={draft.reasoningContent}
          onChange={(event) => onChange({ ...draft, reasoningContent: event.target.value as ReasoningContent })}
        >
          {REASONING_CONTENTS.map((value) => (
            <option key={value} value={value}>{reasoningContentLabel(value)}</option>
          ))}
        </select>
      </Field>
    </Drawer>
  );
}
