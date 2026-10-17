import { Plus } from "lucide-react";
import { useMemo, useState } from "react";
import { useI18n } from "../i18n";
import { hasBackendRuntime } from "../lib/backend";
import { SEARCH_PROVIDERS, searchProviderEntry } from "../lib/searchProviders";
import {
  deleteSearchApiKey,
  getSearchKeyStatus,
  revealSearchApiKey,
  saveSearchApiKey,
  type SearchCredentialSlot
} from "../lib/webSearch";
import type {
  SearchProviderConfig,
  SearchProviderKind,
  WebSearchAssets
} from "../types";
import { Switch } from "./Common";
import { DocsLink } from "./DocsLink";
import { SecretField } from "./SecretField";
import { SettingsRail, SettingsRailRow } from "./SettingsRail";

type WebSearchAssetsChange = WebSearchAssets | ((current: WebSearchAssets) => WebSearchAssets);

interface WebSearchSettingsProps {
  settings: WebSearchAssets;
  onChange: (change: WebSearchAssetsChange) => void;
  onFlush?: () => Promise<void>;
}

/**
 * An input parsed while typing needs its own draft.
 *
 * Controlled values must not render `parse(value)`: parsing a trailing comma or
 * an empty numeric field would destroy the user's in-progress input. Clear the
 * draft only on blur, when the normalized value may be shown again.
 */
function useEditDraft(value: string) {
  const [draft, setDraft] = useState<string | null>(null);
  return {
    value: draft ?? value,
    onChange: setDraft,
    onBlur: () => setDraft(null)
  };
}

function providerConfig(settings: WebSearchAssets, kind: SearchProviderKind): SearchProviderConfig {
  return settings.providers.find((provider) => provider.kind === kind) ?? {
    kind,
    enabled: false,
    searchApiHost: "",
    fetchApiHost: "",
    engines: [],
    basicAuthUsername: ""
  };
}

function useSearchCredentialPort(providerKind: SearchProviderKind, slot: SearchCredentialSlot) {
  return useMemo(() => ({
    status: () => getSearchKeyStatus(providerKind, slot),
    save: (secret: string) => saveSearchApiKey(providerKind, slot, secret),
    reveal: () => revealSearchApiKey(providerKind, slot),
    remove: () => deleteSearchApiKey(providerKind, slot)
  }), [providerKind, slot]);
}

/**
 * Search provider settings.
 *
 * The shared provider layout has a catalog rail and a detail pane, and nothing
 * else: every setting here belongs to one provider row. How a search behaves —
 * result count, compression, domain rules — is asked of the conversation that
 * runs it, so there is no global page for it to sit on.
 */
export function WebSearchSettings({ settings, onChange, onFlush }: WebSearchSettingsProps) {
  const { t } = useI18n();
  const [selectedRow, setSelectedRow] = useState<SearchProviderKind>(SEARCH_PROVIDERS[0].kind);
  const desktopRuntime = hasBackendRuntime();

  const updateProvider = (kind: SearchProviderKind, patch: Partial<SearchProviderConfig>) => {
    onChange((current) => ({
      ...current,
      providers: SEARCH_PROVIDERS.map((catalogProvider) => {
        const provider = providerConfig(current, catalogProvider.kind);
        return catalogProvider.kind === kind ? { ...provider, ...patch } : provider;
      })
    }));
  };

  return (
    <div className="settings-editor-page settings-rail-page search-provider-page">
      <SettingsRail
        footer={(
          <button
            type="button"
            className="provider-rail__add"
            disabled
            title={t(
              "搜索提供商目录随应用发布，暂不支持自定义条目。",
              "The search-provider catalog ships with the app; custom entries are not supported yet."
            )}
          ><Plus size={13} /> {t("添加提供商", "Add provider")}</button>
        )}
      >
        {SEARCH_PROVIDERS.map((provider) => (
          <SettingsRailRow
            key={provider.kind}
            label={provider.label}
            selected={provider.kind === selectedRow}
            active={providerConfig(settings, provider.kind).enabled}
            onSelect={() => setSelectedRow(provider.kind)}
          />
        ))}
      </SettingsRail>

      <ProviderPane
        entry={searchProviderEntry(selectedRow)}
        config={providerConfig(settings, selectedRow)}
        desktopRuntime={desktopRuntime}
        onChange={updateProvider}
        onFlush={onFlush}
      />
    </div>
  );
}

