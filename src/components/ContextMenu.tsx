import { ChevronRight, LoaderCircle } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent, ReactNode } from "react";
import { createPortal } from "react-dom";
import { useI18n } from "../i18n";
import { useFloatingSurface } from "../lib/floatingSurfaces";
import { placeMenu } from "../lib/menuPlacement";
import type { MenuPlacement } from "../lib/menuPlacement";
import { MenuFlyout, MenuSurfacesContext, createMenuSurfaces } from "./MenuFlyout";
import { PathText } from "./PathText";
import "./ContextMenu.css";

/** A row of a context menu. */
export interface ContextMenuItem {
  id: string;
  label: string;
  /** The label is a path, drawn so that it gives way in the middle. */
  labelIsPath?: boolean;
  icon?: ReactNode;
  /** Secondary text under the label, such as the path a workspace is at. */
  description?: string;
  /** The description is a path, drawn so that it gives way in the middle. */
  descriptionIsPath?: boolean;
  /** Subtle trailing text. */
  hint?: string;
  disabled?: boolean;
  title?: string;
  /** Drawn as a destructive action. */
  danger?: boolean;
  onSelect?: () => void;
  /** A submenu, opened beside the row on hover or with the right arrow key. */
  children?: ContextMenuSection[];
  /** A submenu read the first time it opens — the programs that open a file, say. */
  loadChildren?: () => Promise<ContextMenuSection[]>;
}

export interface ContextMenuSection {
  id: string;
  label?: string;
  items: ContextMenuItem[];
}

/** Where the menu opens: at a point (a right click), or against a box (the control that opened it). */
export type ContextMenuAnchor =
  | { x: number; y: number }
  | { rect: { left: number; top: number; right: number; bottom: number }; align?: "start" | "end" };

export interface ContextMenuProps {
  anchor: ContextMenuAnchor;
  sections: ContextMenuSection[];
  /** The menu's accessible name. */
  label: string;
  onClose: () => void;
  /** Shown while `sections` is empty, such as a menu still waiting on what to list. */
  placeholder?: ReactNode;
}

const ANCHOR_GAP = 4;
/** How long the pointer rests on a row before its submenu opens or another one closes. */
const HOVER_DELAY_MS = 120;

/**
 * A menu opened by a right click or by a row's `⋮`, for acting on one thing.
 *
 * It shares `PopoverMenu`'s look, but not its trigger: the thing it acts on is
 * a row or a link, which has no button to anchor to. A row with a submenu opens
 * it beside itself when the pointer rests on it — the way a desktop's own
 * context menus behave — and a submenu can be read when it first opens, so a
 * menu that offers the programs that open a file only asks the system once
 * someone wants to know.
 *
 * The panel is portaled to `document.body` like every floating surface, and
 * published as one, so the built-in browser's native page never paints over it.
 */
export function ContextMenu({ anchor, sections, label, onClose, placeholder }: ContextMenuProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState<MenuPlacement | null>(null);
  const [surfaces] = useState(createMenuSurfaces);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useLayoutEffect(() => {
    const panel = panelRef.current;
    if (!panel) return;
    const box = panel.getBoundingClientRect();
    const next = placeMenu("rect" in anchor ? { ...anchor, gap: ANCHOR_GAP } : anchor, box.width, box.height);
    setPosition((current) => (
      current && current.left === next.left && current.top === next.top && current.above === next.above ? current : next
    ));
  });

  useFloatingSurface(panelRef, position !== null);

  useEffect(() => {
    const close = () => onCloseRef.current();
    // The menu's submenus are panels of their own on the page, not inside the menu's.
    const inside = (target: EventTarget | null) => target instanceof Node
      && (panelRef.current?.contains(target) || surfaces.contains(target));
    const onPointerDown = (event: MouseEvent) => {
      if (inside(event.target)) return;
      close();
    };
    const onScroll = (event: Event) => {
      if (inside(event.target)) return;
      close();
    };
    document.addEventListener("mousedown", onPointerDown, true);
    window.addEventListener("resize", close);
    window.addEventListener("blur", close);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      document.removeEventListener("mousedown", onPointerDown, true);
      window.removeEventListener("resize", close);
      window.removeEventListener("blur", close);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [surfaces]);

  // Whatever had focus gets it back: the menu was a detour from it.
  useEffect(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    return () => {
      if (previous?.isConnected) previous.focus({ preventScroll: true });
    };
  }, []);

  return createPortal(
    <div
      ref={panelRef}
      className="popover-menu__panel popover-menu__panel--dense popover-menu__panel--flyout context-menu"
      style={{
        left: position?.left ?? 0,
        top: position?.top ?? 0,
        visibility: position ? "visible" : "hidden"
      }}
    >
      {/* The list is the menu; one still waiting on what to list is a menu saying so. */}
      {sections.length === 0 && placeholder
        ? <div className="context-menu__placeholder" role="menu" aria-label={label} aria-busy="true">{placeholder}</div>
        : (
          <MenuSurfacesContext.Provider value={surfaces}>
            <MenuLevel
              label={label}
              sections={sections}
              autoFocus
              hang={position?.above ? "up" : "down"}
              onCloseLevel={() => onCloseRef.current()}
              onCloseAll={() => onCloseRef.current()}
            />
          </MenuSurfacesContext.Provider>
        )}
    </div>,
    document.body
  );
}

