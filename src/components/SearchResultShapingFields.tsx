import { type ReactNode, useState } from "react";
import { useI18n } from "../i18n";
import {
  SEARCH_COMPRESSION_CUTOFF_CEILING,
  searchProviderCapability,
  searchProviderEntry
} from "../lib/searchProviders";
import type { FetchProviderSelection, SearchProviderKind, SearchProviderSelection } from "../types";
import { Field } from "./Common";

/**
 * A whole-number row whose value is edited in place.
 *
 * The field cannot render the committed number back while it is being typed:
 * clearing it to type a different one would put the old value straight back
 * under the cursor. The draft holds what was typed, and a keystroke that reads
 * as a number commits it while one that does not — an empty field mid-edit —
 * simply leaves the last committed value alone.
 *
 * What a keystroke commits is `normalize`d first, and what an idle field shows
 * is `value` as given: the caller owns both ends of the range because the range
 * is the selected backend's, not the field's. The draft keeps a half-typed
 * number on screen even while the committed one is being clamped up to a floor,
 * so typing "1000" into a field whose floor is 500 does not fight the cursor.
 */
function NumberField({
  label,
  ariaLabel,
  hint,
  value,
  max,
  normalize,
  onChange
}: {
  label: string;
  ariaLabel: string;
  hint: string;
  value: number;
  max: number;
  normalize: (next: number) => number;
  onChange: (next: number) => void;
}) {
  const [draft, setDraft] = useState<string | null>(null);
  return (
    <Field label={label} hint={hint}>
      <input
        className="input"
        type="number"
        min={0}
        max={max}
        step={1}
        aria-label={ariaLabel}
        value={draft ?? String(value)}
        onChange={(event) => {
          setDraft(event.target.value);
          const next = Number(event.target.value);
          if (event.target.value.trim() === "" || !Number.isSafeInteger(next)) return;
          onChange(normalize(next));
        }}
        onBlur={() => setDraft(null)}
      />
    </Field>
  );
}

/**
 * How many results one search asks the backend for.
 *
 * The stored number is the user's answer and the ceiling is the backend's: a
 * stored 30 on a backend that takes 20 is shown as 20 — which is what is sent —
 * without rewriting the 30, so choosing a wider backend again finds it still
 * there. 0 sends nothing and leaves the backend's own default.
 */
function ResultCountRow({
  value,
  ceiling,
  hint,
  onChange
}: {
  value: number;
  ceiling: number;
  hint: string;
  onChange: (next: number) => void;
}) {
  const { t } = useI18n();
  const label = t("结果数", "Result count");
  return (
    <NumberField
      label={label}
      ariaLabel={label}
      hint={hint}
      value={Math.min(value, ceiling)}
      max={ceiling}
      normalize={(next) => Math.min(Math.max(next, 0), ceiling)}
      onChange={onChange}
    />
  );
}

/**
 * How many tokens of one result's (or one page's) text are kept.
 *
 * 0 is "no cap" and is never raised to the floor: the floor is the smallest cap
 * the backend accepts, not the smallest value the field holds. Like the count,
 * the stored number is shown through the backend's own limit — a stored 100 on a
 * backend whose floor is 500 reads 500, the value the request will carry.
 */
function CompressionRow({
  ariaLabel,
  value,
  floor,
  hint,
  onChange
}: {
  ariaLabel: string;
  value: number;
  floor: number;
  hint: string;
  onChange: (next: number) => void;
}) {
  const { t } = useI18n();
  return (
    <NumberField
      label={t("结果压缩", "Result compression")}
      ariaLabel={ariaLabel}
      hint={hint}
      value={value === 0 ? 0 : Math.max(value, floor)}
      max={SEARCH_COMPRESSION_CUTOFF_CEILING}
      normalize={(next) => (
        next <= 0 ? 0 : Math.min(Math.max(next, floor), SEARCH_COMPRESSION_CUTOFF_CEILING)
      )}
      onChange={onChange}
    />
  );
}

/**
 * The name each catalog backend gives its own result-count parameter, for the
 * hint to quote. SearXNG is absent on purpose: its API has no count, so its hint
 * says what mewrk does with the number instead of naming a field.
 */