function ProviderPane({
  entry,
  config,
  desktopRuntime,
  onChange,
  onFlush
}: {
  entry: ReturnType<typeof searchProviderEntry>;
  config: SearchProviderConfig;
  desktopRuntime: boolean;
  onChange: (kind: SearchProviderKind, patch: Partial<SearchProviderConfig>) => void;
  onFlush?: () => Promise<void>;
}) {
  const { t } = useI18n();
  const enginesDraft = useEditDraft(config.engines.join(", "));
  const isSearxng = entry.kind === "searxng";
  const isLocalFetch = entry.kind === "fetch";
  /* Each capability says for itself whether it needs a key: Jina searches only
     with one but reads pages without, so the field is marked required only when
     nothing this provider does works anonymously. */
  const capabilities = [entry.search, entry.fetch].filter((spec) => spec !== null);
  const keyRequired = capabilities.every((spec) => spec.requiresApiKey);
  const searchNeedsKeyOnly = !keyRequired && entry.search?.requiresApiKey === true;
  /* A required key is already said by the field's label. */
  const keyNote = keyRequired
    ? null
    : searchNeedsKeyOnly
      ? t("仅搜索需要 API Key。", "Only search needs an API key.")
      : t("API Key 可选，用于提高配额。", "The API key is optional; it raises the quota.");
  const secretHelp = desktopRuntime
    ? t("失去焦点时自动保存到系统凭据库。", "Saved to the system credential store when you leave the field.")
    : t("浏览器预览不发送请求，也不保存凭据。", "Browser preview sends no requests and stores no credentials.");
  /* The rule `providers.rs::parse_api_host` enforces: http is fine for any
     address on this machine or the local network, not only loopback. The short
     line names https; the exception for local addresses is on the site. */
  const endpointRule = <>
    {t("留空使用默认端点，须为 https。", "Leave empty for the default; must be https.")}
    <DocsLink inline page="web" section="provider-settings" />
  </>;
  const apiCredentials = useSearchCredentialPort(entry.kind, "apiKey");
  const basicAuthCredentials = useSearchCredentialPort(entry.kind, "basicAuthPassword");

  return (
    <div className="provider-pane">
      <header className="provider-pane__header">
        <div className="provider-pane__identity">
          <h1>{entry.label}</h1>
        </div>
        {/* The list-row dot is state; this switch is the only control. */}
        <Switch
          checked={config.enabled}
          onChange={(enabled) => onChange(entry.kind, { enabled })}
          label={t("启用搜索提供商 {name}", "Enable search provider {name}", { name: entry.label })}
        />
      </header>

      <div className="provider-pane__body">
        <div className="provider-pane__stack">
          {isLocalFetch && (
            <section className="provider-field">
              <div className="provider-field__title"><span>{t("本机抓取", "Local fetch")}</span></div>
              <p className="provider-field__help">
                {t(
                  "由应用自己读取网页，无需配置。",
                  "The app reads pages itself; nothing to configure."
                )}
                <DocsLink inline page="web" section="local-fetch" />
              </p>
            </section>
          )}

          {!isLocalFetch && !isSearxng && (
            <SecretField
              identity={`${entry.kind}:apiKey`}
              credentials={apiCredentials}
              label="API Key"
              required={keyRequired}
              help={keyNote ? t("{note}{storage}", "{note} {storage}", { note: keyNote, storage: secretHelp }) : secretHelp}
              onFlush={onFlush}
            />
          )}

          {isSearxng && (
            <>
              <section className="provider-field">
                <div className="provider-field__title"><span>{t("搜索引擎", "Search engines")}</span></div>
                <div className="provider-field__row">
                  <div className="provider-input-group">
                    <input
                      className="provider-input provider-input--code"
                      aria-label={t("搜索引擎", "Search engines")}
                      spellCheck={false}
                      value={enginesDraft.value}
                      onChange={(event) => {
                        enginesDraft.onChange(event.target.value);
                        onChange(entry.kind, {
                          engines: event.target.value
                            .split(",")
                            .map((engine) => engine.trim())
                            .filter(Boolean)
                        });
                      }}
                      onBlur={enginesDraft.onBlur}
                      placeholder="google, duckduckgo, brave"
                    />
                  </div>
                </div>
                <p className="provider-field__help">{t(
                  "逗号分隔；留空则用实例已启用的引擎。",
                  "Comma separated; empty uses the instance's enabled engines."
                )}</p>
              </section>

              <section className="provider-field">
                <div className="provider-field__title"><span>{t("Basic Auth 用户名", "Basic auth username")}</span></div>
                <div className="provider-field__row">
                  <div className="provider-input-group">
                    <input
                      className="provider-input provider-input--code"
                      aria-label={t("Basic Auth 用户名", "Basic auth username")}
                      spellCheck={false}
                      value={config.basicAuthUsername}
                      onChange={(event) => onChange(entry.kind, { basicAuthUsername: event.target.value })}
                    />
                  </div>
                </div>
                <p className="provider-field__help">{t(
                  "实例有 Basic Auth 时填写。",
                  "For instances behind basic auth."
                )}</p>
              </section>

              <SecretField
                identity={`${entry.kind}:basicAuthPassword`}
                credentials={basicAuthCredentials}
                label={t("Basic Auth 密码", "Basic auth password")}
                required={false}
                help={secretHelp}
                onFlush={onFlush}
              />
            </>
          )}

          {entry.search && entry.search.defaultApiHost && (
            <section className="provider-field">
              <div className="provider-field__title">
                <span>{t("搜索端点", "Search endpoint")}</span>
              </div>
              <div className="provider-field__row">
                <div className="provider-input-group">
                  <input
                    className="provider-input provider-input--code"
                    aria-label={t("搜索端点", "Search endpoint")}
                    type="url"
                    spellCheck={false}
                    value={config.searchApiHost}
                    onChange={(event) => onChange(entry.kind, { searchApiHost: event.target.value })}
                    placeholder={entry.search.defaultApiHost}
                  />
                </div>
              </div>
              <p className="provider-field__help">{endpointRule}</p>
            </section>
          )}

          {entry.fetch && entry.fetch.defaultApiHost && (
            <section className="provider-field">
              <div className="provider-field__title">
                <span>{t("抓取端点", "Fetch endpoint")}</span>
              </div>
              <div className="provider-field__row">
                <div className="provider-input-group">
                  <input
                    className="provider-input provider-input--code"
                    aria-label={t("抓取端点", "Fetch endpoint")}
                    type="url"
                    spellCheck={false}
                    value={config.fetchApiHost}
                    onChange={(event) => onChange(entry.kind, { fetchApiHost: event.target.value })}
                    placeholder={entry.fetch.defaultApiHost}
                  />
                </div>
              </div>
              <p className="provider-field__help">{endpointRule}</p>
            </section>
          )}
        </div>
      </div>
    </div>
  );
}
