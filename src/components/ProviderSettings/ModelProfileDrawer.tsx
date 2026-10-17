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
import { DocsLink } from "../DocsLink";
import { capabilityIcon, capabilityLabel } from "./capabilityMeta";
import { Drawer } from "./Drawer";

/** Where the site explains what each of the switches below does, and the protocols it applies to. */
const modelPropertiesLink = <DocsLink inline page="providers" section="model-properties" />;

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
  const reasoningHint = (
    <>
      {reasoningContentTakesEffect(family)
        ? t(
            "端点流式返回可读的思考文字时，选「明文」。",
            "Choose Plaintext if the endpoint streams readable reasoning."
          )
        : t("这个协议下此项无效。", "No effect on this protocol.")}
      {modelPropertiesLink}
    </>
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
  /* One short line for what the switch is for, then the recognition note for the
     typed model ID, then the link to the model-properties section. */
  const capabilityHint = (summary: string, known?: boolean | null) => (
    <>
      {`${summary}${known !== undefined ? `${sentenceGap}${knownHint(known)}` : ""}`}
      {modelPropertiesLink}
    </>
  );
  const toolAppendLabel = capabilityLabel(t, "tool_append");
  const toolAppendHint = toolAppendTakesEffect(family)
    ? capabilityHint(
        t(
          "让工具能在对话中途加入，且不丢提示词缓存。",
          "Lets tools join mid-conversation, keeping the prompt cache."
        ),
        modelId ? knownToolAppend(family, baseUrl, modelId) : undefined
      )
    : t(
        "此项无效：这个协议不能中途追加工具。",
        "No effect: this protocol can't add tools mid-conversation."
      );
  const systemAppendLabel = capabilityLabel(t, "system_append");
  const systemAppendHint = systemAppendTakesEffect(family)
    ? capabilityHint(
        t(
          "让中途生效的指令以 system 消息送达。",
          "Sends mid-conversation instructions as system messages."
        ),
        modelId ? knownSystemAppend(family, baseUrl, modelId) : undefined
      )
    : t(
        "此项无效：这个协议不能中途带 system 消息。",
        "No effect: this protocol can't take system messages mid-conversation."
      );
  const asyncToolsLabel = capabilityLabel(t, "async_tools");
  const asyncToolsHint = asyncToolsTakeEffect(family)
    ? capabilityHint(
        t(
          "子代理和工作流运行时，模型无需等待。",
          "The model needn't wait while subagents and workflows run."
        ),
        modelId ? knownAsyncTools(family, baseUrl, modelId) : undefined
      )
    : t(
        "此项无效：这个协议没有异步工具调用。",
        "No effect: this protocol has no asynchronous tool calls."
      );
  const nativeCompactionLabel = capabilityLabel(t, "native_compaction");
  const nativeCompactionHint = nativeCompactionTakesEffect(family)
    ? capabilityHint(
        t(
          "超过阈值时，由模型自己压缩上下文。",
          "Past the threshold, the model compacts the context itself."
        ),
        modelId ? knownNativeCompaction(family, baseUrl, modelId) : undefined
      )
    : t(
        "此项无效：这个协议没有原生压缩。",
        "No effect: this protocol has no native compaction."
      );
  const promptCacheLabel = t("提示词缓存", "Prompt caching");
  /** Only the Messages protocol takes client-placed cache breakpoints; elsewhere the value is stored but idle. */
  const promptCacheHint = promptCacheTakesEffect(family)
    ? capabilityHint(
        t(
          "为请求打缓存断点；关闭后完全不缓存。",
          "Marks cache breakpoints; turning it off means no caching."
        )
      )
    : t(
        "此项只对 Anthropic Messages 有效。",
        "Only takes effect on Anthropic Messages."
      );

  return (
    <Drawer
      title={mode === "create" ? t("添加模型", "Add model") : t("模型属性", "Model properties")}
      subtitle={t("{provider} · 保存后生效", "{provider} · Applies on save", { provider: providerName })}
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
        <Field label={t("分组", "Group")} hint={t("模型列表里的分组；留空则由 ID 推断", "Group in the model list; inferred from the ID if blank")}>
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
          hint={<>
            {t(
              "这段时间内，破坏缓存的改动会先提醒。",
              "Within this time, cache-breaking changes ask first."
            )}
            <DocsLink inline page="working" section="tool-lock" />
          </>}
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
