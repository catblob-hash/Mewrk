import { useState } from "react";
import { useI18n } from "../i18n";
import type {
  ConversationWebSearchSettings,
  NativeSearchTool,
  WebSearchAssets
} from "../types";
import { NATIVE_SEARCH_TOOLS } from "../types";
import { FetchProviderField } from "./FetchProviderField";
import type { BackendLock } from "./LockTone";
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
   * tool of its own. Only Anthropic-family models do; everywhere else "the
   * model's own provider fetches" means retrieval happens inside the one search
   * tool, so the choice is still offered and simply grants no second web tool.
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
}

/**
 * A conversation's, a preset's or a subagent role's web configuration. A role
 * answers every one of these for itself — the same shape, the same rows — so
 * there is no "follow the conversation" answer on any of them.
 */
export type WebSearchBehaviorSettingsProps = WebSearchBehaviorCommonProps & {
  value: ConversationWebSearchSettings;
  onChange: (patch: Partial<ConversationWebSearchSettings>) => void;
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
 * Both selectors list only backends that are switched on, and both can be
 * turned off outright: a conversation may search without fetching, fetch
 * without searching, or — with web access still on — do neither, which is a
 * conversation whose settings say plainly that it has no web tools rather than
 * one whose menus are full of choices that do not work.
 *
 * A role asks the same questions through this same component, and answers them
 * for itself: one recipe for every surface is what keeps them from drifting.
 */
export function WebSearchBehaviorSettings(props: WebSearchBehaviorSettingsProps) {
  const {
    value,
    onChange,
    webSearchAssets,
    nativeFetchAvailable = false,
    nativeToolTypeSelectable = false,
    searchLock,
    fetchLock,
    subject = "conversation"
  } = props;
  const { t } = useI18n();
  const [domainWindowOpen, setDomainWindowOpen] = useState(false);
  /* The fetch version is a Messages spelling, so it is offered only where it is
     sent — and only where the family actually grants a separate fetch tool for
     it to be written onto. */
  const fetchVersionSelectable = nativeToolTypeSelectable && nativeFetchAvailable;

  return <>
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

    <FetchProviderField
      value={value.fetchProvider}
      onChange={(fetchProvider) => onChange({ fetchProvider })}
      webSearchAssets={webSearchAssets}
      lock={fetchLock}
      subject={subject}
      nativeToolChoice={fetchVersionSelectable
        ? {
          selected: value.nativeFetchTool,
          onSelect: (version) => onChange({ nativeFetchTool: version })
        }
        : undefined}
    />

    {/* A role's editor cannot be sure which family its model is in — a role
        that rides the caller's model meets whichever one calls it — so it is
        offered a native fetch number and told when it takes effect. */}
    <FetchLegParams
      selection={value.fetchProvider}
      nativeOffered={nativeFetchAvailable || subject === "role"}
      forRole={subject === "role"}
      compressionCutoff={value.fetchCompressionCutoff}
      onChangeCompressionCutoff={(fetchCompressionCutoff) => onChange({ fetchCompressionCutoff })}
    />

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
      hint={t(
        "按域名筛掉检索结果。黑名单丢掉命中的，白名单只留下命中的，两者只有一个生效；关掉过滤不会清空已经写好的名单。",
        "Filters results by domain. A blocklist drops what it matches, an allowlist keeps only what it matches, and only one of them is ever in effect. Turning filtering off does not empty either list."
      )}
    />
  </>;
}
