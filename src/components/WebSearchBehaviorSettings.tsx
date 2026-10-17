import { useRef, useState } from "react";
import { useI18n } from "../i18n";
import { SEARCH_PROVIDERS } from "../lib/searchProviders";
import type { LockTone } from "../lib/toolLock";
import {
  switchWebLeg,
  webLegOn,
  type RememberedWebLegs,
  type WebLeg
} from "../lib/webSearch";
import type {
  ConversationWebSearchSettings,
  NativeSearchTool,
  WebSearchAssets
} from "../types";
import { NATIVE_SEARCH_TOOLS } from "../types";
import { DocsLink } from "./DocsLink";
import { FetchProviderField } from "./FetchProviderField";
import { LockableSwitchRow, type BackendLock, type LockHints } from "./LockTone";
import { SearchDomainFilterRow } from "./SearchDomainFilterRow";
import { SearchProviderField, type WebSearchSubject } from "./SearchProviderField";
import { FetchLegParams, SearchLegParams } from "./SearchResultShapingFields";

interface WebSearchBehaviorCommonProps {
  webSearchAssets: WebSearchAssets;
  /**
   * Whose settings these are, which decides whose model the standing advice on
   * the native backends speaks of: the conversation's own, or — for a role — the
   * model the role runs on. Defaults to the conversation.
   */
  subject?: WebSearchSubject;
  /**
   * Whether this conversation's own model exposes page retrieval as a server
   * tool of its own. Only there is native a fetch backend: everywhere else the
   * fetch menu has no native row, and a native selection carried over from
   * such a model draws the fetch switch off until the model has one again.
   */
  nativeFetchAvailable?: boolean;
  /**
   * Whether this conversation's model spells its native web tools the Messages
   * way, with the version written into the tool's own `type`. Only then is
   * there a version for the user to pick, so only then does the native row open
   * into a second step. On any other model the row selects native directly and
   * the versions this conversation is carrying stay untouched, ready for the
   * next Messages model it runs on.
   */
  nativeToolTypeSelectable?: boolean;
  /**
   * What the conversation says about the search selector: settled once a
   * native search has sealed results into the transcript (`backendPinned`);
   * orange while the cache is warm (`backendTone`).
   */
  searchLock?: BackendLock | null;
  /** The same for the fetch selector. */
  fetchLock?: BackendLock | null;
  /** How each leg's switch is drawn (`webLegTone`). */
  switchTones?: Partial<Record<WebLeg, LockTone | null>>;
  /** What a toned switch says about itself. */
  lockHints?: LockHints;
  /**
   * Whose switches these are — a conversation's id, a role's — so a leg turned
   * off keeps the backend it had across leaving the page and coming back.
   * Without one it is kept only while the page is open.
   */
  rememberKey?: string;
}

/** Backends set aside by a leg switch, per `rememberKey`, for this session. */
const rememberedWebLegs = new Map<string, RememberedWebLegs>();

/**
 * A conversation's, a preset's or a subagent role's web configuration. A role
 * answers every one of these for itself — the same shape, the same rows — so
 * there is no "follow the conversation" answer on any of them.
 */
export type WebSearchBehaviorSettingsProps = WebSearchBehaviorCommonProps & {
  value: ConversationWebSearchSettings;
  onChange: (patch: Partial<ConversationWebSearchSettings>) => void;
  /**
   * The surface's web access switch, which the two leg switches stand in for:
   * it is on while either leg is, and each flip writes it together with the
   * backends in one change. Omitted where that decision is someone else's — a
   * role's caller decides it, and stays the ceiling on whatever the role's own
   * legs say — and the legs then answer only for themselves.
   */
  webAccess?: {
    enabled: boolean;
    onChange: (enabled: boolean, patch: Partial<ConversationWebSearchSettings>) => void;
  };
};