interface MenuLevelProps {
  /** The level's accessible name: the menu's, or the row that opened it. */
  label: string;
  sections: ContextMenuSection[];
  /** Focus the first row on mount: the level was opened from the keyboard, or is the menu itself. */
  autoFocus: boolean;
  /** Which way its submenus grow from their rows: up for a menu standing on the spot it was opened at. */
  hang: "down" | "up";
  /** Escape or the left arrow inside this level. */
  onCloseLevel: () => void;
  /** A row was chosen. */
  onCloseAll: () => void;
}

function MenuLevel({ label, sections, autoFocus, hang, onCloseLevel, onCloseAll }: MenuLevelProps) {
  const { t } = useI18n();
  const listRef = useRef<HTMLDivElement>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  /** The submenu was opened from the keyboard, so it takes focus. */
  const [openedByKey, setOpenedByKey] = useState(false);
  const [loaded, setLoaded] = useState<ReadonlyMap<string, ContextMenuSection[] | Error>>(() => new Map());
  const loading = useRef(new Set<string>());
  const hoverTimer = useRef<number | null>(null);

  const items = sections.flatMap((section) => section.items);

  useEffect(() => {
    if (!autoFocus) return;
    listRef.current?.querySelector<HTMLButtonElement>(":scope > .popover-menu__section > .popover-menu__row > button:not(:disabled)")?.focus();
  }, [autoFocus]);

  useEffect(() => () => {
    if (hoverTimer.current !== null) window.clearTimeout(hoverTimer.current);
  }, []);

  const load = useCallback((item: ContextMenuItem) => {
    if (!item.loadChildren || loading.current.has(item.id)) return;
    loading.current.add(item.id);
    item.loadChildren().then((children) => {
      setLoaded((current) => new Map(current).set(item.id, children));
    }).catch((error: unknown) => {
      setLoaded((current) => new Map(current).set(item.id, error instanceof Error ? error : new Error(String(error))));
    });
  }, []);

  const open = useCallback((item: ContextMenuItem | null, byKey: boolean) => {
    if (hoverTimer.current !== null) {
      window.clearTimeout(hoverTimer.current);
      hoverTimer.current = null;
    }
    setOpenedByKey(byKey);
    setOpenId(item?.id ?? null);
    if (item) load(item);
  }, [load]);

  const hover = (item: ContextMenuItem) => {
    const branch = Boolean(item.children || item.loadChildren) && !item.disabled;
    // A submenu read on opening starts reading as the pointer reaches its row, so the wait
    // before it opens covers some of the reading.
    if (branch) load(item);
    if ((branch ? item.id : null) === openId) {
      if (hoverTimer.current !== null) window.clearTimeout(hoverTimer.current);
      hoverTimer.current = null;
      return;
    }
    if (hoverTimer.current !== null) window.clearTimeout(hoverTimer.current);
    hoverTimer.current = window.setTimeout(() => {
      hoverTimer.current = null;
      open(branch ? item : null, false);
    }, HOVER_DELAY_MS);
  };

  const rowButtons = () => Array.from(
    listRef.current?.querySelectorAll<HTMLButtonElement>(":scope > .popover-menu__section > .popover-menu__row > button:not(:disabled)") ?? []
  );

  const onKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    // A nested level handles its own keys and stops them; what reaches here is this level's.
    const buttons = rowButtons();
    const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const focusedItem = index >= 0 ? items.find((item) => item.id === buttons[index]!.dataset.itemId) : undefined;
    switch (event.key) {
      case "ArrowDown":
        buttons[(index + 1 + buttons.length) % buttons.length]?.focus();
        break;
      case "ArrowUp":
        buttons[(index - 1 + buttons.length) % buttons.length]?.focus();
        break;
      case "Home":
        buttons[0]?.focus();
        break;
      case "End":
        buttons[buttons.length - 1]?.focus();
        break;
      case "ArrowRight":
        if (!focusedItem || !(focusedItem.children || focusedItem.loadChildren)) return;
        open(focusedItem, true);
        break;
      case "ArrowLeft":
      case "Escape":
        onCloseLevel();
        break;
      case "Tab":
        onCloseAll();
        break;
      default:
        return;
    }
    event.preventDefault();
    event.stopPropagation();
  };

  return (
    <div ref={listRef} className="popover-menu__list" role="menu" aria-label={label} onKeyDown={onKeyDown}>
      {sections.filter((section) => section.items.length > 0).map((section, sectionIndex) => (
        <div className="popover-menu__section" key={section.id}>
          {sectionIndex > 0 && <div className="popover-menu__divider" />}
          {section.label && <div className="popover-menu__label">{section.label}</div>}
          {section.items.map((item) => {
            const branch = Boolean(item.children || item.loadChildren);
            const expanded = branch && openId === item.id;
            const children = item.children ?? (() => {
              const answer = loaded.get(item.id);
              return answer instanceof Error || answer === undefined ? null : answer;
            })();
            const failure = loaded.get(item.id);
            return (
              <div
                key={item.id}
                className={`popover-menu__row${branch ? " popover-menu__row--branch" : ""}`}
              >
                {/* Hover is the row's button's, not the row's: the pointer travelling from
                    the button into its own submenu must not count as leaving for another row. */}
                <button
                  type="button"
                  role="menuitem"
                  onMouseEnter={() => hover(item)}
                  data-item-id={item.id}
                  aria-haspopup={branch ? "menu" : undefined}
                  aria-expanded={branch ? expanded : undefined}
                  className={`popover-menu__item${item.danger ? " context-menu__item--danger" : ""}`}
                  disabled={item.disabled}
                  title={item.title}
                  onClick={() => {
                    if (branch) {
                      open(expanded ? null : item, false);
                      return;
                    }
                    onCloseAll();
                    item.onSelect?.();
                  }}
                >
                  {item.icon && <span className="popover-menu__icon">{item.icon}</span>}
                  <span className="popover-menu__copy">
                    <strong>{item.labelIsPath ? <PathText path={item.label} /> : item.label}</strong>
                    {item.description && (
                      <small>{item.descriptionIsPath ? <PathText path={item.description} /> : item.description}</small>
                    )}
                  </span>
                  {item.hint && <span className="popover-menu__hint">{item.hint}</span>}
                  {branch && <ChevronRight size={13} className="popover-menu__chevron" aria-hidden="true" />}
                </button>
                {expanded && (
                  <MenuFlyout
                    className="popover-menu__panel popover-menu__panel--dense popover-menu__panel--flyout popover-menu__flyout"
                    hang={hang}
                  >
                    {children
                      ? (
                        children.some((group) => group.items.length > 0)
                          ? (
                            <MenuLevel
                              label={item.label}
                              sections={children}
                              autoFocus={openedByKey}
                              hang={hang}
                              onCloseLevel={() => {
                                open(null, false);
                                rowButtons().find((button) => button.dataset.itemId === item.id)?.focus();
                              }}
                              onCloseAll={onCloseAll}
                            />
                          )
                          : <p className="popover-menu__empty">{t("没有可用的程序", "Nothing to offer")}</p>
                      )
                      : failure instanceof Error
                        ? <p className="popover-menu__empty">{failure.message}</p>
                        : (
                          <p className="popover-menu__empty context-menu__loading">
                            <LoaderCircle size={12} className="spin" aria-hidden="true" />
                            {t("正在读取…", "Loading…")}
                          </p>
                        )}
                  </MenuFlyout>
                )}
              </div>
            );
          })}
        </div>
      ))}
    </div>
  );
}
