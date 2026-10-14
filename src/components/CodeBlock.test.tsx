import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { NumberedCode } from "./CodeBlock";

const content = "alpha\nbeta\ngamma\n";

describe("NumberedCode", () => {
  it("answers a right click on a line with that line's number and where the pointer was", () => {
    const onLineMenu = vi.fn();
    render(<NumberedCode content={content} language={null} label="notes.txt" onLineMenu={onLineMenu} />);

    const notPrevented = fireEvent.contextMenu(screen.getByText("beta"), { clientX: 40, clientY: 60 });

    expect(onLineMenu).toHaveBeenCalledTimes(1);
    expect(onLineMenu).toHaveBeenCalledWith(2, { x: 40, y: 60 });
    expect(notPrevented).toBe(false);
  });

  it("ignores a right click that lands on the block rather than a line", () => {
    const onLineMenu = vi.fn();
    render(<NumberedCode content={content} language={null} label="notes.txt" onLineMenu={onLineMenu} />);

    const notPrevented = fireEvent.contextMenu(screen.getByLabelText("notes.txt"), { clientX: 1, clientY: 2 });

    expect(onLineMenu).not.toHaveBeenCalled();
    expect(notPrevented).toBe(true);
  });

  it("leaves a right click to the browser when no line menu is given", () => {
    render(<NumberedCode content={content} language={null} label="notes.txt" />);

    const notPrevented = fireEvent.contextMenu(screen.getByText("beta"), { clientX: 40, clientY: 60 });

    expect(notPrevented).toBe(true);
  });
});
