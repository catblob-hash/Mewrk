import { Cloud, Plus, Search } from "lucide-react";
import { useI18n } from "../../i18n";
import { isBuiltinProvider } from "../../lib/codexProvider";
import type { ApiProvider } from "../../types";
import { SettingsRail, SettingsRailDelete, SettingsRailEmpty } from "../SettingsRail";
import type { ReorderDropTarget } from "../usePointerDrag";
import type { usePointerDrag } from "../usePointerDrag";
import { ProviderAvatar } from "./ProviderAvatar";

export type RailFilter = "all" | "enabled" | "disabled";

type ProviderSort = ReturnType<typeof usePointerDrag<string, ReorderDropTarget>>;

function providerLabel(provider: ApiProvider, untitled: string): string {
  return provider.name.trim() || untitled;
}

/** The modifier the keyboard reorder listens for (`altKey`), named the way the keyboard labels it. */
function reorderModifierName(): string {
  const apple = typeof navigator !== "undefined" && /^(Mac|iPhone|iPad)/.test(navigator.platform ?? "");
  return apple ? "Option" : "Alt";
}

/**
 * Provider rail with search, filters, provider rows, and a persistent add-provider action.
 *
 * `SettingsRail` supplies the search, scrolling, and footer shell shared with
 * conversation presets and search providers. This component owns provider-specific
 * rows, including drag reordering and the delete action.
 *
 * Every provider is user-created and deletable except the built-in Codex row. The enable switch is only in the
 * detail header; an enabled dot yields to the delete button on hover or keyboard focus.
 */
export function ProviderRail({
  providers,
  totalCount,
  selectedId,
  query,
  onQueryChange,
  filter,
  onFilterChange,
  reorderable,
  sort,
  onSelect,
  onMoveByKeyboard,
  onAdd,
  onDelete,
  mutationDisabled
}: {
  providers: ApiProvider[];
  totalCount: number;
  selectedId: string | null;
  query: string;
  onQueryChange: (query: string) => void;
  filter: RailFilter;
  onFilterChange: (filter: RailFilter) => void;
  reorderable: boolean;
  sort: ProviderSort;
  onSelect: (providerId: string) => void;
  onMoveByKeyboard: (providerId: string, direction: -1 | 1) => void;
  onAdd: () => void;
  onDelete: (providerId: string) => void;
  mutationDisabled: boolean;
}) {
  const { t } = useI18n();
  const untitled = t("未命名提供商", "Untitled provider");
  /* Reordering moves rows by list index, so it is off whenever the list shows a
     subset; the hint says which of the two narrowings is in the way. */
  const searching = query.trim() !== "";
  const filtering = filter !== "all";
  const modifier = reorderModifierName();
  const reorderHint = reorderable
    ? t("拖动整行或按 {key}+↑/↓ 排序", "Drag the row or press {key}+↑/↓ to reorder", { key: modifier })
    : searching && filtering
      ? t("搜索和筛选期间不能排序", "Reordering is unavailable while searching and filtering")
      : searching
        ? t("搜索期间不能排序", "Reordering is unavailable while searching")
        : t("只显示已启用或已停用的提供商时不能排序", "Reordering is unavailable while the list is filtered to enabled or disabled providers");

  const filterOptions: Array<{ value: RailFilter; label: string }> = [
    { value: "all", label: t("全部提供商", "All providers") },
    { value: "enabled", label: t("仅已启用", "Enabled only") },
    { value: "disabled", label: t("仅已停用", "Disabled only") }
  ];

  const renderRow = (provider: ApiProvider) => {
    const label = providerLabel(provider, untitled);
    const selected = provider.id === selectedId;
    const dropTarget = sort.dropTarget?.id === provider.id ? ` drop-target--${sort.dropTarget.position}` : "";
    return (
      <div className="provider-rail__slot" key={provider.id}>
        {/* biome-ignore lint/a11y/useSemanticElements: The row contains a delete button and cannot be a button itself. */}
        <div
          role="button"
          tabIndex={0}
          data-sortable-id={provider.id}
          data-selected={selected ? "true" : "false"}
          aria-current={selected || undefined}
          // The nested delete button must not contribute to the row's accessible name.
          aria-label={label}
          title={reorderHint}
          aria-keyshortcuts="Alt+ArrowUp Alt+ArrowDown"
          className={`provider-rail__row sortable-surface${sort.activeItem === provider.id ? " sortable-surface--dragging" : ""}${dropTarget}`}
          onClick={() => onSelect(provider.id)}
          onKeyDown={(event) => {
            if (event.currentTarget !== event.target) return;
            if (event.key === "Enter" || event.key === " ") {
              event.preventDefault();
              onSelect(provider.id);
              return;
            }
            if (!reorderable) return;
            if (!event.altKey || (event.key !== "ArrowUp" && event.key !== "ArrowDown")) return;
            event.preventDefault();
            onMoveByKeyboard(provider.id, event.key === "ArrowUp" ? -1 : 1);
          }}
          {...(reorderable ? sort.bind(provider.id) : {})}
        >
          <ProviderAvatar name={label} />
          <span className="provider-rail__name">{label}</span>
          <span className="provider-rail__trailing">
            {provider.enabled && <span className="provider-rail__dot" aria-hidden="true" />}
            {!isBuiltinProvider(provider) && (
              <SettingsRailDelete
                name={label}
                disabled={mutationDisabled}
                onDelete={() => onDelete(provider.id)}
              />
            )}
          </span>
        </div>
      </div>
    );
  };

  return (
    <SettingsRail
      search={{
        value: query,
        onChange: onQueryChange,
        label: t("搜索提供商", "Search providers"),
        placeholder: t("搜索提供商或模型", "Search providers or models")
      }}
      filter={{
        label: t("筛选提供商", "Filter providers"),
        value: filter,
        neutralValue: "all",
        options: filterOptions,
        onChange: (value) => onFilterChange(value as RailFilter)
      }}
      sortableListId="api-providers"
      footer={(
        <button type="button" className="provider-rail__add" onClick={onAdd}>
          <Plus size={13} /> {t("添加提供商", "Add provider")}
        </button>
      )}
    >
      {providers.map((provider) => renderRow(provider))}
      {!totalCount && (
        <SettingsRailEmpty
          icon={<Cloud size={18} />}
          title={t("还没有提供商", "No providers yet")}
          description={t("添加一个自定义提供商。", "Add a custom provider.")}
        />
      )}
      {Boolean(totalCount) && !providers.length && (
        <SettingsRailEmpty
          icon={<Search size={18} />}
          title={t("没有匹配的提供商", "No matching provider")}
          description={t("搜索会匹配提供商名与它下面的模型 ID。", "Search matches provider names and the model IDs under them.")}
        />
      )}
    </SettingsRail>
  );
}
