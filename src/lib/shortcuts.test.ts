import { describe, expect, it, onTestFinished, vi } from "vitest";
import {
  isImeKeyEvent,
  isShortcutModified,
  keyTokenLabel,
  matchesEvent,
  resolveShortcut,
  SHORTCUT_COMMANDS,
  shortcutKeyCaps
} from "./shortcuts";

const keydown = (init: KeyboardEventInit) => new KeyboardEvent("keydown", init);

describe("isImeKeyEvent", () => {
  it("claims the WebKit keydown that commits a composition after compositionend", () => {
    // macOS WKWebView: Enter confirming Pinyin letters arrives with isComposing false.
    expect(isImeKeyEvent(keydown({ key: "Enter", code: "Enter", keyCode: 229 }))).toBe(true);
    expect(isImeKeyEvent(keydown({ key: "Enter", code: "Enter", keyCode: 229, shiftKey: true }))).toBe(true);
  });

  it("claims Chromium composition keystrokes", () => {
    expect(isImeKeyEvent(keydown({ key: "Enter", code: "Enter", isComposing: true }))).toBe(true);
    expect(isImeKeyEvent(keydown({ key: "Process", code: "KeyN" }))).toBe(true);
  });

  it("leaves ordinary Enter and Shift+Enter to the page", () => {
    expect(isImeKeyEvent(keydown({ key: "Enter", code: "Enter", keyCode: 13 }))).toBe(false);
    expect(isImeKeyEvent(keydown({ key: "Enter", code: "Enter", keyCode: 13, shiftKey: true }))).toBe(false);
  });
});

describe("matchesEvent", () => {
  it("keeps the default send and newline bindings apart", () => {
    const enter = keydown({ key: "Enter", code: "Enter" });
    const shiftEnter = keydown({ key: "Enter", code: "Enter", shiftKey: true });
    expect(matchesEvent(["Enter"], enter)).toBe(true);
    expect(matchesEvent(["Enter"], shiftEnter)).toBe(false);
    expect(matchesEvent(["Shift", "Enter"], shiftEnter)).toBe(true);
    expect(matchesEvent(["Shift", "Enter"], enter)).toBe(false);
  });
});

describe("platform defaults", () => {
  const command = (id: string) => SHORTCUT_COMMANDS.find((candidate) => candidate.id === id)!;
  const onPlatform = (platform: string) => {
    const spy = vi.spyOn(window.navigator, "platform", "get").mockReturnValue(platform);
    onTestFinished(() => spy.mockRestore());
  };

  it("uses ⌘ where Windows uses Ctrl on a Mac, and shows Mac keycaps", () => {
    onPlatform("MacIntel");
    expect(resolveShortcut({}, command("conversation.create")).binding).toEqual(["Meta", "KeyN"]);
    expect(resolveShortcut({}, command("app.settings.open")).binding).toEqual(["Meta", "Comma"]);
    expect(resolveShortcut({}, command("app.conversation_settings.open")).binding).toEqual(["Shift", "Meta", "Comma"]);
    expect(resolveShortcut({}, command("panel.browser.toggle")).binding).toEqual(["Meta", "KeyB"]);
    expect(resolveShortcut({}, command("app.zoom.in")).binding).toEqual(["Meta", "Equal"]);
    // Switching conversations stays on Control, as switching tabs does.
    expect(resolveShortcut({}, command("conversation.next")).binding).toEqual(["Control", "Tab"]);
    expect(shortcutKeyCaps(["Shift", "Meta", "Comma"])).toEqual(["⇧", "⌘", ","]);
    expect(shortcutKeyCaps(["Control", "Shift", "Tab"])).toEqual(["⌃", "⇧", "Tab"]);
    expect(keyTokenLabel("Alt")).toBe("⌥");
    expect(keyTokenLabel("Enter")).toBe("↩");
    // A recorded ⌘N is the default, not an override.
    expect(isShortcutModified(
      { "conversation.create": { binding: ["Meta", "KeyN"], enabled: true } },
      command("conversation.create")
    )).toBe(false);
    expect(matchesEvent(
      resolveShortcut({}, command("conversation.create")).binding,
      keydown({ key: "n", code: "KeyN", metaKey: true })
    )).toBe(true);
    expect(matchesEvent(
      resolveShortcut({}, command("conversation.create")).binding,
      keydown({ key: "n", code: "KeyN", ctrlKey: true })
    )).toBe(false);
  });

  it("keeps Ctrl and the Windows key names elsewhere", () => {
    onPlatform("Win32");
    expect(resolveShortcut({}, command("conversation.create")).binding).toEqual(["Control", "KeyN"]);
    expect(shortcutKeyCaps(["Control", "Shift", "Comma"])).toEqual(["Ctrl", "Shift", ","]);
    expect(keyTokenLabel("Meta")).toBe("Win");
  });
});
