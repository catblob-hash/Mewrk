import { MoreHorizontal, Pencil } from "lucide-react";
import { useI18n } from "../i18n";
import { PopoverMenu } from "./PopoverMenu";

/** A conversation preset eligible as a workspace default. The sidebar needs only its identity and display name. */
export interface WorkspacePresetOption {
  id: string;
  name: string;
}

export function WorkspaceOptionsMenu({
  workspaceName,
  presets,
  selectedPresetId,
  disabled = false,
  onSelectPreset,
  onEditProject
}: {
  workspaceName: string;
  presets: WorkspacePresetOption[];
  /** The active preset ID; an empty or unresolvable ID follows the most recent settings instead. */
  selectedPresetId: string;
  disabled?: boolean;
  onSelectPreset: (presetId: string) => void;
  /** Opens the project dialog on this project; absent for the temporary project, which has none. */
  onEditProject?: () => void;
}) {
  const { t } = useI18n();
  const selected = presets.find((preset) => preset.id === selectedPresetId) ?? null;
  const label = t("{name} 的更多选项", "More options for {name}", { name: workspaceName });

  return (
    <PopoverMenu
      rootClassName="workspace-options"
      triggerClassName="icon-button"
      trigger={<MoreHorizontal size={15} />}
      triggerLabel={label}
      disabled={disabled}
      menuLabel={label}
      dense
      anchorToPointer
      sections={[{
        id: "workspace",
        items: [...(onEditProject ? [{
          id: "edit-project",
          label: t("编辑项目…", "Edit project…"),
          icon: <Pencil size={14} />,
          onSelect: onEditProject
        }] : []), {
          id: "default-preset",
          label: t("默认对话预设", "Default conversation preset"),
          // Nothing to choose from means nothing to follow; the workspace keeps reusing its most recent settings.
          disabled: presets.length === 0,
          children: [{
            // No preset of its own: a new task starts from the settings the project last used.
            id: "last-used",
            label: t("上一次", "Last used"),
            description: t("沿用这个项目最近一次用过的设置", "Reuse the settings this project last used"),
            checked: !selected,
            onSelect: () => onSelectPreset("")
          }, ...presets.map((preset) => ({
            id: preset.id,
            label: preset.name || t("未命名预设", "Untitled preset"),
            checked: preset.id === selected?.id,
            onSelect: () => onSelectPreset(preset.id)
          }))]
        }]
      }]}
    />
  );
}