/**
 * Search and fetch backend selection for conversations, presets and subagent
 * roles, with the result shaping and domain filtering that go with them.
 *
 * Each leg's numbers are drawn directly under that leg's selector, and only the
 * ones the selected backend actually has: they are that backend's own request
 * parameters, not generic knobs, so a backend without the parameter draws no row
 * for it (`SearchLegParams`, `FetchLegParams`).
 *
 * The two selectors are independent because upstreams disagree about how many
 * web tools there are. Anthropic splits retrieval into a second server tool;
 * DeepSeek and OpenAI keep it inside their one search tool. So "native" is a
 * legal answer on both sides, and on a family of the second kind choosing it
 * for both simply means the model is handed a single `web_search` — which is
 * that family's own shape, not a missing feature.
 *
 * Each leg has a switch of its own, and its selector is drawn under it only
 * while it is on, so neither menu has an "off" row: a conversation may search
 * without fetching, fetch without searching, or neither. Both menus list only
 * backends that are switched on in settings, and the fetch menu offers native
 * only on a model that has a fetch tool of its own.
 *
 * A role asks the same questions through this same component, and answers them
 * for itself: one recipe for every surface is what keeps them from drifting.
 */
export function WebSearchBehaviorSettings(props: WebSearchBehaviorSettingsProps) {
  const {
    value,
    onChange,
    webAccess,
    webSearchAssets,
    nativeFetchAvailable = false,
    nativeToolTypeSelectable = false,
    searchLock,
    fetchLock,
    switchTones,
    lockHints,
    rememberKey,
    subject = "conversation"
  } = props;
  const { t } = useI18n();
  const [domainWindowOpen, setDomainWindowOpen] = useState(false);
  /* The backend a leg had when it was switched off, so switching it back on
     brings that one back rather than a default. Held for the session per
     surface, not saved: the leg's own selection reads `disabled` while it is
     off. */
  const ownRemembered = useRef<RememberedWebLegs>({});
  const readRemembered = () => (
    rememberKey === undefined ? ownRemembered.current : rememberedWebLegs.get(rememberKey) ?? {}
  );
  const writeRemembered = (next: RememberedWebLegs) => {
    if (rememberKey === undefined) ownRemembered.current = next;
    else rememberedWebLegs.set(rememberKey, next);
  };
  /* The fetch version is a Messages spelling, so it is offered only where it is
     sent — and only where the family actually grants a separate fetch tool for
     it to be written onto. */
  const fetchVersionSelectable = nativeToolTypeSelectable && nativeFetchAvailable;
  const access = webAccess ? webAccess.enabled : null;
  const searchOn = webLegOn("search", access, value, nativeFetchAvailable);
  const fetchOn = webLegOn("fetch", access, value, nativeFetchAvailable);
  const flip = (leg: WebLeg, on: boolean) => {
    const next = switchWebLeg(leg, on, {
      webAccess: access,
      settings: value,
      nativeFetchOffered: nativeFetchAvailable,
      fetchProviders: SEARCH_PROVIDERS
        .filter((provider) => provider.fetch
          && webSearchAssets.providers.some((item) => item.kind === provider.kind && item.enabled))
        .map((provider) => provider.kind),
      remembered: readRemembered()
    });
    writeRemembered(next.remembered);
    if (webAccess && next.webAccess !== null) webAccess.onChange(next.webAccess, next.patch);
    else onChange(next.patch);
  };
  /* A native backend a run has used is settled: its switch is held with the
     selector under it, since turning the leg off would move the backend too. */
  const settledNote = (lock: BackendLock | null | undefined) => (
    lock?.kind === "settled" ? lock.note : undefined
  );
  const role = subject === "role";

  return <>
    <LockableSwitchRow
      title={t("网页搜索", "Web search")}
      description={role
        ? <>
          {t("让这个角色搜索网页。", "Lets this role search the web.")}
          <DocsLink inline page="web" section="enable" />
        </>
        : t("让模型用关键词搜索网页。", "Lets the model search the web by keyword.")}
      checked={searchOn}
      tone={switchTones?.search}
      hints={lockHints}
      disabled={Boolean(settledNote(searchLock))}
      disabledNote={settledNote(searchLock)}
      onChange={(on) => flip("search", on)}
      label={searchOn
        ? t("网页搜索已开启", "Web search enabled")
        : t("网页搜索已关闭", "Web search disabled")}
    />
    {searchOn && (
      <div className="web-search-provider-field">
        <SearchProviderField
          value={value.provider}
          onChange={(provider) => onChange({ provider })}
          webSearchAssets={webSearchAssets}
          lock={searchLock}
          subject={subject}
          nativeToolChoice={nativeToolTypeSelectable
            ? {
              offered: NATIVE_SEARCH_TOOLS,
              selected: value.nativeSearchTool,
              onSelect: (version: NativeSearchTool) => onChange({ nativeSearchTool: version })
            }
            : undefined}
        />

        <SearchLegParams
          selection={value.provider}
          maxResults={value.maxResults}
          compressionCutoff={value.compressionCutoff}
          onChangeMaxResults={(maxResults) => onChange({ maxResults })}
          onChangeCompressionCutoff={(compressionCutoff) => onChange({ compressionCutoff })}
        />
      </div>
    )}

    <LockableSwitchRow
      title={t("网页抓取", "Web fetch")}
      description={<>
        {role
          ? t("让这个角色读取指定网址的页面。", "Lets this role read pages at given addresses.")
          : t("让模型读取指定网址的页面。", "Lets the model read pages at given addresses.")}
        <DocsLink inline page="web" section={role ? "enable" : "fetch"} />
      </>}
      checked={fetchOn}
      tone={switchTones?.fetch}
      hints={lockHints}
      disabled={Boolean(settledNote(fetchLock))}
      disabledNote={settledNote(fetchLock)}
      onChange={(on) => flip("fetch", on)}
      label={fetchOn
        ? t("网页抓取已开启", "Web fetch enabled")
        : t("网页抓取已关闭", "Web fetch disabled")}
    />
    {fetchOn && (
      <div className="web-search-provider-field">
        <FetchProviderField
          value={value.fetchProvider}
          onChange={(fetchProvider) => onChange({ fetchProvider })}
          webSearchAssets={webSearchAssets}
          lock={fetchLock}
          subject={subject}
          nativeOffered={nativeFetchAvailable}
          nativeToolChoice={fetchVersionSelectable
            ? {
              selected: value.nativeFetchTool,
              onSelect: (version) => onChange({ nativeFetchTool: version })
            }
            : undefined}
        />

        {/* The page cap is a parameter of the Messages fetch tool, so a native
            fetch carries it only where that tool is what goes out. */}
        <FetchLegParams
          selection={value.fetchProvider}
          nativeOffered={fetchVersionSelectable}
          forRole={role}
          compressionCutoff={value.fetchCompressionCutoff}
          onChangeCompressionCutoff={(fetchCompressionCutoff) => onChange({ fetchCompressionCutoff })}
        />
      </div>
    )}

    {(searchOn || fetchOn) && (
      <div className="web-search-provider-field">
        <SearchDomainFilterRow
          mode={value.domainFilter}
          includeDomains={value.includeDomains}
          excludeDomains={value.excludeDomains}
          windowOpen={domainWindowOpen}
          onOpenWindow={() => setDomainWindowOpen(true)}
          onCloseWindow={() => setDomainWindowOpen(false)}
          onChangeMode={(domainFilter) => onChange({ domainFilter })}
          onChangeRules={(list, rules) => onChange(
            list === "include" ? { includeDomains: rules } : { excludeDomains: rules }
          )}
          hint={<>
            {t("按域名筛选检索结果。", "Filters results by domain.")}
            <DocsLink inline page="web" section="domains" />
          </>}
        />
      </div>
    )}
  </>;
}
