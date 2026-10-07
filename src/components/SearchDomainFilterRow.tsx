import { useI18n } from "../i18n";
import type { SearchDomainFilterMode } from "../types";
import { SearchDomainRulesWindow } from "./SearchDomainRulesWindow";

/** Which of the two lists the rules window edits. */
type DomainList = Exclude<SearchDomainFilterMode, "off">;

interface SearchDomainFilterRowProps {
  mode: SearchDomainFilterMode;
  includeDomains: readonly string[];
  excludeDomains: readonly string[];
  hint: string;
  windowOpen: boolean;
  onOpenWindow: () => void;
  onCloseWindow: () => void;
  onChangeMode: (next: SearchDomainFilterMode) => void;
  onChangeRules: (list: DomainList, rules: string[]) => void;
}

/**
 * The domain-filter row: which list is in effect, and the way in to writing
 * both of them.
 *
 * Deliberately NOT a `Field`: that renders a `<label>`, and a label makes the
 * whole row a click target for the first labelable thing inside it — which here
 * is the link that opens a window, not the picker. A row that opens a window
 * when its description is clicked is one nobody asked for.
 *
 * Shared by conversations, presets and subagent roles, so they cannot drift: a
 * role filters by its own mode and its own two lists, never by its caller's.
 */
export function SearchDomainFilterRow({
  mode,
  includeDomains,
  excludeDomains,
  hint,
  windowOpen,
  onOpenWindow,
  onCloseWindow,
  onChangeMode,
  onChangeRules
}: SearchDomainFilterRowProps) {
  const { t } = useI18n();
  /* The window opens on the list the selector has in effect. With filtering off
     there is no list in effect, so it opens on the blocklist: the one a person
     reaching for this row almost always means. */
  const openDomainList: DomainList = mode === "include" ? "include" : "exclude";

  return <>
    <div className="field">
      <span className="field__label">{t("域名过滤", "Domain filter")}</span>
      <div className="field__control-pair">
        <button
          type="button"
          className="text-button"
          onClick={onOpenWindow}
        >{t("编辑名单", "Edit lists")}</button>
        <select
          className="input"
          aria-label={t("域名过滤", "Domain filter")}
          value={mode}
          onChange={(event) => onChangeMode(event.target.value as SearchDomainFilterMode)}
        >
          <option value="exclude">{t("启用黑名单", "Use blocklist")}</option>
          <option value="include">{t("启用白名单", "Use allowlist")}</option>
          <option value="off">{t("不启用", "Off")}</option>
        </select>
      </div>
      <span className="field__hint">{hint}</span>
    </div>

    {windowOpen && (
      <SearchDomainRulesWindow
        includeDomains={includeDomains}
        excludeDomains={excludeDomains}
        initialList={openDomainList}
        onChange={onChangeRules}
        onClose={onCloseWindow}
      />
    )}
  </>;
}
