import { ChevronDown } from "lucide-react";
import { useI18n } from "../i18n";
import { SEARCH_PROVIDERS } from "../lib/searchProviders";
import { selectedProviderProblem } from "../lib/webSearch";
import type { FetchProviderSelection, NativeFetchTool, WebSearchAssets } from "../types";
import { NATIVE_FETCH_TOOLS } from "../types";
import { Field } from "./Common";
import { backendLockTone, LockMark, lockedFieldHint, lockToneClass, type BackendLock } from "./LockTone";
import { PopoverMenu } from "./PopoverMenu";
import type { PopoverMenuItem, PopoverMenuSection } from "./PopoverMenu";
import type { WebSearchSubject } from "./SearchProviderField";

/**
 * The Messages `web_fetch` versions this surface may pick between, and which
 * one it is carrying. Absent wherever there is no version to offer.
 */
export interface NativeFetchToolChoice {
  selected: NativeFetchTool;
  onSelect: (next: NativeFetchTool) => void;
}

interface FetchProviderFieldProps {
  value: FetchProviderSelection;
  onChange: (next: FetchProviderSelection) => void;
  webSearchAssets: WebSearchAssets;
  hint?: string;
  /**
   * What the conversation says about this selector: settled cannot move, and
   * its note replaces the standing advice to say why; orange moves, and its
   * note says what moving it costs.
   */
  lock?: BackendLock | null;
  /** Opens the native row into a second step naming the wire tool version. */
  nativeToolChoice?: NativeFetchToolChoice;
  /**
   * Whose backend this is, for the standing advice: a conversation's, whose
   * native fetch is its own model provider's, or a role's, whose native fetch is
   * the provider of the model the role runs on. Defaults to the conversation.
   */
  subject?: WebSearchSubject;
}

/**
 * Shared fetch-provider picker for conversations, presets and subagent roles.
 *
 * The twin of `SearchProviderField`, and deliberately its own component rather
 * than a second spelling inside the conversation's web settings: a role picks
 * its fetch backend on the same terms a conversation does, and one recipe is
 * what keeps the two from drifting. Lists only catalog entries that can fetch
 * and are switched on; a selection naming a provider that has since been
 * switched off or lost is said plainly rather than silently repaired, and
 * `web_fetch` stays offered meanwhile. One still waiting for its API key is an
 * ordinary choice: the fetch fails when it is made, and says so there.
 */
export function FetchProviderField({
  value,
  onChange,
  webSearchAssets,
  hint,
  lock,
  nativeToolChoice,
  subject = "conversation"
}: FetchProviderFieldProps) {
  const { t } = useI18n();
  const fetchProviders = SEARCH_PROVIDERS.filter((provider) => provider.fetch
    && webSearchAssets.providers.some((item) => item.kind === provider.kind && item.enabled));
  const nativeLabel = t("原生", "Native");
  const offLabel = t("不启用", "Off");
  const isNative = value.kind === "native";
  /* The version only belongs on the trigger where it is actually sent. */
  const triggerVersion = isNative && nativeToolChoice ? nativeToolChoice.selected : null;
  /* A named provider that is no longer usable. Said plainly rather than quietly
     redrawn as "off": the selection is still carried and `web_fetch` is still
     offered, and showing it as a deliberate choice would hide a leg that has
     stopped working. */
  const unavailable = selectedProviderProblem(value, webSearchAssets, "fetchUrls") !== null;
  const triggerLabel = unavailable
    ? t("请修复抓取提供商", "Repair fetch provider")
    : value.kind === "native"
      ? nativeLabel
      : value.kind === "disabled"
        ? offLabel
        : value.kind === "explicit"
          ? fetchProviders.find((provider) => provider.kind === value.providerKind)?.label ?? ""
          : "";

  const nativeRow: PopoverMenuItem = {
    id: "native",
    label: nativeLabel,
    checked: isNative,
    hint: triggerVersion ?? undefined,
    children: nativeToolChoice
      ? NATIVE_FETCH_TOOLS.map((version) => ({
        id: version,
        label: version,
        // Checked by the version this surface carries, not by whether native is
        // the backend right now: the second step answers "which spelling", and
        // it keeps its answer while another backend fetches.
        checked: nativeToolChoice.selected === version,
        onSelect: () => {
          onChange({ kind: "native" });
          nativeToolChoice.onSelect(version);
        }
      }))
      : undefined,
    onSelect: nativeToolChoice ? undefined : () => onChange({ kind: "native" })
  };

  const sections: PopoverMenuSection[] = [{
    id: "backends",
    items: [
      nativeRow,
      ...fetchProviders.map((provider) => ({
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
    label={t("抓取提供商", "Fetch provider")}
    hint={lockedFieldHint(lock, hint ?? (subject === "role"
      ? t(
        "原生用这个角色所跑模型自带的抓取，部分模型在搜索里一并完成。",
        "Native uses the built-in fetch of the model this role runs on; some models do it inside search."
      )
      : t(
        "原生用模型自带的抓取，部分模型在搜索里一并完成。",
        "Native uses the model's own fetch; some models do it inside search."
      )))}
  >
    <PopoverMenu
      rootClassName="popover-select"
      triggerClassName={`input popover-select__trigger${unavailable ? " input--error" : ""}${lockToneClass("popover-select__trigger", tone)}`}
      triggerLabel={t("抓取提供商：{value}", "Fetch provider: {value}", { value: triggerLabel })}
      trigger={<>
        <span className="popover-select__value">{triggerLabel}</span>
        {triggerVersion && <span className="popover-select__note">{triggerVersion}</span>}
        <LockMark tone={tone} />
        <ChevronDown size={14} className="popover-select__chevron" aria-hidden="true" />
      </>}
      disabled={disabled}
      submenu="flyout"
      menuLabel={t("抓取提供商", "Fetch provider")}
      sections={sections}
    />
  </Field>;
}
