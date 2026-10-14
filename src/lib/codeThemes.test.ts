import { describe, expect, it } from "vitest";
import baseCss from "../styles/base.css?raw";
import {
  CODE_THEMES,
  CODE_THEME_VARIABLES,
  codeThemeFor,
  codeThemesFor,
  codeThemeVariables,
  DEFAULT_CODE_THEME,
  normalizeCodeTheme
} from "./codeThemes";

const SCHEMES = ["day", "night"] as const;

const COLOUR_FIELDS = [
  "background", "foreground", "lineNumber", "comment", "string", "number", "keyword", "function", "type",
  "meta", "tag", "attribute", "property", "variable", "added", "removed"
] as const;

const TOKEN_KINDS = [
  "comment", "string", "number", "keyword", "function", "type", "meta", "tag", "attribute", "property", "variable"
] as const;

describe("code themes", () => {
  it("gives every theme its own id", () => {
    const ids = CODE_THEMES.map((theme) => theme.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("gives every theme a day or night scheme and six-digit lowercase colours", () => {
    for (const theme of CODE_THEMES) {
      expect(["day", "night"], theme.id).toContain(theme.scheme);
      for (const field of COLOUR_FIELDS) {
        expect(theme[field], `${theme.id} ${field}`).toMatch(/^#[0-9a-f]{6}$/);
      }
    }
  });

  it("makes the app's own adaptive theme the default of each scheme", () => {
    expect(DEFAULT_CODE_THEME).toEqual({ day: "mewrk-light", night: "mewrk-dark" });
    for (const scheme of SCHEMES) {
      const theme = CODE_THEMES.find((candidate) => candidate.id === DEFAULT_CODE_THEME[scheme]);
      expect(theme, scheme).toMatchObject({ scheme, adaptive: true });
    }
  });

  it("lists the day themes for day and the night themes for night, the app's own first", () => {
    const day = codeThemesFor("day");
    const night = codeThemesFor("night");

    expect(day.every((theme) => theme.scheme === "day")).toBe(true);
    expect(day[0].id).toBe("mewrk-light");
    expect(night.every((theme) => theme.scheme === "night")).toBe(true);
    expect(night[0].id).toBe("mewrk-dark");
    expect(day.length + night.length).toBe(CODE_THEMES.length);
  });

  it("resolves an unknown, non-string, or other-scheme id to the scheme's default", () => {
    expect(codeThemeFor("no-such-theme", "day").id).toBe("mewrk-light");
    expect(codeThemeFor(42, "night").id).toBe("mewrk-dark");
    expect(codeThemeFor(undefined, "day").id).toBe("mewrk-light");
    expect(codeThemeFor(null, "night").id).toBe("mewrk-dark");
    expect(codeThemeFor({ id: "dracula" }, "day").id).toBe("mewrk-light");

    expect(codeThemeFor("dracula", "day").id).toBe("mewrk-light");
    expect(codeThemeFor("github-light", "night").id).toBe("mewrk-dark");
    expect(normalizeCodeTheme("dracula", "day")).toBe("mewrk-light");
    expect(normalizeCodeTheme("github-light", "night")).toBe("mewrk-dark");
  });

  it("keeps a valid id of the scheme as itself", () => {
    expect(normalizeCodeTheme("github-light", "day")).toBe("github-light");
    expect(normalizeCodeTheme("dracula", "night")).toBe("dracula");
    expect(normalizeCodeTheme("mewrk-dark", "night")).toBe("mewrk-dark");
    expect(codeThemeFor("dracula", "night").id).toBe("dracula");
  });
});

describe("codeThemeVariables", () => {
  it("leaves an adaptive theme's surface to the app by default, with every token and tint", () => {
    const theme = codeThemeFor("mewrk-light", "day");
    const variables = codeThemeVariables(theme);

    expect(Object.keys(variables).sort()).toEqual([
      "added", "added-line", "added-word", "attribute", "comment", "comment-style", "function", "keyword",
      "meta", "number", "property", "removed", "removed-line", "removed-word", "string", "tag", "type", "variable"
    ]);
    expect(variables["comment-style"]).toBe("italic");
    for (const kind of TOKEN_KINDS) {
      expect(variables[kind], kind).toBe(theme[kind]);
    }
    expect(variables.added).toBe(theme.added);
    expect(variables.removed).toBe(theme.removed);
  });

  it("sets the theme's own ground, text, and line numbers when asked for the theme's surface", () => {
    const theme = codeThemeFor("mewrk-dark", "night");
    const variables = codeThemeVariables(theme, "theme");

    expect(variables.bg).toBe(theme.background);
    expect(variables.fg).toBe(theme.foreground);
    expect(variables["line-number"]).toBe(theme.lineNumber);
  });

  it("draws an adaptive theme's glass as its text colour at low alpha, not as its ground", () => {
    for (const theme of [codeThemeFor("mewrk-light", "day"), codeThemeFor("mewrk-dark", "night")]) {
      const glass = codeThemeVariables(theme, "glass").bg;

      expect(glass, theme.id).toMatch(/^#[0-9a-f]{8}$/);
      expect(glass, theme.id).not.toBe(theme.background);
    }
  });

  it("keeps a non-adaptive theme's own opaque ground, even when glass is asked for", () => {
    const dracula = codeThemeFor("dracula", "night");

    expect(codeThemeVariables(dracula)).toMatchObject({
      bg: "#282a36",
      fg: "#f8f8f2",
      "line-number": "#6272a4"
    });
    expect(codeThemeVariables(dracula, "glass").bg).toBe("#282a36");
  });

  it("tints each changed line and its changed words in the theme's hue, the words more strongly", () => {
    for (const theme of CODE_THEMES) {
      const variables = codeThemeVariables(theme);
      for (const side of ["added", "removed"] as const) {
        const line = variables[`${side}-line`];
        const word = variables[`${side}-word`];

        expect(line, `${theme.id} ${side}-line`).toMatch(new RegExp(`^${theme[side]}[0-9a-f]{2}$`));
        expect(word, `${theme.id} ${side}-word`).toMatch(new RegExp(`^${theme[side]}[0-9a-f]{2}$`));
        expect(parseInt(word.slice(-2), 16), theme.id).toBeGreaterThan(parseInt(line.slice(-2), 16));
      }
    }
  });

  it("names in CODE_THEME_VARIABLES every variable any theme can set", () => {
    const listed = new Set(CODE_THEME_VARIABLES);
    const unlisted: string[] = [];
    for (const theme of CODE_THEMES) {
      for (const surface of [undefined, "app", "theme", "glass"] as const) {
        for (const name of Object.keys(codeThemeVariables(theme, surface))) {
          if (!listed.has(name)) unlisted.push(`${theme.id}: ${name}`);
        }
      }
    }
    expect(unlisted).toEqual([]);
  });

  it("has base.css map each variable to the day set and to the night set of the same name", () => {
    for (const name of CODE_THEME_VARIABLES) {
      expect(baseCss, `--code-${name} from day`)
        .toMatch(new RegExp(`--code-${name}:\\s*var\\(--code-day-${name}\\);`));
      expect(baseCss, `--code-${name} from night`)
        .toMatch(new RegExp(`--code-${name}:\\s*var\\(--code-night-${name}\\);`));
    }
  });
});
