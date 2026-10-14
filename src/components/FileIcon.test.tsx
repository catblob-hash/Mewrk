import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import fileIconCss from "./FileIcon.css?raw";
import { FileIcon } from "./FileIcon";

function icons(container: HTMLElement) {
  return [...container.querySelectorAll<HTMLImageElement>("img.file-icon")];
}

describe("FileIcon", () => {
  it("draws the icon of the file's type at the size asked for", () => {
    const { container } = render(<FileIcon path="src-tauri/src/lib.rs" size={13} />);
    const [icon, ...rest] = icons(container);

    expect(rest).toEqual([]);
    expect(icon).toHaveAttribute("src", "/file-icons/rust.svg");
    expect(icon).toHaveAttribute("width", "13");
    expect(icon).toHaveAttribute("height", "13");
  });

  it("is decoration: it says nothing the name beside it does not, and a drag takes the row", () => {
    const { container, queryByRole } = render(<FileIcon path="notes.md" />);
    const [icon] = icons(container);

    expect(queryByRole("img")).toBeNull();
    expect(icon).toHaveAttribute("alt", "");
    expect(icon).toHaveAttribute("aria-hidden", "true");
    expect(icon).toHaveAttribute("draggable", "false");
  });

  it("carries the caller's class and sits in a line of text when asked to", () => {
    const { container } = render(<FileIcon path="a.ts" inline className="row__icon" />);

    expect(icons(container)[0]).toHaveClass("file-icon", "file-icon--inline", "row__icon");
  });

  it("renders both drawings of an icon that has one per ground, and the theme hides one", () => {
    const { container } = render(<FileIcon path="Cargo.toml" className="row__icon" />);
    const [night, day, ...rest] = icons(container);

    expect(rest).toEqual([]);
    expect(night).toHaveAttribute("src", "/file-icons/toml.svg");
    expect(night).toHaveClass("file-icon--night", "row__icon");
    expect(day).toHaveAttribute("src", "/file-icons/toml_light.svg");
    expect(day).toHaveClass("file-icon--day", "row__icon");
    // The stylesheet is what picks between them: day's drawing is gone under the night
    // theme, night's under anything else.
    expect(fileIconCss.replace(/\s+/g, " ")).toContain(
      ':root[data-theme="night"] .file-icon--day, :root:not([data-theme="night"]) .file-icon--night { display: none; }'
    );
  });
});
