import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ContextMenu } from "./ContextMenu";
import type { ContextMenuSection } from "./ContextMenu";

describe("ContextMenu", () => {
  afterEach(() => vi.restoreAllMocks());

  it("opens a submenu as a panel of its own beside the menu, read from the moment its row is reached", async () => {
    let answer: (sections: ContextMenuSection[]) => void = () => {};
    const loadChildren = vi.fn(() => new Promise<ContextMenuSection[]>((resolve) => { answer = resolve; }));
    const chosen = vi.fn();
    const onClose = vi.fn();
    render(
      <ContextMenu
        anchor={{ x: 10, y: 10 }}
        sections={[{ id: "file", items: [{ id: "open-with", label: "Open With", loadChildren }] }]}
        label="File actions"
        onClose={onClose}
      />
    );
    const menu = screen.getByRole("menu", { name: "File actions" });
    const row = within(menu).getByRole("menuitem", { name: "Open With" });

    fireEvent.mouseEnter(row);
    expect(loadChildren).toHaveBeenCalledTimes(1);
    fireEvent.click(row);
    await act(async () => { answer([{ id: "apps", items: [{ id: "editor", label: "Editor", onSelect: chosen }] }]); });

    const submenu = screen.getByRole("menu", { name: "Open With" });
    const panel = menu.closest(".context-menu");
    expect(panel).not.toContainElement(submenu);
    expect(submenu.closest(".popover-menu__flyout")?.parentElement).toBe(panel?.parentElement);
    expect(row).toHaveAttribute("aria-expanded", "true");

    // A press in the submenu is a press in the menu, not outside it.
    const editor = within(submenu).getByRole("menuitem", { name: "Editor" });
    fireEvent.mouseDown(editor);
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.click(editor);
    expect(chosen).toHaveBeenCalledTimes(1);
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(loadChildren).toHaveBeenCalledTimes(1);
  });

  it("takes the keyboard once it is placed, and gives focus back when it closes", () => {
    // What a browser does and jsdom does not: nothing inside a hidden box takes focus.
    const focus = HTMLElement.prototype.focus;
    vi.spyOn(HTMLElement.prototype, "focus").mockImplementation(function (this: HTMLElement, options?: FocusOptions) {
      for (let box: HTMLElement | null = this; box; box = box.parentElement) {
        if (box.style.visibility === "hidden") return;
      }
      focus.call(this, options);
    });
    const opener = document.createElement("button");
    document.body.append(opener);
    opener.focus();
    const onClose = vi.fn();
    const { unmount } = render(
      <ContextMenu
        anchor={{ x: 10, y: 10 }}
        sections={[{ id: "file", items: [{ id: "open", label: "Open" }, { id: "rename", label: "Rename" }] }]}
        label="File actions"
        onClose={onClose}
      />
    );
    const menu = screen.getByRole("menu", { name: "File actions" });
    const open = within(menu).getByRole("menuitem", { name: "Open" });

    expect(open).toHaveFocus();
    fireEvent.keyDown(open, { key: "ArrowDown" });
    expect(within(menu).getByRole("menuitem", { name: "Rename" })).toHaveFocus();
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);

    unmount();
    expect(opener).toHaveFocus();
    opener.remove();
  });

  it("leaves focus where the chosen row put it", () => {
    function Files() {
      const [open, setOpen] = useState(true);
      const [renaming, setRenaming] = useState(false);
      return (
        <>
          {renaming && <input aria-label="New name" ref={(field) => field?.focus()} />}
          {open && (
            <ContextMenu
              anchor={{ x: 10, y: 10 }}
              sections={[{ id: "file", items: [{ id: "rename", label: "Rename", onSelect: () => setRenaming(true) }] }]}
              label="File actions"
              onClose={() => setOpen(false)}
            />
          )}
        </>
      );
    }
    const opener = document.createElement("button");
    document.body.append(opener);
    opener.focus();
    render(<Files />);

    fireEvent.click(screen.getByRole("menuitem", { name: "Rename" }));

    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: "New name" })).toHaveFocus();
    opener.remove();
  });
});
