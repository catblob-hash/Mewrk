import { PUBLISHED_CODE_THEMES } from "./codeThemePalettes";

/**
 * Code themes (Appearance → Code appearance): the colours code and diffs are drawn in.
 *
 * One is chosen for each of the app's two themes, from the themes of that scheme, and
 * `applyAppearance` puts both on the root as `--code-day-*` and `--code-night-*`. The
 * stylesheet reads whichever set the theme on screen names (`styles/base.css`), so a
 * change of theme needs no second write. Every code surface — a fenced block, the file
 * viewer, a diff — is drawn from the resulting `--code-*` variables and nothing else.
 *
 * The colours live in TypeScript because the palette guard scans CSS files, and because
 * they are data: a published theme is its author's palette, not a step of the app's own.
 */

export type CodeThemeScheme = "day" | "night";

export interface CodeTheme {
  id: string;
  /** A proper name, shown as written in every language. */
  name: string;
  scheme: CodeThemeScheme;
  /**
   * The Mewrk themes: code keeps the app's own surface and text colour, so it sits in
   * the window's well, and in glass under liquid glass (`styles/backdrop.css`). The three
   * surface colours below are then what that surface is in the plain window, for drawing
   * the theme where the app's own tokens are the other scheme's (the settings preview).
   */
  adaptive?: true;
  background: string;
  foreground: string;
  lineNumber: string;
  comment: string;
  commentItalic: boolean;
  string: string;
  number: string;
  keyword: string;
  function: string;
  type: string;
  /** Decorators, preprocessor directives, a diff's file headers. */
  meta: string;
  tag: string;
  attribute: string;
  /** A key in data or configuration. */
  property: string;
  /** A sigilled variable: `$HOME`, `@name`. */
  variable: string;
  /** The hues an added and a removed line are marked and tinted with. */
  added: string;
  removed: string;
}

/**
 * The app's own themes. Their hues are the palette's syntax colours tuned per scheme by
 * hand rather than mirrored: a violet at mid lightness is its own mirror image, and reads
 * well on the day well and poorly on the night one.
 */
const MEWRK_CODE_THEMES: readonly CodeTheme[] = [
  {
    id: "mewrk-light", name: "Mewrk Light", scheme: "day", adaptive: true,
    background: "#f1f1ee", foreground: "#343431", lineNumber: "#989892",
    comment: "#73736d", commentItalic: true,
    string: "#246b4e", number: "#a2531d", keyword: "#6254aa", function: "#795d2e", type: "#1f7389",
    meta: "#aa4f6a", tag: "#8e3630", attribute: "#96580f", property: "#315eae", variable: "#315eae",
    added: "#287a38", removed: "#a23737"
  },
  {
    id: "mewrk-dark", name: "Mewrk Dark", scheme: "night", adaptive: true,
    background: "#11110e", foreground: "#cececb", lineNumber: "#6d6d67",
    comment: "#8a8a83", commentItalic: true,
    string: "#8fd0ae", number: "#f0a070", keyword: "#a99cf0", function: "#dcc48e", type: "#6cc4d8",
    meta: "#e58fb0", tag: "#ee8a82", attribute: "#e8b05c", property: "#86aef0", variable: "#86aef0",
    added: "#6fcf82", removed: "#f08a8a"
  }
];

/** Every theme, the app's own first, then the published ones as their authors named them. */
export const CODE_THEMES: readonly CodeTheme[] = [...MEWRK_CODE_THEMES, ...PUBLISHED_CODE_THEMES];

export const DEFAULT_CODE_THEME: Readonly<Record<CodeThemeScheme, string>> = {
  day: "mewrk-light",
  night: "mewrk-dark"
};

export function codeThemesFor(scheme: CodeThemeScheme): CodeTheme[] {
  return CODE_THEMES.filter((theme) => theme.scheme === scheme);
}

/** The theme saved as `id` for `scheme`; an unknown id, or one of the other scheme, is the default. */
export function codeThemeFor(id: unknown, scheme: CodeThemeScheme): CodeTheme {
  const theme = CODE_THEMES.find((candidate) => candidate.id === id && candidate.scheme === scheme)
    ?? CODE_THEMES.find((candidate) => candidate.id === DEFAULT_CODE_THEME[scheme]);
  if (!theme) throw new Error(`No default code theme for ${scheme}`);
  return theme;
}

export function normalizeCodeTheme(id: unknown, scheme: CodeThemeScheme): string {
  return codeThemeFor(id, scheme).id;
}

/** The token kinds a theme colours, each under `--code-<kind>` (`CodeBlock.css`). */
const TOKEN_KINDS = [
  "comment", "string", "number", "keyword", "function", "type", "meta", "tag", "attribute", "property", "variable"
] as const;

/**
 * How much of its hue a changed line is tinted with, and how much the characters that
 * changed within it. A dark ground swallows a tint a light one shows.
 */
const DIFF_TINT: Readonly<Record<CodeThemeScheme, { line: number; word: number }>> = {
  day: { line: 0.13, word: 0.28 },
  night: { line: 0.17, word: 0.36 }
};
/** A themed block's edge: its own text colour, thinned. */
const BORDER_ALPHA = 0.14;
/** A code well under liquid glass is the text colour at this alpha (`--subtle`, backdrop.css). */
const GLASS_WELL_ALPHA = 0.04;
const GLASS_TEXT: Readonly<Record<CodeThemeScheme, string>> = { day: "#1b1b1a", night: "#e5e5e4" };

/** `#rrggbb` at `alpha`, as the eight-digit hex CSS takes anywhere a colour goes. */
function withAlpha(color: string, alpha: number): string {
  return `${color}${Math.round(alpha * 255).toString(16).padStart(2, "0")}`;
}

/** Every variable a theme may set, without the `--code-` prefix, in a fixed order. */
export const CODE_THEME_VARIABLES: readonly string[] = [
  "bg", "fg", "border", "line-number",
  "comment-style", ...TOKEN_KINDS,
  "added", "added-line", "added-word", "removed", "removed-line", "removed-word"
];

/**
 * A theme as `--code-*` values, keyed without the prefix.
 *
 * `surface` says where the block's ground and text colour come from: `"theme"` is the
 * theme's own; `"app"` leaves them unset, so the app's surfaces show (what an adaptive
 * theme does in the window); `"glass"` is an adaptive theme's well as liquid glass draws
 * it, for a preview that cannot borrow the window's.
 */
export function codeThemeVariables(
  theme: CodeTheme,
  surface: "theme" | "app" | "glass" = theme.adaptive ? "app" : "theme"
): Record<string, string> {
  const tint = DIFF_TINT[theme.scheme];
  const variables: Record<string, string> = {
    "comment-style": theme.commentItalic ? "italic" : "normal",
    added: theme.added,
    "added-line": withAlpha(theme.added, tint.line),
    "added-word": withAlpha(theme.added, tint.word),
    removed: theme.removed,
    "removed-line": withAlpha(theme.removed, tint.line),
    "removed-word": withAlpha(theme.removed, tint.word)
  };
  for (const kind of TOKEN_KINDS) variables[kind] = theme[kind];
  if (surface !== "app") {
    const glass = surface === "glass" && theme.adaptive;
    variables.bg = glass ? withAlpha(GLASS_TEXT[theme.scheme], GLASS_WELL_ALPHA) : theme.background;
    variables.fg = theme.foreground;
    variables.border = withAlpha(glass ? GLASS_TEXT[theme.scheme] : theme.foreground, glass ? 0.11 : BORDER_ALPHA);
    variables["line-number"] = theme.lineNumber;
  }
  return variables;
}
