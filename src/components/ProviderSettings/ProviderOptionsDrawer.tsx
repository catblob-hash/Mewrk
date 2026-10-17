import { useI18n } from "../../i18n";
import { knownFamilySettings, requiredFamilySettings } from "../../lib/modelCapabilities";
import { CODEX_DEFAULT_BASE_URL, isBuiltinProvider, isCodexProvider } from "../../lib/codexProvider";
import type { ProviderFamily, ApiProvider } from "../../types";
import { Field } from "../Common";
import { DocsLink } from "../DocsLink";
import { Drawer } from "./Drawer";
import { API_FORMAT_OPTIONS, builtinFamilyOption, familyLabel, familySettingMeta } from "./endpointMeta";

/**
 * Provider settings drawer for the name, API format, and the identity fields of
 * the families that have them.
 *
 * These infrequently changed settings stay out of the main panel so its API key and
 * API address controls remain prominent. The header gear and the gear beside the API
 * address open this same drawer. A provider has one address, the chat API address on
 * the main panel; the only address here is Codex's, whose main panel has none.
 */
export function ProviderOptionsDrawer({
  provider,
  onChange,
  onClose
}: {
  provider: ApiProvider;
  onChange: (update: (provider: ApiProvider) => ApiProvider) => void;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const settings = knownFamilySettings(provider.family);
  const required = requiredFamilySettings(provider.family);

  return (
    <Drawer
      title={t("提供商设置", "Provider settings")}
      subtitle={isBuiltinProvider(provider)
        ? t("内置提供商", "Built-in provider")
        : t("自定义提供商", "Custom provider")}
      labelledBy="provider-options-title"
      onClose={onClose}
    >
      <Field label={t("提供商名称", "Provider name")}>
        <input
          className="input"
          aria-label={t("提供商名称", "Provider name")}
          value={provider.name}
          onChange={(event) => {
            const name = event.target.value;
            onChange((current) => ({ ...current, name }));
          }}
        />
      </Field>

      <Field
        label={t("API 格式", "API format")}
        hint={<>
          {t("决定对话请求走哪个端点。", "Picks the endpoint chat requests use.")}
          <DocsLink inline page="providers" section="protocols" />
        </>}
      >
        <select
          className="input"
          aria-label={t("API 格式", "API format")}
          value={provider.family}
          disabled={isBuiltinProvider(provider)}
          onChange={(event) => {
            const family = event.target.value as ProviderFamily;
            const oldDefault = API_FORMAT_OPTIONS.find((option) => option.value === provider.family)?.defaultBaseUrl;
            const nextDefault = API_FORMAT_OPTIONS.find((option) => option.value === family)?.defaultBaseUrl ?? "";
            // Preserve an entered URL when changing protocol; replace only an empty URL or the old default.
            const baseUrl = !provider.baseUrl.trim() || provider.baseUrl === oldDefault ? nextDefault : provider.baseUrl;
            // Remove identity fields irrelevant to the new provider family so they cannot
            // be sent with its requests.
            const keep = new Set<string>(knownFamilySettings(family));
            onChange((current) => {
              const familySettings = Object.fromEntries(
                Object.entries(current.familySettings).filter(([key]) => keep.has(key))
              ) as typeof current.familySettings;
              return { ...current, family, baseUrl, familySettings };
            });
          }}
        >
          {builtinFamilyOption(provider.family) ? (
            <option value={provider.family}>{familyLabel(provider.family)}</option>
          ) : API_FORMAT_OPTIONS.map((option) => (
            <option key={option.value} value={option.value}>{option.label}</option>
          ))}
        </select>
      </Field>

      {settings.length > 0 && (
        <div className="drawer-section">
          <div className="drawer-section__title">
            <strong>{t("身份字段", "Identity fields")}</strong>
            <small>{t(
              "地址由这些字段推导，缺项会拒绝请求。",
              "The address comes from these; a missing one blocks requests."
            )}</small>
          </div>
          {settings.map((setting) => {
            const meta = familySettingMeta(t, setting);
            const isRequired = required.includes(setting);
            return (
              <Field
                key={setting}
                label={isRequired ? `${meta.label} *` : meta.label}
                hint={meta.hint}
              >
                <input
                  className="input input--code"
                  aria-label={meta.label}
                  spellCheck={false}
                  value={provider.familySettings[setting] ?? ""}
                  placeholder={meta.placeholder}
                  onChange={(event) => {
                    const value = event.target.value;
                    onChange((current) => {
                      const familySettings = { ...current.familySettings };
                      if (value.trim()) familySettings[setting] = value;
                      else delete familySettings[setting];
                      return { ...current, familySettings };
                    });
                  }}
                />
              </Field>
            );
          })}
        </div>
      )}

      {/* Codex's chat address belongs to the ChatGPT backend, so its main panel
          shows none; this field exists only to point it at a local test stub. */}
      {isCodexProvider(provider) && (
      <div className="drawer-section">
        <Field
          label={t("本机测试桩地址", "Local test stub address")}
          hint={t(
            "留空使用 ChatGPT 后端，仅限本机地址。",
            "Blank uses the ChatGPT backend; local addresses only."
          )}
        >
          <input
            className="input input--code"
            aria-label={t("本机测试桩地址", "Local test stub address")}
            type="url"
            spellCheck={false}
            value={provider.baseUrl}
            placeholder={CODEX_DEFAULT_BASE_URL}
            onChange={(event) => {
              const baseUrl = event.target.value;
              onChange((current) => ({ ...current, baseUrl }));
            }}
          />
        </Field>
      </div>
      )}
    </Drawer>
  );
}
