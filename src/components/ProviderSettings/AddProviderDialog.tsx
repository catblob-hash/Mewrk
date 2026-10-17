import { useState } from "react";
import { useI18n } from "../../i18n";
import type { ProviderFamily } from "../../types";
import { Field } from "../Common";
import { DocsLink } from "../DocsLink";
import { DialogShell } from "./DialogShell";
import { API_FORMAT_OPTIONS } from "./endpointMeta";

/**
 * Adds providers through a settings form; API address and key are configured
 * in the right pane after creation.
 */
export function AddProviderDialog({
  onClose,
  onCreate
}: {
  onClose: () => void;
  onCreate: (draft: { name: string; family: ProviderFamily }) => void;
}) {
  const { t } = useI18n();
  const [name, setName] = useState("");
  // OpenAI Chat Completions is the default for relays and most upstream providers.
  const [family, setProviderFamily] = useState<ProviderFamily>("openai_chat");
  const trimmedName = name.trim();

  const submit = () => {
    if (!trimmedName) return;
    onCreate({ name: trimmedName, family });
  };

  return (
    <DialogShell
      title={t("添加提供商", "Add provider")}
      subtitle={t(
        "填名称、选协议，地址与 Key 稍后填。",
        "Name it and pick a protocol; address and key come next."
      )}
      labelledBy="add-provider-title"
      onClose={onClose}
      footer={
        <>
          <button type="button" className="button button--ghost" onClick={onClose}>
            {t("取消", "Cancel")}
          </button>
          <button type="button" className="button button--primary" disabled={!trimmedName} onClick={submit}>
            {t("添加", "Add")}
          </button>
        </>
      }
    >
      <Field label={t("提供商名称", "Provider name")}>
        <input
          className="input"
          aria-label={t("提供商名称", "Provider name")}
          value={name}
          maxLength={32}
          placeholder={t("例如 我的中转站", "Example: My relay")}
          onChange={(event) => setName(event.target.value)}
          onKeyDown={(event) => {
            // DialogShell handles Escape; Enter submits this form.
            if (event.key !== "Enter") return;
            event.preventDefault();
            submit();
          }}
        />
      </Field>

      <Field
        label={t("对话协议", "Chat protocol")}
        hint={<>
          {t("决定对话请求走哪个端点，建好后仍可改。", "Picks the chat endpoint; you can change it later.")}
          <DocsLink inline page="providers" section="protocols" />
        </>}
      >
        <select
          className="input"
          aria-label={t("对话协议", "Chat protocol")}
          value={family}
          onChange={(event) => setProviderFamily(event.target.value as ProviderFamily)}
        >
          {API_FORMAT_OPTIONS.map((option) => (
            <option key={option.value} value={option.value}>{option.label}</option>
          ))}
        </select>
      </Field>
    </DialogShell>
  );
}
