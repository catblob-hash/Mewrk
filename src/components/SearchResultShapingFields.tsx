import { type ReactNode, useState } from "react";
import { useI18n } from "../i18n";
import {
  SEARCH_COMPRESSION_CUTOFF_CEILING,
  searchProviderCapability
} from "../lib/searchProviders";
import type { FetchProviderSelection, SearchProviderSelection } from "../types";
import { Field } from "./Common";
import { DocsLink } from "./DocsLink";

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
  hint: ReactNode;
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
  hint: ReactNode;
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
  let compression: { floor: number; hint: ReactNode } | null = null;

  if (selection.kind === "explicit") {
    const kind = selection.providerKind;
    const spec = searchProviderCapability(kind, "searchKeywords");
    if (spec?.maxResults != null) {
      count = {
        ceiling: spec.maxResults,
        hint: kind === "searxng"
          ? t(
            "读取的结果页数，最多 {max}；0 为全部。",
            "Result pages to read, up to {max}; 0 reads all.",
            { max: spec.maxResults }
          )
          : t(
            "最多 {max}；0 使用后端默认值。",
            "Up to {max}; 0 uses the backend's default.",
            { max: spec.maxResults }
          )
      };
    }
    if (spec?.minContentTokens != null) {
      compression = {
        floor: spec.minContentTokens,
        hint: <>
          {t("每条结果最多保留多少 token；0 不设限。", "Most tokens kept per result; 0 means no limit.")}
          <DocsLink inline page="web" section="results" />
        </>
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

  let compression: { floor: number; hint: ReactNode } | null = null;

  if (selection.kind === "native") {
    if (nativeOffered) {
      compression = {
        floor: 1,
        hint: forRole
          ? t(
            "每页上限，仅 Anthropic 家族模型会发送。",
            "Per-page cap, sent only on Anthropic-family models."
          )
          : t("每个页面最多保留多少 token；0 不设限。", "Most tokens kept per page; 0 means no limit.")
      };
    }
  } else if (selection.kind === "explicit") {
    const spec = searchProviderCapability(selection.providerKind, "fetchUrls");
    if (spec?.minContentTokens != null) {
      compression = {
        floor: spec.minContentTokens,
        hint: <>
          {t("每个页面最多保留多少 token；0 不设限。", "Most tokens kept per page; 0 means no limit.")}
          <DocsLink inline page="web" section="results" />
        </>
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
