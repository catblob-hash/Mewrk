import { Minus, Plus } from "lucide-react";
import type { KeyboardEvent, MouseEvent, ReactNode } from "react";
import { useI18n } from "../i18n";
import { isApplePlatform } from "../lib/shortcuts";
import type { LockTone } from "../lib/toolLock";
import { LockMark } from "./LockTone";
import { findReorderDropTarget, usePointerDrag } from "./usePointerDrag";
import type { ReorderDropTarget } from "./usePointerDrag";
import "./CatalogRow.css";

/**
 * One line in a catalog the conversation composes with — a skill, an MCP server,
 * a hook, a role, a template, a preset.
 *
 * Every catalog draws the same row, so a reader learns the shape once: what the
 * row IS goes on the left (a switch that enables it, or the arrow that applies it
 * here), what the row is CALLED fills the middle, and what can be done TO it sits
 * on the right, ending in delete. A row has no frame of its own — it is chrome-
 * free until the pointer is on it, and then it takes a grey wash. It is also
 * exactly one line tall, which means the second line the old cards carried — a
 * path on disk, a message count, a model binding — rides `detail` into `title`
 * instead.
 *
 * The row body is the drag handle that reorders the list. A row that only
 * applies or opens its entry does nothing when its body is clicked: opening on
 * click and moving on drag would get one of the two wrong. A row that is on or
 * off (`CatalogToggleRow`) is different — turning it is the one thing its body
 * could mean, and a drag never ends in a click (`usePointerDrag` swallows it).
 */

/**
 * The drag-sort a catalog list is under, shared by its `CatalogList` and every
 * `CatalogRow` in it.
 *
 * `listId` must be unique among the lists on screen at once: the preset editor is
 * this same pane opened in a dialog, so two skills lists can be mounted together
 * and `findReorderDropTarget` resolves a list id to the first element carrying
 * it. Callers key it by the conversation the pane is editing.
 */
export type CatalogSort = ReturnType<typeof useCatalogSort>;

export function useCatalogSort({ listId, ids, enabled = true, onReorder }: {
  listId: string;
  /** The row ids in their current visible order, for keyboard reordering. */
  ids: string[];
  /** False while a search or filter is on: adjacency means nothing then. */
  enabled?: boolean;
  onReorder: (sourceId: string, targetId: string, position: "before" | "after") => void;
}) {
  const drag = usePointerDrag<string, ReorderDropTarget>({
    getTarget: (point, id) => findReorderDropTarget(listId, id, point),
    onDrop: (id, target) => onReorder(id, target.id, target.position)
  });
  const moveByKeyboard = (id: string, direction: -1 | 1) => {
    const index = ids.indexOf(id);
    if (index < 0) return;
    const neighbour = ids[index + direction];
    if (!neighbour) return;
    onReorder(id, neighbour, direction < 0 ? "before" : "after");
  };
  return { listId, enabled, drag, moveByKeyboard };
}

/**
 * A catalog entry that is only ever on or off — a skill, an MCP server, a hook,
 * a role.
 *
 * It is picked the way a tool is in the tool list (`ToolSelectionGroups`): the
 * row itself is the control. It fills in green while the entry is on — orange
 * while the conversation's warm cache holds it — and the sign at its end says
 * which way a click moves it, or carries the lock while the cache does. Besides
 * turning it, the only thing to do with one of these is what the right slot
 * carries, ending in removing it from the catalog.
 */
export function CatalogToggleRow({
  id,
  name,
  label = name,
  detail,
  badge,
  icon,
  actions,
  checked,
  disabled = false,
  tone = null,
  onChange,
  sort
}: {
  /** Identity for drag-sorting. Omitted for rows that cannot be arranged. */
  id?: string;
  name: string;
  /**
   * The toggle's accessible name, when the bare name would not say what it
   * turns or would not tell two rows apart — a built-in role and a copy of it
   * share one. The name is the default.
   */
  label?: string;
  /** What the row no longer prints — a path, a reason it is inert. Goes to `title`. */
  detail?: string;
  badge?: ReactNode;
  icon?: ReactNode;
  /** What can be done to the entry itself, ending in delete. */
  actions?: ReactNode;
  checked: boolean;
  disabled?: boolean;
  /** How the conversation's lock draws the row (`LockTone.tsx`): orange warns before it moves. */
  tone?: LockTone | null;
  onChange: (checked: boolean) => void;
  sort?: CatalogSort;
}) {
  return (
    <CatalogRow
      id={id}
      name={name}
      detail={detail}
      badge={badge}
      icon={icon}
      actions={actions}
      on={checked}
      tone={tone}
      sort={sort}
      toggle={{ label, disabled, onToggle: () => onChange(!checked) }}
    />
  );
}

/**
 * A catalog entry with a state, a name and a few actions.
 *
 * `lead` is the one control that says what this entry is to this conversation —
 * a switch for something that is on or off, an apply arrow for something that is
 * copied in — and `actions` are the things done to the entry itself, which end in
 * delete. The name between them is text, not a button.
 */