const RESULT_COUNT_FIELD: Partial<Record<SearchProviderKind, string>> = {
  zhipu: "count",
  tavily: "max_results",
  exa: "numResults",
  "exa-mcp": "numResults",
  bocha: "count",
  querit: "count",
  jina: "count",
  firecrawl: "limit"
};

/** The rows of one leg sit under that leg's selector, as a group that reads as belonging to it. */
function LegParams({ children }: { children: ReactNode }) {
  return <div className="web-search-leg-params">{children}</div>;
}

/**
 * The search leg's own parameters: how many results a search asks for, and how
 * many tokens of each result's text are kept.
 *
 * Drawn directly under the search selector, and only the rows the selected
 * backend actually has. These numbers ARE that backend's request parameters —
 * so a backend with no count field draws no count row, and a backend with no
 * content-length parameter draws no compression row, rather than offering a
 * knob that does nothing. A native search has neither on any family and draws
 * nothing at all.
 *
 * Both read 0 as "no limit", which is why neither has a switch beside it: the
 * off state is a value the field can already hold.
 */
export function SearchLegParams({
  selection,
  maxResults,
  compressionCutoff,
  onChangeMaxResults,
  onChangeCompressionCutoff
}: {
  selection: SearchProviderSelection;
  maxResults: number;
  compressionCutoff: number;
  onChangeMaxResults: (next: number) => void;
  onChangeCompressionCutoff: (next: number) => void;
}) {
  const { t } = useI18n();

  let count: { ceiling: number; hint: string } | null = null;
  let compression: { floor: number; hint: string } | null = null;

  if (selection.kind === "explicit") {
    const kind = selection.providerKind;
    const spec = searchProviderCapability(kind, "searchKeywords");
    const label = searchProviderEntry(kind).label;
    if (spec?.maxResults != null) {
      const field = RESULT_COUNT_FIELD[kind];
      count = {
        ceiling: spec.maxResults,
        hint: kind === "searxng"
          ? t(
            "SearXNG 的接口没有结果数参数，这里是 mewrk 自己读取的结果页面数；0 表示返回多少读多少，最多 {max}。",
            "SearXNG's API has no result-count parameter; this is how many result pages mewrk reads itself. 0 reads every result; at most {max}.",
            { max: spec.maxResults }
          )
          : t(
            "作为 {label} 的 {field} 发送，0 表示不发送、用 {label} 自己的默认值；最多 {max}。",
            "Sent as {label}'s {field}; 0 sends nothing and leaves {label}'s own default. At most {max}.",
            { label, field: field ?? "", max: spec.maxResults }
          )
      };
    }
    if (spec?.minContentTokens != null) {
      const min = spec.minContentTokens;
      compression = {
        floor: min,
        hint: kind === "exa"
          ? t(
            "每条结果正文最多多少 token，按约 4 字符一个 token 换算成 Exa 的 contents.text.maxCharacters 发送；0 表示不设限，Exa 会返回整页正文。",
            "The most tokens of body text per result, converted at about 4 characters a token into Exa's contents.text.maxCharacters. 0 means no limit, and Exa returns each page's full text."
          )
          : kind === "jina"
            ? t(
              "每条结果正文最多多少 token，作为 Jina 的 X-Max-Tokens 请求头发送，至少 {min}；0 表示不设限，Jina 会返回整页正文。",
              "The most tokens of body text per result, sent as Jina's X-Max-Tokens header (at least {min}). 0 means no limit, and Jina returns each page's full text.",
              { min }
            )
            : kind === "searxng"
              ? t(
                "每个结果页面正文最多多少 token。SearXNG 只给链接，页面由 mewrk 自己读取并在本地截断；0 表示不设限。",
                "The most tokens of body text per result page. SearXNG only returns links; mewrk reads the pages itself and truncates them locally. 0 means no limit."
              )
              : t(
                "每条结果正文最多多少 token，0 表示不设限。只有带正文长度参数的后端会用它（Exa、Jina、SearXNG），其它后端原样返回。",
                "The most tokens of body text kept per result; 0 means no limit. Only backends with a length parameter use it (Exa, Jina, SearXNG); the rest return what they return."
              )
      };
    }
  }

  if (!count && !compression) return null;
  return (
    <LegParams>
      {count && (
        <ResultCountRow
          value={maxResults}
          ceiling={count.ceiling}
          hint={count.hint}
          onChange={onChangeMaxResults}
        />
      )}
      {compression && (
        <CompressionRow
          ariaLabel={t("搜索结果压缩", "Search result compression")}
          value={compressionCutoff}
          floor={compression.floor}
          hint={compression.hint}
          onChange={onChangeCompressionCutoff}
        />
      )}
    </LegParams>
  );
}

