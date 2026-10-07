import { ChevronDown } from "lucide-react";
import { useI18n } from "../i18n";
import { SEARCH_PROVIDERS } from "../lib/searchProviders";
import { selectedProviderProblem } from "../lib/webSearch";
import type {
  NativeSearchTool,
  SearchProviderSelection,
  WebSearchAssets
} from "../types";
import { Field } from "./Common";
import { backendLockTone, LockMark, lockedFieldHint, lockToneClass, type BackendLock } from "./LockTone";
import { PopoverMenu } from "./PopoverMenu";
import type { PopoverMenuItem, PopoverMenuSection } from "./PopoverMenu";

/**
 * The Messages `web_search` versions this conversation may pick between, and
 * which one it is carrying.
 *
 * Absent on every surface that has no version to offer — a conversation, a
 * preset or a role whose model speaks another protocol — and the native row is
 * then a plain choice that selects immediately. The version is deliberately not
 * cleared in that case: it stays on the surface so that coming back to a
 * Messages model comes back to the same version.
 */
export interface NativeSearchToolChoice {
  offered: readonly NativeSearchTool[];
  selected: NativeSearchTool;
  onSelect: (next: NativeSearchTool) => void;
}

/**
 * Whose web settings a search or fetch picker is drawn for, so its standing
 * advice names the right model: a `conversation`'s native backend is its own
 * model's, a `role`'s is the model the role runs on.
 */
export type WebSearchSubject = "conversation" | "role";

interface SearchProviderFieldProps {
  value: SearchProviderSelection;
  onChange: (next: SearchProviderSelection) => void;
  webSearchAssets: WebSearchAssets;
  hint?: string;
  /**
   * What the conversation says about this selector: settled cannot move, and
   * its note replaces the standing advice to say why; orange moves, and its
   * note says what moving it costs.
   */
  lock?: BackendLock | null;
  /** Opens the native row into a second step naming the wire tool version. */
  nativeToolChoice?: NativeSearchToolChoice;
  /**
   * Whose backend this is, for the standing advice: a conversation's, whose
   * native search is its own model's, or a role's, whose native search is the
   * model the role runs on. Defaults to the conversation.
   */
  subject?: WebSearchSubject;
}

/**
 * Shared search-provider picker for conversations, presets and subagent roles.
 *
 * Lists only catalog entries that both do keyword search and are switched on:
 * a fetch-only provider cannot satisfy this selection, and a disabled one is
 * not a choice — offering it greyed out would make the menu a list of things
 * that do not work. A selection naming a provider that has since been switched
 * off is not silently repaired; the trigger says so and the menu is where a
 * working one is picked instead. A provider still waiting for its API key is
 * an ordinary choice: the search fails when it is made, and says so there.
 *
 * It is a menu rather than a `<select>` because the native choice has a second
 * step under it on Messages models — which version of the server-side tool to
 * send — and a second step is exactly what an option list cannot hold.
 */
export function SearchProviderField({
  value,
  onChange,
  webSearchAssets,
  hint,
  lock,
  nativeToolChoice,
  subject = "conversation"
}: SearchProviderFieldProps) {
  const { t } = useI18n();
  const searchProviders = SEARCH_PROVIDERS.filter((provider) => provider.search
    && webSearchAssets.providers.some((item) => item.kind === provider.kind && item.enabled));
  const unavailable = selectedProviderProblem(value, webSearchAssets, "searchKeywords") !== null;
  const isNative = value.kind === "native";
  const nativeLabel = t("原生", "Native");
  const offLabel = t("不启用", "Off");
  const providerLabel = value.kind === "explicit"
    ? searchProviders.find((provider) => provider.kind === value.providerKind)?.label
    : undefined;
  const triggerLabel = unavailable
    ? t("请修复搜索提供商", "Repair search provider")
    : isNative
      ? nativeLabel
      : value.kind === "disabled" ? offLabel : providerLabel ?? "";
  /* The version only belongs on the trigger where it is actually sent. On every
     other model the conversation still carries it, and saying so here would
     claim an effect this request does not have. */
  const triggerVersion = isNative && nativeToolChoice ? nativeToolChoice.selected : null;

  const nativeRow: PopoverMenuItem = {
    id: "native",
    label: nativeLabel,
    checked: isNative,
    hint: triggerVersion ?? undefined,
    children: nativeToolChoice?.offered.map((version) => ({
      id: version,
      label: version,
      // Checked by the version this conversation carries, not by whether native
      // is the backend right now: the second step answers "which spelling", and
      // it keeps its answer while another backend searches.
      checked: nativeToolChoice.selected === version,
      onSelect: () => {
        onChange({ kind: "native" });
        nativeToolChoice.onSelect(version);
      }
    })),
    onSelect: nativeToolChoice ? undefined : () => onChange({ kind: "native" })
  };

  const sections: PopoverMenuSection[] = [{
    id: "backends",
    items: [
      nativeRow,
      ...searchProviders.map((provider) => ({
        id: provider.kind,
        label: provider.label,
        checked: value.kind === "explicit" && value.providerKind === provider.kind,
        onSelect: () => onChange({ kind: "explicit", providerKind: provider.kind })
      })),
      {
        id: "disabled",
        label: offLabel,
        checked: value.kind === "disabled",
        onSelect: () => onChange({ kind: "disabled" })
      }
    ]
  }];

  const disabled = lock?.kind === "settled";
  const tone = backendLockTone(lock);
  return <Field
    label={t("搜索提供商", "Search provider")}
    hint={lockedFieldHint(lock, hint ?? (subject === "role"
      ? t(
        "原生用这个角色所跑模型自带的搜索；选提供商则由 Mewrk 代为检索。",
        "Native uses the built-in search of the model this role runs on; with a provider, Mewrk searches instead."
      )
      : t(
        "原生用模型自带的搜索；选提供商则由 Mewrk 代为检索。",
        "Native uses the model's own search; with a provider, Mewrk searches instead."
      )))}
  >
    <PopoverMenu
      rootClassName="popover-select"
      triggerClassName={`input popover-select__trigger${unavailable ? " input--error" : ""}${lockToneClass("popover-select__trigger", tone)}`}
      triggerLabel={t("搜索提供商：{value}", "Search provider: {value}", { value: triggerLabel })}
      trigger={<>
        <span className="popover-select__value">{triggerLabel}</span>
        {triggerVersion && <span className="popover-select__note">{triggerVersion}</span>}
        <LockMark tone={tone} />
        <ChevronDown size={14} className="popover-select__chevron" aria-hidden="true" />
      </>}
      disabled={disabled}
      submenu="flyout"
      sections={sections}
      menuLabel={t("搜索提供商", "Search provider")}
    />
  </Field>;
}