export function CatalogRow({
  id,
  name,
  detail,
  badge,
  icon,
  lead,
  actions,
  nameEditor,
  on = false,
  tone = null,
  toggle,
  sort
}: {
  /** Identity for drag-sorting. Omitted for rows that cannot be arranged. */
  id?: string;
  name: string;
  /** What the row no longer prints. Goes to `title` on the row. */
  detail?: string;
  badge?: ReactNode;
  icon?: ReactNode;
  /** The leading control: an on/off switch, or the button that applies this entry. */
  lead?: ReactNode;
  actions?: ReactNode;
  /**
   * A field drawn where the name is, for a row being renamed in place.
   *
   * The rest of the row stays exactly as it was — the lead control, the icon and
   * the actions all keep their columns — so renaming moves nothing and the field
   * starts where the name it replaces started. The caller is what disables the
   * action that opened it, the way the sidebar does.
   */
  nameEditor?: ReactNode;
  /**
   * The entry this conversation currently carries: marked by its name, or —
   * on a row that is itself the toggle — filled in.
   */
  on?: boolean;
  tone?: LockTone | null;
  /**
   * Makes the row itself the control that turns the entry on or off
   * (`CatalogToggleRow`). The name becomes the toggle's button, the whole row
   * takes the click, and a sign at its end — past the actions, so the signs of
   * every row read down one column — says which way a click moves it. `lead` is
   * not drawn on such a row.
   */
  toggle?: { label: string; disabled?: boolean; onToggle: () => void };
  sort?: CatalogSort;
}) {
  const { t } = useI18n();
  const sortable = Boolean(id && sort?.enabled);
  const dragging = id !== undefined && sort?.drag.activeItem === id;
  const dropTarget = id !== undefined && sort?.drag.dropTarget?.id === id
    ? ` drop-target--${sort.drag.dropTarget.position}`
    : "";
  const title = [
    detail,
    sortable && t(
      "拖动整行排序 · {key} + ↑/↓ 键盘排序",
      "Drag the row to reorder · {key} + ↑/↓ to reorder with the keyboard",
      { key: isApplePlatform() ? "Option" : "Alt" }
    )
  ].filter(Boolean).join("\n");

  /* On a toggle row the keyboard reaches the row through its button, so the
     reorder keys are read there rather than on a second tab stop. */
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (!sortable || !id || !sort) return;
    const fromRow = event.currentTarget === event.target;
    const fromToggle = Boolean(toggle) && (event.target as HTMLElement).classList.contains("catalog-row__toggle");
    if (!fromRow && !fromToggle) return;
    if (!event.altKey || (event.key !== "ArrowUp" && event.key !== "ArrowDown")) return;
    event.preventDefault();
    sort.moveByKeyboard(id, event.key === "ArrowUp" ? -1 : 1);
  };

  /* The whole row turns a toggle row, as a tool row's whole box does; the
     actions are the one part that is about something else. The button's own
     click — pointer or keyboard — bubbles here too, so this is the one
     handler. */
  const onClick = (event: MouseEvent<HTMLDivElement>) => {
    if (!toggle || toggle.disabled) return;
    if ((event.target as HTMLElement).closest(".catalog-row__actions")) return;
    toggle.onToggle();
  };
  const Sign = on ? Minus : Plus;

  return (
    // biome-ignore lint/a11y/noNoninteractiveElementInteractions: The handlers reorder the row, and on a toggle row forward a click anywhere on it to the toggle button inside — there is no role for "drag handle", and the button is the control assistive technology is given.
    // biome-ignore lint/a11y/noStaticElementInteractions: Same handlers; the keyboard path is Alt + arrow, announced through aria-keyshortcuts, and a toggle row's button is focusable on its own.
    <div
      className={[
        "catalog-row",
        toggle ? "catalog-row--pick" : "",
        on ? "catalog-row--on" : "",
        tone ? `catalog-row--${tone}` : "",
        toggle?.disabled ? "catalog-row--disabled" : "",
        nameEditor ? "catalog-row--editing" : "",
        sortable ? "sortable-surface" : "",
        dragging ? "sortable-surface--dragging" : ""
      ].filter(Boolean).join(" ") + dropTarget}
      data-sortable-id={id}
      title={title || undefined}
      tabIndex={sortable && !toggle ? 0 : undefined}
      aria-keyshortcuts={sortable && !toggle ? "Alt+ArrowUp Alt+ArrowDown" : undefined}
      onKeyDown={onKeyDown}
      onClick={toggle ? onClick : undefined}
      {...(sortable && id && sort ? sort.drag.bind(id) : {})}
    >
      {toggle ? (
        <button
          type="button"
          className="catalog-row__toggle"
          aria-pressed={on}
          aria-label={toggle.label}
          aria-keyshortcuts={sortable ? "Alt+ArrowUp Alt+ArrowDown" : undefined}
          disabled={toggle.disabled}
        >
          {icon}
          <span className="catalog-row__name">{name}</span>
          {badge}
        </button>
      ) : <>
        {lead}
        <span className="catalog-row__main">
          {icon}
          {nameEditor ?? <span className="catalog-row__name">{name}</span>}
          {badge}
        </span>
      </>}
      {actions && <span className="catalog-row__actions">{actions}</span>}
      {toggle && (tone
        ? <LockMark tone={tone} className="catalog-row__sign" />
        : <Sign className="catalog-row__sign" size={14} aria-hidden="true" />)}
    </div>
  );
}

/** The list every catalog page draws its rows into. */
export function CatalogList({ children, sort }: { children: ReactNode; sort?: CatalogSort }) {
  return (
    <div className="catalog-list" data-sortable-list={sort?.listId}>
      {children}
    </div>
  );
}