/**
 * The fetch leg's own parameter: how many tokens of one fetched page are kept.
 *
 * A fetch never has a result count, so this is a single row, drawn under the
 * fetch selector when the selected backend has a length parameter of its own
 * (Jina Reader), or when mewrk itself fetches and truncates the page (the
 * local `fetch`). A native fetch has one only where it exists as a separate
 * server tool — the Anthropic family, as `max_content_tokens` — and on every
 * other family the native leg is ignored and draws nothing.
 *
 * `nativeOffered` is whether a native fetch can carry the number. A role's
 * editor cannot be sure: a role that rides the caller's model meets whichever
 * family calls it, so for a role (`forRole`) the row is drawn and its hint says
 * that only an Anthropic-family model sends it.
 */
export function FetchLegParams({
  selection,
  nativeOffered,
  forRole,
  compressionCutoff,
  onChangeCompressionCutoff
}: {
  selection: FetchProviderSelection;
  nativeOffered: boolean;
  forRole: boolean;
  compressionCutoff: number;
  onChangeCompressionCutoff: (next: number) => void;
}) {
  const { t } = useI18n();

  const genericHint = t(
    "抓取一个页面最多保留多少 token，0 表示不设限。只有带这个参数的后端会用它（Jina、本地 fetch、Anthropic 家族的原生抓取）。",
    "The most tokens kept from one fetched page; 0 means no limit. Only backends with this parameter use it (Jina, local fetch, and native fetch on the Anthropic family)."
  );
  let compression: { floor: number; hint: string } | null = null;

  if (selection.kind === "native") {
    if (nativeOffered) {
      compression = {
        floor: 1,
        hint: t(
          "作为 Anthropic web_fetch 的 max_content_tokens 发送，超出部分由上游截掉；0 表示不设限。",
          "Sent as Anthropic web_fetch's max_content_tokens; the upstream truncates the page to it. 0 means no limit."
        ) + (forRole
          ? t(
            " 只有这个角色的模型属于 Anthropic 家族时才会发出。",
            " It is only sent when this role's model is in the Anthropic family."
          )
          : "")
      };
    }
  } else if (selection.kind === "explicit") {
    const kind = selection.providerKind;
    const spec = searchProviderCapability(kind, "fetchUrls");
    if (spec?.minContentTokens != null) {
      const min = spec.minContentTokens;
      compression = {
        floor: min,
        hint: kind === "jina"
          ? t(
            "作为 Jina Reader 的 X-Max-Tokens 请求头发送，超出部分由 Jina 截掉，至少 {min}；0 表示不设限。",
            "Sent as Jina Reader's X-Max-Tokens header; Jina truncates the page to it (at least {min}). 0 means no limit.",
            { min }
          )
          : kind === "fetch"
            ? t(
              "mewrk 在本地抓取页面后截断到这么多 token；0 表示不设限，整页进入上下文。",
              "mewrk fetches the page locally and truncates it to this many tokens. 0 means no limit, and the whole page enters the context."
            )
            : genericHint
      };
    }
  }

  if (!compression) return null;
  return (
    <LegParams>
      <CompressionRow
        ariaLabel={t("抓取结果压缩", "Fetch result compression")}
        value={compressionCutoff}
        floor={compression.floor}
        hint={compression.hint}
        onChange={onChangeCompressionCutoff}
      />
    </LegParams>
  );
}
