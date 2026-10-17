import { useState } from "react";
import { Ban, ShieldCheck } from "lucide-react";
import { useI18n } from "../i18n";
import type { SearchDomainFilterMode } from "../types";
import { Dialog, DialogSidebarTitle } from "./Common";
import { DocsLink } from "./DocsLink";

/** Which of the two lists a page of this window edits. */
type DomainList = Exclude<SearchDomainFilterMode, "off">;

interface SearchDomainRulesWindowProps {
  includeDomains: readonly string[];
  excludeDomains: readonly string[];
  /** Which page opens first. The row that opens this window passes the list its
   * selector currently has in effect, so the window opens on the one being used
   * rather than on whichever happens to be first. */
  initialList?: DomainList;
  onChange: (list: DomainList, rules: string[]) => void;
  onClose: () => void;
}

/**
 * The window the two domain lists are written in.
 *
 * Laid out like the conversation-template window — a rail of pages down the
 * left, the body on the right — because it is the same move: pick which of
 * several bodies to look at. Both lists are always editable here regardless of
 * which one the selector has in effect, so the one that is off can be written
 * before it is switched on; the row that opens this window is where the choice
 * between them is made, and nothing is chosen in here.
 *
 * One rule per line, in the order they were written. Nothing is sorted or
 * de-duplicated on the way out: the list is a thing the user maintains by hand,
 * and a list that rearranges itself under an edit is one they cannot keep.
 */
export function SearchDomainRulesWindow({
  includeDomains,
  excludeDomains,
  initialList = "exclude",
  onChange,
  onClose
}: SearchDomainRulesWindowProps) {
  const { t } = useI18n();
  const [page, setPage] = useState<DomainList>(initialList);
  /* The textarea is parsed on every keystroke, so it cannot render the parsed
     value back: a blank line being typed between two rules would be swallowed
     the moment it appeared. The draft holds exactly what was typed until the
     field is left. */
  const [draft, setDraft] = useState<string | null>(null);

  const rules = page === "include" ? includeDomains : excludeDomains;
  const openPage = (next: DomainList) => {
    setDraft(null);
    setPage(next);
  };

  const railEntry = (list: DomainList, label: string, count: number) => (
    <button
      type="button"
      key={list}
      aria-current={page === list || undefined}
      className={page === list
        ? "settings-nav__item settings-nav__item--active"
        : "settings-nav__item"}
      onClick={() => openPage(list)}
    >
      {list === "exclude"
        ? <Ban size={14} aria-hidden="true" />
        : <ShieldCheck size={14} aria-hidden="true" />}
      <span>{label}</span>
      <small className="conversation-settings__nav-count">{count}</small>
    </button>
  );

  return (
    <Dialog
      title={t("域名名单", "Domain lists")}
      width="860px"
      sidebar
      bodyClassName="dialog__body--flush"
      onClose={onClose}
    >
      <div className="domain-rules">
        <div className="domain-rules__aside">
          <DialogSidebarTitle />
          <nav className="settings-nav domain-rules__nav" aria-label={t("域名名单", "Domain lists")}>
            {railEntry(
              "exclude",
              t("黑名单", "Blocklist"),
              excludeDomains.length
            )}
            {railEntry(
              "include",
              t("白名单", "Allowlist"),
              includeDomains.length
            )}
          </nav>
        </div>
        <div className="domain-rules__page">
          <h3 className="domain-rules__title">
            {page === "exclude" ? t("黑名单", "Blocklist") : t("白名单", "Allowlist")}
          </h3>
          <p className="domain-rules__help">
            {page === "exclude"
              ? t("命中的结果会被丢掉，一行一条规则。", "Matching results are dropped; one rule per line.")
              : t("只保留命中的结果，空名单什么都不留。", "Only matching results are kept; an empty list keeps nothing.")}
            <DocsLink inline page="web" section="domains" />
          </p>
          <textarea
            className="input domain-rules__input"
            aria-label={page === "exclude" ? t("黑名单", "Blocklist") : t("白名单", "Allowlist")}
            spellCheck={false}
            placeholder={"*://ads.example/*\n*://*.example.com/*\n/\\/(login|signup)$/"}
            value={draft ?? rules.join("\n")}
            onChange={(event) => {
              setDraft(event.target.value);
              onChange(
                page,
                event.target.value
                  .split("\n")
                  .map((rule) => rule.trim())
                  .filter(Boolean)
              );
            }}
            onBlur={() => setDraft(null)}
          />
        </div>
      </div>
    </Dialog>
  );
}
